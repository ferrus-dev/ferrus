use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::warn;

pub use crate::runtime_status::TaskStatus;

use crate::{
    agent_id::ENV_PROJECT_ROOT,
    legacy_state::{self, LegacyTaskState},
    platform,
    repository_graph::domain::{
        BuildId, Digest, OverlayRevisionId, PublishedViewName, SnapshotId, SourceRevisionId,
        TaskViewLifecycle,
    },
};

const PROJECT_VERSION: u32 = 1;
const RUNTIME_SCHEMA_VERSION: u32 = 4;
const LOCAL_PROJECT_TOML: &str = ".ferrus/project.toml";
const CURRENT_TASK_ID: &str = "current";
const CURRENT_TASK_PATH: &str = ".ferrus/TASK.md";
const BASELINE_WORKTREE_METADATA_DIR: &str = ".baseline-trees";
const BASELINE_REF_PREFIX: &str = "refs/ferrus/baselines";
static RUN_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RepositoryViewStatus {
    #[default]
    NotBuilt,
    Available,
    Stale,
    Unavailable,
    Failed,
}

impl RepositoryViewStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotBuilt => "not_built",
            Self::Available => "available",
            Self::Stale => "stale",
            Self::Unavailable => "unavailable",
            Self::Failed => "failed",
        }
    }

    fn from_database(value: &str) -> Result<Self> {
        match value {
            "not_built" => Ok(Self::NotBuilt),
            "available" => Ok(Self::Available),
            "stale" => Ok(Self::Stale),
            "unavailable" => Ok(Self::Unavailable),
            "failed" => Ok(Self::Failed),
            _ => anyhow::bail!("Unknown repository view status in ferrus.db: {value:?}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CanonicalGraphStatus {
    #[default]
    Unknown,
    Stale,
    Fresh,
}

impl CanonicalGraphStatus {
    fn from_database(value: &str) -> Result<Self> {
        match value {
            "unknown" => Ok(Self::Unknown),
            "stale" => Ok(Self::Stale),
            "fresh" => Ok(Self::Fresh),
            _ => anyhow::bail!("Unknown canonical graph status in ferrus.db: {value:?}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalSourceIdentity {
    pub source_revision_id: SourceRevisionId,
    pub manifest_digest: Digest,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CanonicalGraphReference {
    pub source: Option<CanonicalSourceIdentity>,
    pub snapshot_id: Option<SnapshotId>,
    pub status: CanonicalGraphStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanonicalGraphRefreshGuard {
    invalidation_event_id: i64,
    refresh_event_id: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalGraphRefreshOutcome {
    Recorded,
    Superseded,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepositoryGraphRetentionReferences {
    pub snapshot_ids: std::collections::BTreeSet<SnapshotId>,
    pub view_names: std::collections::BTreeSet<PublishedViewName>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalInvalidationReason {
    ApprovedIntegration,
    PartialMutation,
    SourceComparisonUnavailable,
}

impl CanonicalInvalidationReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::ApprovedIntegration => "approved_integration",
            Self::PartialMutation => "partial_mutation",
            Self::SourceComparisonUnavailable => "source_comparison_unavailable",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepositoryViewReference {
    pub baseline_snapshot_id: Option<SnapshotId>,
    pub overlay_revision_id: Option<OverlayRevisionId>,
    pub view_snapshot_id: Option<SnapshotId>,
    pub frozen_source_tree: Option<Digest>,
    pub lifecycle: TaskViewLifecycle,
    pub status: RepositoryViewStatus,
}

impl RepositoryViewReference {
    pub fn new(
        baseline_snapshot_id: Option<SnapshotId>,
        overlay_revision_id: Option<OverlayRevisionId>,
        status: RepositoryViewStatus,
    ) -> Result<Self> {
        if overlay_revision_id.is_some() && baseline_snapshot_id.is_none() {
            anyhow::bail!("A repository overlay revision requires a baseline snapshot");
        }
        if status == RepositoryViewStatus::Available && baseline_snapshot_id.is_none() {
            anyhow::bail!("An available repository view requires a baseline snapshot");
        }
        Ok(Self {
            baseline_snapshot_id,
            overlay_revision_id,
            view_snapshot_id: None,
            frozen_source_tree: None,
            lifecycle: TaskViewLifecycle::Mutable,
            status,
        })
    }

    pub fn materialized(
        baseline_snapshot_id: SnapshotId,
        overlay_revision_id: Option<OverlayRevisionId>,
        view_snapshot_id: SnapshotId,
        status: RepositoryViewStatus,
    ) -> Result<Self> {
        if status == RepositoryViewStatus::NotBuilt {
            anyhow::bail!("A materialized repository view cannot be not_built");
        }
        Ok(Self {
            baseline_snapshot_id: Some(baseline_snapshot_id),
            overlay_revision_id,
            view_snapshot_id: Some(view_snapshot_id),
            frozen_source_tree: None,
            lifecycle: TaskViewLifecycle::Mutable,
            status,
        })
    }

    pub fn frozen(mut self, source_tree: Digest) -> Result<Self> {
        if self.baseline_snapshot_id.is_none() || self.view_snapshot_id.is_none() {
            anyhow::bail!(
                "A frozen repository view requires materialized baseline and view snapshots"
            );
        }
        self.lifecycle = TaskViewLifecycle::FrozenSubmitted;
        self.frozen_source_tree = Some(source_tree);
        Ok(self)
    }

    pub fn mutable_successor(mut self) -> Self {
        self.lifecycle = TaskViewLifecycle::Mutable;
        self.frozen_source_tree = None;
        self
    }

    fn validate(&self) -> Result<()> {
        if self.overlay_revision_id.is_some() && self.baseline_snapshot_id.is_none() {
            anyhow::bail!("A repository overlay revision requires a baseline snapshot");
        }
        if self.status == RepositoryViewStatus::Available && self.baseline_snapshot_id.is_none() {
            anyhow::bail!("An available repository view requires a baseline snapshot");
        }
        match self.lifecycle {
            TaskViewLifecycle::Mutable if self.frozen_source_tree.is_some() => {
                anyhow::bail!("A mutable repository view cannot retain a frozen source tree");
            }
            TaskViewLifecycle::FrozenSubmitted
                if self.view_snapshot_id.is_none() || self.frozen_source_tree.is_none() =>
            {
                anyhow::bail!("A frozen repository view requires a snapshot and source tree");
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LocalProjectRef {
    pub project_id: String,
    pub name: String,
    pub data_dir: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProjectMetadata {
    pub id: String,
    pub name: String,
    pub workspace_dir: String,
    pub ferrus_dir: String,
    pub vcs: Option<String>,
    pub origin_repo: Option<String>,
    pub default_branch: Option<String>,
    pub current_head: Option<String>,
    pub created_at: String,
    pub last_opened_at: String,
    pub version: u32,
}

#[derive(Debug)]
pub struct ProjectRegistration {
    pub local_ref: LocalProjectRef,
    pub metadata: ProjectMetadata,
    pub data_dir: PathBuf,
    pub database_path: PathBuf,
}

#[derive(Debug)]
pub struct DoctorReport {
    pub registration: ProjectRegistration,
    pub checks: Vec<DoctorCheck>,
}

#[derive(Debug)]
pub struct DoctorCheck {
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct ProjectListEntry {
    pub id: String,
    pub name: Option<String>,
    pub workspace_dir: Option<String>,
    pub data_dir: PathBuf,
    pub database_exists: bool,
    pub last_opened_at: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeRecovery {
    pub interrupted_runs: usize,
    pub expired_task_leases: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    pub id: String,
    pub task_id: String,
    pub role: String,
    pub agent: String,
    pub status: String,
    pub started_at: String,
    pub updated_at: String,
    pub pid: Option<u32>,
    pub workspace_path: String,
}

#[derive(Debug, Clone)]
pub struct EventRecord {
    pub id: i64,
    pub run_id: Option<String>,
    pub event_type: String,
    pub payload_json: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecArchiveResult {
    pub archive_dir: String,
    pub archived_tasks: usize,
    pub archived_runs: usize,
}

#[derive(Debug, Clone)]
pub struct TaskArtifact {
    pub id: String,
    pub path: String,
    pub run_dir: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectSelection {
    pub selected_spec: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRecord {
    pub id: String,
    pub path: String,
    pub spec_path: Option<String>,
    pub milestone_id: Option<String>,
    pub status: String,
    pub paused_status: Option<String>,
    pub claimed_by: Option<String>,
    pub lease_until: Option<String>,
    pub last_heartbeat: Option<String>,
    pub check_retries: u32,
    pub review_cycles: u32,
    pub failure_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumanQuestion {
    pub task_id: String,
    pub task_path: String,
    pub run_dir: String,
    pub question: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnsweredHumanWaiter {
    pub task_id: String,
    pub awaiting_human_by: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RuntimeTaskContext {
    pub task_id: String,
    pub task_path: String,
    pub spec_path: Option<String>,
    pub milestone_id: Option<String>,
    pub run_dir: String,
    pub status: String,
    pub paused_status: Option<String>,
    pub check_retries: u32,
    pub review_cycles: u32,
    pub failure_reason: Option<String>,
    pub run_id: Option<String>,
    pub run_role: Option<String>,
    pub workspace_path: Option<String>,
    pub repository_workspace_path: Option<String>,
    pub repository_view: RepositoryViewReference,
}

#[derive(Debug, Clone)]
struct CurrentTaskRecord {
    id: String,
    path: String,
    #[cfg(test)]
    spec_path: Option<String>,
    #[cfg(test)]
    milestone_id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum TaskClaim {
    Claimed,
    AlreadyClaimed,
    ClaimedByOther { claimed_by: String },
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct TaskLease {
    pub task_id: String,
    pub task_path: String,
    pub status: String,
    pub paused_status: Option<String>,
    pub check_retries: u32,
    pub review_cycles: u32,
    pub failure_reason: Option<String>,
    pub claimed_by: String,
    pub lease_until: DateTime<Utc>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum ReadyTaskClaim {
    Claimed(TaskLease),
    AlreadyClaimed(TaskLease),
    NoAvailable,
}

#[derive(Debug, Clone)]
pub enum LeaseRenewal {
    Renewed {
        task_id: String,
        task_path: String,
        claimed_by: String,
        lease_until: DateTime<Utc>,
    },
    NotClaimed,
    Expired,
}

#[derive(Debug, Clone)]
pub enum TaskCheckFailure {
    Failed { retries: u32 },
    LimitExceeded { retries: u32 },
}

#[derive(Debug, Clone)]
pub enum TaskReviewRejection {
    Addressing { cycles: u32 },
    LimitExceeded { cycles: u32 },
}

/// Outcome of gating one executor dispatch (spawn) against the per-work-phase
/// ceiling.
#[derive(Debug, Clone)]
pub enum ExecutorDispatchOutcome {
    /// The dispatch is within budget; HQ may spawn the executor session.
    Proceed,
    /// The task has already consumed its dispatch budget for this phase without
    /// reaching review; it has been transitioned to Failed and must not be spawned.
    LimitExceeded { dispatches: u32 },
}

#[derive(Debug, Clone)]
pub enum TaskConsultRestore {
    Restored { status: String },
    NotInConsultation,
}

#[derive(Debug, Clone)]
pub enum TaskHumanAnswerRestore {
    Restored { status: String },
    NotAwaitingHuman,
}

impl DoctorReport {
    pub fn has_errors(&self) -> bool {
        self.checks.iter().any(|check| !check.ok)
    }
}

mod registry;
pub use registry::*;
#[cfg(test)]
use registry::{
    add_recovery_doctor_checks, add_runtime_doctor_checks, list_registered_projects_from,
    migrate_legacy_active_task, migrate_legacy_project_selection,
};

#[allow(dead_code)]
mod graph;
pub use graph::*;

mod task;
use task::task_record_from_row;
pub use task::*;

mod claims;
pub use claims::*;
use claims::{consultation_context_for_run, default_task_path_for_id, latest_active_run_for_agent};
use claims::{latest_executor_workspace_for_task, run_dir_for_task};

mod runs;
#[cfg(test)]
use runs::orphaned_worktrees_for;
use runs::preview_runtime_recovery_from;
pub use runs::*;

mod database;
pub(crate) use database::prepare_runtime_database_for_read_only_operations;
use database::*;

async fn copy_legacy_artifacts(copy_task: bool) -> Result<()> {
    if copy_task {
        copy_if_nonempty(".ferrus/TASK.md", ".ferrus/tasks/t-001.md").await?;
    }
    tokio::fs::write(".ferrus/TASK.md", crate::templates::TASK_TEMPLATE)
        .await
        .context("Failed to restore .ferrus/TASK.md template")?;
    if !copy_task {
        return Ok(());
    }
    tokio::fs::create_dir_all(".ferrus/runs/t-001")
        .await
        .context("Failed to create .ferrus/runs/t-001")?;
    copy_if_nonempty(".ferrus/REVIEW.md", ".ferrus/runs/t-001/REVIEW.md").await?;
    copy_if_nonempty(".ferrus/SUBMISSION.md", ".ferrus/runs/t-001/SUBMISSION.md").await?;
    copy_if_nonempty(".ferrus/QUESTION.md", ".ferrus/runs/t-001/QUESTION.md").await?;
    copy_if_nonempty(".ferrus/ANSWER.md", ".ferrus/runs/t-001/ANSWER.md").await?;
    copy_if_nonempty(
        ".ferrus/CONSULT_REQUEST.md",
        ".ferrus/runs/t-001/CONSULT_REQUEST.md",
    )
    .await?;
    copy_if_nonempty(
        ".ferrus/CONSULT_RESPONSE.md",
        ".ferrus/runs/t-001/CONSULT_RESPONSE.md",
    )
    .await?;
    Ok(())
}

async fn retire_legacy_current_task_row() -> Result<()> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = transaction
            .query_row(
                r#"
                SELECT status, paused_status, spec_path, milestone_id, check_retries,
                       review_cycles, failure_reason, awaiting_human_by, awaiting_human_status
                FROM tasks
                WHERE id = ?1
                "#,
                [CURRENT_TASK_ID],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, Option<String>>(8)?,
                    ))
                },
            )
            .optional()?;

        if let Some((
            status,
            paused_status,
            spec_path,
            milestone_id,
            check_retries,
            review_cycles,
            failure_reason,
            awaiting_human_by,
            awaiting_human_status,
        )) = current
        {
            let parsed = status.parse::<TaskStatus>().unwrap_or(TaskStatus::Unknown);
            if !matches!(parsed, TaskStatus::Unknown | TaskStatus::Reset) {
                transaction.execute(
                    r#"
                    INSERT INTO tasks (
                        id, path, status, paused_status, spec_path, milestone_id,
                        check_retries, review_cycles, failure_reason, awaiting_human_by,
                        awaiting_human_status
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                    ON CONFLICT(id) DO NOTHING
                    "#,
                    params![
                        "t-001",
                        ".ferrus/tasks/t-001.md",
                        status,
                        paused_status,
                        spec_path,
                        milestone_id,
                        check_retries,
                        review_cycles,
                        failure_reason,
                        awaiting_human_by,
                        awaiting_human_status,
                    ],
                )?;
                transaction.execute(
                    "UPDATE runs SET task_id = ?1, updated_at = ?2 WHERE task_id = ?3",
                    params!["t-001", timestamp(), CURRENT_TASK_ID],
                )?;
            }

            transaction.execute(
                r#"
                UPDATE tasks
                SET status = ?1,
                    paused_status = NULL,
                    claimed_by = NULL,
                    lease_until = NULL,
                    last_heartbeat = NULL,
                    failure_reason = NULL,
                    awaiting_human_by = NULL,
                    awaiting_human_status = NULL
                WHERE id = ?2
                "#,
                params![TaskStatus::Reset.as_str(), CURRENT_TASK_ID],
            )?;
        }

        transaction.commit()?;
        Ok(())
    })
    .await?
}

async fn remove_legacy_state_files() -> Result<()> {
    for path in [".ferrus/STATE.json", ".ferrus/STATE.lock"] {
        match tokio::fs::remove_file(path).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("Failed to remove {path}")),
        }
    }
    Ok(())
}

async fn copy_if_nonempty(from: &str, to: &str) -> Result<()> {
    if Path::new(to).exists() {
        return Ok(());
    }
    let Ok(contents) = tokio::fs::read_to_string(from).await else {
        return Ok(());
    };
    if contents.trim().is_empty() {
        return Ok(());
    }
    tokio::fs::write(to, contents)
        .await
        .with_context(|| format!("Failed to write {to}"))
}

#[derive(Debug, Serialize)]
struct SpecArchiveManifest {
    spec_path: String,
    archived_at: String,
    tasks: Vec<SpecArchiveTaskManifest>,
}

#[derive(Debug, Serialize)]
struct SpecArchiveTaskManifest {
    id: String,
    status: String,
    milestone_id: Option<String>,
    original_task_path: String,
    archived_task_path: String,
    original_run_dir: String,
    archived_run_dir: String,
}

impl SpecArchiveManifest {
    fn new(spec_path: &str, archived_at: &str, tasks: &[TaskRecord]) -> Self {
        let tasks = tasks
            .iter()
            .map(|task| SpecArchiveTaskManifest {
                id: task.id.clone(),
                status: task.status.clone(),
                milestone_id: task.milestone_id.clone(),
                original_task_path: task.path.clone(),
                archived_task_path: format!("tasks/{}.md", task.id),
                original_run_dir: run_dir_for_task(&task.id),
                archived_run_dir: format!("runs/{}", task.id),
            })
            .collect();
        Self {
            spec_path: spec_path.to_string(),
            archived_at: archived_at.to_string(),
            tasks,
        }
    }
}

async fn unique_spec_archive_dir(
    data_dir: &Path,
    spec_path: &str,
    closed_at: &str,
) -> Result<PathBuf> {
    let slug = spec_archive_slug(spec_path);
    let safe_time = closed_at.replace([':', '-'], "").replace('T', "-");
    let base = data_dir
        .join("archive")
        .join("specs")
        .join(format!("{slug}-{safe_time}"));
    let mut candidate = base.clone();
    let mut suffix = 2u32;
    while tokio::fs::metadata(&candidate).await.is_ok() {
        candidate = data_dir
            .join("archive")
            .join("specs")
            .join(format!("{slug}-{safe_time}-{suffix}"));
        suffix += 1;
    }
    Ok(candidate)
}

fn spec_archive_slug(spec_path: &str) -> String {
    let stem = Path::new(spec_path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("spec");
    let mut slug = String::new();
    let mut previous_dash = false;
    for ch in stem.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            previous_dash = false;
        } else if !previous_dash && !slug.is_empty() {
            slug.push('-');
            previous_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "spec".to_string()
    } else {
        slug.to_string()
    }
}

fn stage_spec_archive_files(
    archive_dir: &Path,
    spec_path: &str,
    tasks: &[TaskRecord],
    manifest: &SpecArchiveManifest,
) -> Result<(usize, usize)> {
    let tasks_dir = archive_dir.join("tasks");
    let runs_dir = archive_dir.join("runs");
    std::fs::create_dir_all(&tasks_dir)
        .with_context(|| format!("Failed to create {}", tasks_dir.display()))?;
    std::fs::create_dir_all(&runs_dir)
        .with_context(|| format!("Failed to create {}", runs_dir.display()))?;

    std::fs::copy(spec_path, archive_dir.join("spec.md")).with_context(|| {
        format!(
            "Failed to copy spec {} to {}",
            spec_path,
            archive_dir.join("spec.md").display()
        )
    })?;

    let manifest_text =
        toml::to_string_pretty(manifest).context("Failed to render archive manifest")?;
    std::fs::write(archive_dir.join("manifest.toml"), manifest_text).with_context(|| {
        format!(
            "Failed to write archive manifest {}",
            archive_dir.join("manifest.toml").display()
        )
    })?;

    let mut archived_tasks = 0usize;
    let mut archived_runs = 0usize;
    for task in tasks {
        let task_path = Path::new(&task.path);
        if registry::checkout_task_artifact_path(task_path) && task_path.exists() {
            copy_path_recursive(task_path, &tasks_dir.join(format!("{}.md", task.id)))?;
            archived_tasks += 1;
        }

        let run_dir = PathBuf::from(run_dir_for_task(&task.id));
        if registry::checkout_task_artifact_path(task_path) && run_dir.exists() {
            copy_path_recursive(&run_dir, &runs_dir.join(&task.id))?;
            archived_runs += 1;
        }
    }
    Ok((archived_tasks, archived_runs))
}

fn cleanup_checkout_archive_artifacts(tasks: &[TaskRecord]) -> Result<()> {
    for task in tasks {
        let task_path = Path::new(&task.path);
        if registry::checkout_task_artifact_path(task_path) && task_path.exists() {
            remove_path_recursive(task_path)?;
        }

        let run_dir = PathBuf::from(run_dir_for_task(&task.id));
        if registry::checkout_task_artifact_path(task_path) && run_dir.exists() {
            remove_path_recursive(&run_dir)?;
        }
    }
    Ok(())
}

fn copy_path_recursive(from: &Path, to: &Path) -> Result<()> {
    let metadata =
        std::fs::metadata(from).with_context(|| format!("Failed to inspect {}", from.display()))?;
    if metadata.is_dir() {
        std::fs::create_dir_all(to)
            .with_context(|| format!("Failed to create {}", to.display()))?;
        for entry in
            std::fs::read_dir(from).with_context(|| format!("Failed to read {}", from.display()))?
        {
            let entry = entry?;
            copy_path_recursive(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else {
        std::fs::copy(from, to)
            .with_context(|| format!("Failed to copy {} to {}", from.display(), to.display()))?;
    }
    Ok(())
}

fn remove_path_recursive(path: &Path) -> Result<()> {
    let metadata =
        std::fs::metadata(path).with_context(|| format!("Failed to inspect {}", path.display()))?;
    if metadata.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
    .with_context(|| format!("Failed to remove {}", path.display()))
}

async fn read_local_project_ref() -> Result<LocalProjectRef> {
    let path = project_path(LOCAL_PROJECT_TOML);
    let contents = tokio::fs::read_to_string(&path)
        .await
        .context("Failed to read .ferrus/project.toml")?;
    toml::from_str(&contents).context("Failed to parse .ferrus/project.toml")
}

fn project_path(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if path.is_absolute() || !starts_with_ferrus_dir(path) {
        return path.to_path_buf();
    }
    if path == Path::new(LOCAL_PROJECT_TOML) {
        return path.to_path_buf();
    }
    std::env::var(ENV_PROJECT_ROOT)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|root| root.join(path))
        .or_else(|| canonical_project_root_from_local_project_ref().map(|root| root.join(path)))
        .unwrap_or_else(|| path.to_path_buf())
}

fn canonical_project_root_from_local_project_ref() -> Option<PathBuf> {
    let contents = std::fs::read_to_string(LOCAL_PROJECT_TOML).ok()?;
    let local_ref = toml::from_str::<LocalProjectRef>(&contents).ok()?;
    let metadata_path = Path::new(&local_ref.data_dir).join("project.toml");
    let metadata = std::fs::read_to_string(metadata_path).ok()?;
    let metadata = toml::from_str::<ProjectMetadata>(&metadata).ok()?;
    Some(PathBuf::from(metadata.workspace_dir))
}

fn starts_with_ferrus_dir(path: &Path) -> bool {
    path.components()
        .next()
        .and_then(|component| match component {
            std::path::Component::Normal(value) => value.to_str(),
            _ => None,
        })
        == Some(".ferrus")
}

async fn read_project_metadata_from(path: &Path) -> Result<ProjectMetadata> {
    let contents = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("Failed to read {}", path.display()))?;
    toml::from_str(&contents).with_context(|| format!("Failed to parse {}", path.display()))
}

async fn write_toml<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let contents = toml::to_string_pretty(value).context("Failed to serialize project metadata")?;
    tokio::fs::write(path, contents)
        .await
        .with_context(|| format!("Failed to write {}", path.display()))
}

fn global_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Cannot determine home directory")?;
    Ok(home.join(".ferrus"))
}

fn project_data_dir(project_id: &str) -> Result<PathBuf> {
    validate_project_id(project_id)?;
    Ok(global_dir()?.join("projects").join(project_id))
}

async fn canonical_current_dir() -> Result<PathBuf> {
    let current = std::env::current_dir().context("Failed to read current directory")?;
    tokio::fs::canonicalize(current)
        .await
        .context("Failed to canonicalize current directory")
}

async fn equivalent_paths(left: &Path, right: &Path) -> bool {
    let left = tokio::fs::canonicalize(left)
        .await
        .unwrap_or_else(|_| left.to_path_buf());
    let right = tokio::fs::canonicalize(right)
        .await
        .unwrap_or_else(|_| right.to_path_buf());
    left == right
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn generate_project_id(workspace_dir: &Path) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let mut hasher = DefaultHasher::new();
    workspace_dir.hash(&mut hasher);
    millis.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    let hash = hasher.finish();
    format!("P{:012X}{:016X}", millis & 0xFFFFFFFFFFFF, hash)
}

fn generate_run_id(role: &str, agent: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let mut hasher = DefaultHasher::new();
    role.hash(&mut hasher);
    agent.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    RUN_ID_COUNTER
        .fetch_add(1, Ordering::Relaxed)
        .hash(&mut hasher);
    millis.hash(&mut hasher);
    let hash = hasher.finish();
    format!("r-{:012x}-{:016x}", millis & 0xFFFFFFFFFFFF, hash)
}

fn parse_task_number(task_id: &str) -> Option<u32> {
    task_id.strip_prefix("t-")?.parse().ok()
}

fn validate_project_id(project_id: &str) -> Result<()> {
    let valid = !project_id.is_empty()
        && project_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if valid {
        Ok(())
    } else {
        anyhow::bail!("Invalid project_id in .ferrus/project.toml: {project_id:?}")
    }
}

#[derive(Debug)]
struct GitMetadata {
    origin_repo: Option<String>,
    default_branch: Option<String>,
    current_head: Option<String>,
}

async fn read_git_metadata() -> Option<GitMetadata> {
    if git_output(["rev-parse", "--is-inside-work-tree"]).await? != "true" {
        return None;
    }
    Some(GitMetadata {
        origin_repo: git_output(["config", "--get", "remote.origin.url"]).await,
        default_branch: read_default_branch().await,
        current_head: git_output(["rev-parse", "HEAD"]).await,
    })
}

async fn read_default_branch() -> Option<String> {
    if let Some(branch) = git_output(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]).await
    {
        return branch
            .strip_prefix("origin/")
            .unwrap_or(&branch)
            .to_string()
            .into();
    }
    git_output(["rev-parse", "--abbrev-ref", "HEAD"]).await
}

async fn git_output<const N: usize>(args: [&str; N]) -> Option<String> {
    let output = Command::new("git").args(args).output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn process_is_alive(pid: u32) -> bool {
    platform::pid_is_alive(pid)
}

pub async fn live_active_run_task_ids() -> Result<std::collections::HashSet<String>> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<std::collections::HashSet<String>> {
        let connection = open_runtime_database(&database_path)?;
        live_active_run_task_ids_from_database(&connection)
    })
    .await?
}

pub async fn live_active_run_task_ids_for_role(
    role: &str,
) -> Result<std::collections::HashSet<String>> {
    let database_path = current_database_path().await?;
    let role = role.to_string();
    tokio::task::spawn_blocking(move || -> Result<std::collections::HashSet<String>> {
        let connection = open_runtime_database(&database_path)?;
        let mut statement = connection.prepare(
            r#"
            SELECT task_id, pid
            FROM runs
            WHERE role = ?1 AND status IN ('running', 'checking', 'reviewing')
            "#,
        )?;
        let rows = statement.query_map([role], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
        })?;
        let mut task_ids = std::collections::HashSet::new();
        for row in rows {
            let (task_id, pid) = row?;
            if pid.is_some_and(|pid| process_is_alive(pid as u32)) {
                task_ids.insert(task_id);
            }
        }
        Ok(task_ids)
    })
    .await?
}

pub async fn live_active_run_agents() -> Result<std::collections::HashSet<String>> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<std::collections::HashSet<String>> {
        let connection = open_runtime_database(&database_path)?;
        let mut statement = connection.prepare(
            "SELECT agent, pid FROM runs WHERE status IN ('running', 'checking', 'reviewing')",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
        })?;
        let mut agents = std::collections::HashSet::new();
        for row in rows {
            let (agent, pid) = row?;
            if pid.is_some_and(|pid| process_is_alive(pid as u32)) {
                agents.insert(agent);
            }
        }
        Ok(agents)
    })
    .await?
}

fn live_active_run_task_ids_from_database(
    connection: &Connection,
) -> Result<std::collections::HashSet<String>> {
    let mut statement = connection.prepare(
        "SELECT task_id, pid FROM runs WHERE status IN ('running', 'checking', 'reviewing')",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
    })?;
    let mut task_ids = std::collections::HashSet::new();
    for row in rows {
        let (task_id, pid) = row?;
        if pid.is_some_and(|pid| process_is_alive(pid as u32)) {
            task_ids.insert(task_id);
        }
    }
    Ok(task_ids)
}

#[cfg(test)]
#[path = "project_tests.rs"]
mod tests;
