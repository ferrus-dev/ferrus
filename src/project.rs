use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
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
};

const PROJECT_VERSION: u32 = 1;
const LOCAL_PROJECT_TOML: &str = ".ferrus/project.toml";
const CURRENT_TASK_ID: &str = "current";
const CURRENT_TASK_PATH: &str = ".ferrus/TASK.md";
const BASELINE_WORKTREE_METADATA_DIR: &str = ".baseline-trees";
static RUN_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    pub workspace_path: Option<String>,
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

pub async fn ensure_global_dir() -> Result<PathBuf> {
    let root = global_dir()?;
    tokio::fs::create_dir_all(root.join("projects"))
        .await
        .with_context(|| format!("Failed to create {}", root.join("projects").display()))?;
    Ok(root)
}

pub async fn register_current_project() -> Result<ProjectRegistration> {
    ensure_global_dir().await?;
    let workspace_dir = canonical_current_dir()
        .await
        .context("Failed to resolve current workspace directory")?;
    let ferrus_dir = workspace_dir.join(".ferrus");
    tokio::fs::create_dir_all(&ferrus_dir)
        .await
        .with_context(|| format!("Failed to create {}", ferrus_dir.display()))?;

    let now = timestamp();
    let existing = read_local_project_ref().await.ok();
    let project_id = if let Some(project) = existing.as_ref() {
        validate_project_id(&project.project_id)?;
        project.project_id.clone()
    } else {
        generate_project_id(&workspace_dir)
    };
    let data_dir = project_data_dir(&project_id)?;
    tokio::fs::create_dir_all(data_dir.join("logs"))
        .await
        .with_context(|| format!("Failed to create {}", data_dir.join("logs").display()))?;

    let project_toml_path = data_dir.join("project.toml");
    let previous_metadata = read_project_metadata_from(&project_toml_path).await.ok();
    let created_at = previous_metadata
        .as_ref()
        .map(|metadata| metadata.created_at.clone())
        .unwrap_or_else(|| now.clone());
    let name = workspace_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project")
        .to_string();
    let git = read_git_metadata().await;
    let metadata = ProjectMetadata {
        id: project_id.clone(),
        name: name.clone(),
        workspace_dir: path_string(&workspace_dir),
        ferrus_dir: path_string(&ferrus_dir),
        vcs: git.as_ref().map(|_| "git".to_string()),
        origin_repo: git.as_ref().and_then(|git| git.origin_repo.clone()),
        default_branch: git.as_ref().and_then(|git| git.default_branch.clone()),
        current_head: git.as_ref().and_then(|git| git.current_head.clone()),
        created_at,
        last_opened_at: now,
        version: PROJECT_VERSION,
    };
    write_toml(&project_toml_path, &metadata).await?;

    let local_ref = LocalProjectRef {
        project_id,
        name,
        data_dir: path_string(&data_dir),
    };
    write_toml(&project_path(LOCAL_PROJECT_TOML), &local_ref).await?;

    let database_path = data_dir.join("ferrus.db");
    initialize_database(&database_path).await?;

    Ok(ProjectRegistration {
        local_ref,
        metadata,
        data_dir,
        database_path,
    })
}

pub async fn migrate_current_project() -> Result<ProjectRegistration> {
    let registration = register_current_project().await?;
    tokio::fs::create_dir_all(".ferrus/tasks")
        .await
        .context("Failed to create .ferrus/tasks")?;
    tokio::fs::create_dir_all(".ferrus/runs")
        .await
        .context("Failed to create .ferrus/runs")?;
    copy_legacy_artifacts().await?;
    if let Ok(state) = legacy_state::read_legacy_state(project_path(".ferrus/STATE.json")).await {
        migrate_legacy_project_selection(&state).await?;
        let state_value = state.state();
        if state_value != LegacyTaskState::Idle {
            record_task_status_with_origin(
                "t-001",
                ".ferrus/tasks/t-001.md",
                legacy_state::task_status_for_legacy_state(&state_value),
                state.task_spec.as_deref(),
                state.task_milestone.as_deref(),
            )
            .await?;
        }
    }
    retire_legacy_current_task_row().await?;
    remove_legacy_state_files().await?;
    Ok(registration)
}

async fn migrate_legacy_project_selection(state: &legacy_state::LegacyStateData) -> Result<()> {
    if let Some(selected_spec) = normalized_metadata_value(state.selected_spec.as_deref()) {
        write_project_selection(&ProjectSelection {
            selected_spec: Some(selected_spec),
        })
        .await?;
    }
    Ok(())
}

pub async fn touch_current_project() -> Result<ProjectRegistration> {
    let local_ref = read_local_project_ref()
        .await
        .context(".ferrus/project.toml not found or invalid — run `ferrus migrate`")?;
    validate_project_id(&local_ref.project_id)?;
    let data_dir = PathBuf::from(&local_ref.data_dir);
    tokio::fs::create_dir_all(data_dir.join("logs"))
        .await
        .with_context(|| format!("Failed to create {}", data_dir.join("logs").display()))?;

    let metadata_path = data_dir.join("project.toml");
    let previous_metadata = read_project_metadata_from(&metadata_path)
        .await
        .with_context(|| format!("Failed to read {}", metadata_path.display()))?;
    if previous_metadata.id != local_ref.project_id {
        anyhow::bail!(
            "local project_id {} does not match global metadata id {}",
            local_ref.project_id,
            previous_metadata.id
        );
    }

    let workspace_dir = canonical_current_dir()
        .await
        .context("Failed to resolve current workspace directory")?;
    let ferrus_dir = workspace_dir.join(".ferrus");
    let name = workspace_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project")
        .to_string();
    let git = read_git_metadata().await;
    let metadata = ProjectMetadata {
        id: local_ref.project_id.clone(),
        name,
        workspace_dir: path_string(&workspace_dir),
        ferrus_dir: path_string(&ferrus_dir),
        vcs: git.as_ref().map(|_| "git".to_string()),
        origin_repo: git.as_ref().and_then(|git| git.origin_repo.clone()),
        default_branch: git.as_ref().and_then(|git| git.default_branch.clone()),
        current_head: git.as_ref().and_then(|git| git.current_head.clone()),
        created_at: previous_metadata.created_at,
        last_opened_at: timestamp(),
        version: PROJECT_VERSION,
    };
    write_toml(&metadata_path, &metadata).await?;
    initialize_database(&data_dir.join("ferrus.db")).await?;

    Ok(ProjectRegistration {
        local_ref,
        metadata,
        database_path: data_dir.join("ferrus.db"),
        data_dir,
    })
}

pub async fn allocate_task_artifact() -> Result<TaskArtifact> {
    let tasks_dir = Path::new(".ferrus/tasks");
    let runs_dir = Path::new(".ferrus/runs");
    tokio::fs::create_dir_all(tasks_dir)
        .await
        .context("Failed to create .ferrus/tasks")?;
    tokio::fs::create_dir_all(runs_dir)
        .await
        .context("Failed to create .ferrus/runs")?;

    let mut max_number = max_task_number_from_files(tasks_dir).await?;
    if let Ok(database_path) = current_database_path().await {
        max_number = max_number.max(max_task_number_from_database(&database_path).await?);
    }

    let mut number = max_number + 1;
    loop {
        let id = format!("t-{number:03}");
        let task_path = tasks_dir.join(format!("{id}.md"));
        if !task_path.exists() {
            // Store project-local artifact paths with `/` separators. Rust accepts these paths on
            // Windows too, and keeping the serialized STATE/DB value stable avoids platform drift.
            return Ok(TaskArtifact {
                path: format!(".ferrus/tasks/{id}.md"),
                run_dir: format!(".ferrus/runs/{id}"),
                id,
            });
        }
        number += 1;
    }
}

pub async fn doctor_current_project() -> Result<DoctorReport> {
    let local_ref = read_local_project_ref()
        .await
        .context(".ferrus/project.toml not found or invalid — run `ferrus migrate`")?;
    let data_dir = PathBuf::from(&local_ref.data_dir);
    let metadata_path = data_dir.join("project.toml");
    let metadata = read_project_metadata_from(&metadata_path)
        .await
        .with_context(|| format!("Failed to read {}", metadata_path.display()))?;
    let database_path = data_dir.join("ferrus.db");
    let current_dir = canonical_current_dir().await?;
    let current_ferrus_dir = current_dir.join(".ferrus");
    let expected_data_dir = project_data_dir(&local_ref.project_id)?;

    let mut checks = Vec::new();
    checks.push(DoctorCheck {
        ok: local_ref.project_id == metadata.id,
        message: format!(
            "local project_id matches global metadata id ({})",
            local_ref.project_id
        ),
    });
    checks.push(DoctorCheck {
        ok: equivalent_paths(&data_dir, &expected_data_dir).await,
        message: format!("data_dir points at {}", expected_data_dir.display()),
    });
    checks.push(DoctorCheck {
        ok: equivalent_paths(Path::new(&metadata.workspace_dir), &current_dir).await,
        message: format!("workspace_dir points at {}", current_dir.display()),
    });
    checks.push(DoctorCheck {
        ok: equivalent_paths(Path::new(&metadata.ferrus_dir), &current_ferrus_dir).await,
        message: format!("ferrus_dir points at {}", current_ferrus_dir.display()),
    });
    checks.push(DoctorCheck {
        ok: tokio::fs::metadata(&database_path).await.is_ok(),
        message: format!("database exists at {}", database_path.display()),
    });
    checks.push(DoctorCheck {
        ok: validate_database_schema(&database_path)
            .await
            .unwrap_or(false),
        message: "database has tasks, runs, events, and task lease columns".to_string(),
    });
    add_recovery_doctor_checks(&mut checks, &database_path).await;
    add_runtime_doctor_checks(&mut checks, &database_path).await;

    Ok(DoctorReport {
        registration: ProjectRegistration {
            local_ref,
            metadata,
            data_dir,
            database_path,
        },
        checks,
    })
}

async fn add_recovery_doctor_checks(checks: &mut Vec<DoctorCheck>, database_path: &Path) {
    let recovery = match preview_runtime_recovery_from(database_path).await {
        Ok(recovery) => recovery,
        Err(err) => {
            checks.push(DoctorCheck {
                ok: false,
                message: format!("runtime recovery preview can read ferrus.db ({err})"),
            });
            return;
        }
    };

    checks.push(DoctorCheck {
        ok: recovery.interrupted_runs == 0,
        message: format!(
            "no interrupted run recovery pending ({} found; run `ferrus recover`)",
            recovery.interrupted_runs
        ),
    });
    checks.push(DoctorCheck {
        ok: recovery.expired_task_leases == 0,
        message: format!(
            "no expired task lease recovery pending ({} found; run `ferrus recover`)",
            recovery.expired_task_leases
        ),
    });
}

async fn add_runtime_doctor_checks(checks: &mut Vec<DoctorCheck>, database_path: &Path) {
    let task_rows = match read_task_records_from_database(database_path).await {
        Ok(rows) => rows,
        Err(err) => {
            checks.push(DoctorCheck {
                ok: false,
                message: format!("task rows can be read from ferrus.db ({err})"),
            });
            return;
        }
    };
    checks.push(DoctorCheck {
        ok: true,
        message: "task rows can be read from ferrus.db".to_string(),
    });
    for task in task_rows
        .iter()
        .filter(|task| runtime_artifacts_expected_for_status(&task.status))
    {
        checks.push(DoctorCheck {
            ok: tokio::fs::metadata(&task.path).await.is_ok(),
            message: format!("task artifact exists for {} at {}", task.id, task.path),
        });
        let run_dir = run_dir_for_task(&task.id);
        let run_dir_exists = tokio::fs::metadata(&run_dir)
            .await
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false);
        checks.push(DoctorCheck {
            ok: run_dir_exists,
            message: format!(
                "run artifact directory exists for {} at {}",
                task.id, run_dir
            ),
        });
    }
}

fn runtime_artifacts_expected_for_status(status: &str) -> bool {
    !matches!(
        status.parse::<TaskStatus>().ok(),
        Some(TaskStatus::Reset | TaskStatus::Unknown)
    )
}

pub async fn list_registered_projects() -> Result<Vec<ProjectListEntry>> {
    let projects_dir = global_dir()?.join("projects");
    list_registered_projects_from(&projects_dir).await
}

async fn list_registered_projects_from(projects_dir: &Path) -> Result<Vec<ProjectListEntry>> {
    if tokio::fs::metadata(projects_dir).await.is_err() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    let mut read_dir = tokio::fs::read_dir(projects_dir)
        .await
        .with_context(|| format!("Failed to read {}", projects_dir.display()))?;
    while let Some(entry) = read_dir
        .next_entry()
        .await
        .with_context(|| format!("Failed to iterate {}", projects_dir.display()))?
    {
        let file_type = entry
            .file_type()
            .await
            .with_context(|| format!("Failed to inspect {}", entry.path().display()))?;
        if !file_type.is_dir() {
            continue;
        }

        let data_dir = entry.path();
        let fallback_id = entry.file_name().to_string_lossy().into_owned();
        let database_exists = tokio::fs::metadata(data_dir.join("ferrus.db"))
            .await
            .is_ok();
        match read_project_metadata_from(&data_dir.join("project.toml")).await {
            Ok(metadata) => entries.push(ProjectListEntry {
                id: metadata.id,
                name: Some(metadata.name),
                workspace_dir: Some(metadata.workspace_dir),
                data_dir,
                database_exists,
                last_opened_at: Some(metadata.last_opened_at),
                error: None,
            }),
            Err(err) => entries.push(ProjectListEntry {
                id: fallback_id,
                name: None,
                workspace_dir: None,
                data_dir,
                database_exists,
                last_opened_at: None,
                error: Some(err.to_string()),
            }),
        }
    }

    entries.sort_by(|left, right| {
        right
            .last_opened_at
            .cmp(&left.last_opened_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(entries)
}

pub async fn list_tasks() -> Result<Vec<TaskRecord>> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<Vec<TaskRecord>> {
        let connection = open_runtime_database(&database_path)?;
        let mut statement = connection.prepare(
            r#"
            SELECT id, path, spec_path, milestone_id, status, paused_status, claimed_by,
                   lease_until, last_heartbeat, check_retries, review_cycles, failure_reason
            FROM tasks
            ORDER BY
                CASE WHEN id = 'current' THEN 0 ELSE 1 END,
                id
            "#,
        )?;
        let rows = statement.query_map([], task_record_from_row)?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row?);
        }
        Ok(tasks)
    })
    .await?
}

pub async fn list_human_questions() -> Result<Vec<HumanQuestion>> {
    let tasks = list_tasks().await?;
    let mut questions = Vec::new();
    for task in tasks
        .into_iter()
        .filter(|task| task.status == TaskStatus::AwaitingHuman.as_str())
    {
        let run_dir = run_dir_for_task(&task.id);
        let question = crate::state::store::read_question_for_run_dir(&run_dir)
            .await
            .unwrap_or_default()
            .trim()
            .to_string();
        questions.push(HumanQuestion {
            task_id: task.id,
            task_path: task.path,
            run_dir,
            question,
        });
    }
    Ok(questions)
}

#[allow(dead_code)]
pub async fn find_non_terminal_task_by_origin(
    spec_path: &str,
    milestone_id: &str,
) -> Result<Option<TaskRecord>> {
    let database_path = current_database_path().await?;
    let spec_path = spec_path.to_string();
    let milestone_id = milestone_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<TaskRecord>> {
        let connection = open_runtime_database(&database_path)?;
        let task = connection
            .query_row(
                r#"
                SELECT id, path, spec_path, milestone_id, status, paused_status, claimed_by,
                       lease_until, last_heartbeat, check_retries, review_cycles, failure_reason
                FROM tasks
                WHERE spec_path = ?1
                  AND milestone_id = ?2
                  AND status NOT IN (?3, ?4, ?5)
                ORDER BY id
                LIMIT 1
                "#,
                params![
                    spec_path,
                    milestone_id,
                    TaskStatus::Reset.as_str(),
                    TaskStatus::Complete.as_str(),
                    TaskStatus::Failed.as_str()
                ],
                task_record_from_row,
            )
            .optional()?;
        Ok(task)
    })
    .await?
}

pub async fn list_runs(limit: usize) -> Result<Vec<RunRecord>> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<Vec<RunRecord>> {
        let connection = open_runtime_database(&database_path)?;
        let mut statement = connection.prepare(
            r#"
            SELECT id, task_id, role, agent, status, started_at, updated_at, pid, workspace_path
            FROM runs
            ORDER BY updated_at DESC, started_at DESC, id DESC
            LIMIT ?1
            "#,
        )?;
        let rows = statement.query_map([limit as i64], |row| {
            Ok(RunRecord {
                id: row.get(0)?,
                task_id: row.get(1)?,
                role: row.get(2)?,
                agent: row.get(3)?,
                status: row.get(4)?,
                started_at: row.get(5)?,
                updated_at: row.get(6)?,
                pid: row.get::<_, Option<i64>>(7)?.map(|pid| pid as u32),
                workspace_path: row.get(8)?,
            })
        })?;

        let mut runs = Vec::new();
        for row in rows {
            runs.push(row?);
        }
        Ok(runs)
    })
    .await?
}

pub async fn list_events(limit: usize, run_id: Option<String>) -> Result<Vec<EventRecord>> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<Vec<EventRecord>> {
        let connection = open_runtime_database(&database_path)?;
        let mut events = Vec::new();
        if let Some(run_id) = run_id {
            let mut statement = connection.prepare(
                r#"
                SELECT id, run_id, type, payload_json, created_at
                FROM events
                WHERE run_id = ?1
                ORDER BY id DESC
                LIMIT ?2
                "#,
            )?;
            let rows = statement.query_map(params![run_id, limit as i64], event_from_row)?;
            for row in rows {
                events.push(row?);
            }
        } else {
            let mut statement = connection.prepare(
                r#"
                SELECT id, run_id, type, payload_json, created_at
                FROM events
                ORDER BY id DESC
                LIMIT ?1
                "#,
            )?;
            let rows = statement.query_map([limit as i64], event_from_row)?;
            for row in rows {
                events.push(row?);
            }
        }
        Ok(events)
    })
    .await?
}

pub async fn read_project_selection() -> Result<ProjectSelection> {
    let database_path = current_database_path().await?;

    tokio::task::spawn_blocking(move || -> Result<ProjectSelection> {
        let connection = open_runtime_database(&database_path)?;
        read_project_selection_from_database(&connection)
    })
    .await?
}

pub async fn write_project_selection(selection: &ProjectSelection) -> Result<()> {
    let database_path = current_database_path().await?;
    let selection_for_db = selection.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        write_project_selection_to_database(&connection, &selection_for_db)?;
        insert_event(
            &connection,
            None,
            "project_selection_changed",
            &serde_json::json!({
                "selected_spec": selection_for_db.selected_spec,
            }),
        )?;
        Ok(())
    })
    .await??;

    Ok(())
}

pub async fn read_last_spec_path() -> Result<Option<String>> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let connection = open_runtime_database(&database_path)?;
        read_last_spec_path_from_database(&connection)
    })
    .await?
}

pub async fn write_last_spec_path(path: &str) -> Result<()> {
    let database_path = current_database_path().await?;
    let path = path.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        write_last_spec_path_to_database(&connection, Some(&path))?;
        insert_event(
            &connection,
            None,
            "spec_created",
            &serde_json::json!({ "path": path }),
        )?;
        Ok(())
    })
    .await?
}

pub async fn clear_last_spec_path() -> Result<()> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        write_last_spec_path_to_database(&connection, None)?;
        Ok(())
    })
    .await?
}

fn event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRecord> {
    Ok(EventRecord {
        id: row.get(0)?,
        run_id: row.get(1)?,
        event_type: row.get(2)?,
        payload_json: row.get(3)?,
        created_at: row.get(4)?,
    })
}

fn task_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRecord> {
    Ok(TaskRecord {
        id: row.get(0)?,
        path: row.get(1)?,
        spec_path: row.get(2)?,
        milestone_id: row.get(3)?,
        status: row.get(4)?,
        paused_status: row.get(5)?,
        claimed_by: row.get(6)?,
        lease_until: row.get(7)?,
        last_heartbeat: row.get(8)?,
        check_retries: row.get::<_, i64>(9)? as u32,
        review_cycles: row.get::<_, i64>(10)? as u32,
        failure_reason: row.get(11)?,
    })
}

#[cfg(test)]
pub async fn record_current_task_status(status: TaskStatus) -> Result<()> {
    let task = current_task_record().await;
    record_task_status_with_origin(
        &task.id,
        &task.path,
        status,
        task.spec_path.as_deref(),
        task.milestone_id.as_deref(),
    )
    .await
}

pub async fn record_task_status(task_id: &str, task_path: &str, status: TaskStatus) -> Result<()> {
    record_task_status_with_origin(task_id, task_path, status, None, None).await
}

pub async fn record_task_status_with_origin(
    task_id: &str,
    task_path: &str,
    status: TaskStatus,
    spec_path: Option<&str>,
    milestone_id: Option<&str>,
) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let task_path = task_path.to_string();
    let spec_path = spec_path.map(str::to_string);
    let milestone_id = milestone_id.map(str::to_string);
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        upsert_task(
            &connection,
            &task_id,
            &task_path,
            status,
            spec_path.as_deref(),
            milestone_id.as_deref(),
        )?;
        if status.clears_lease() {
            clear_task_lease(&connection, &task_id)?;
        }
        insert_event(
            &connection,
            None,
            "task_status_changed",
            &serde_json::json!({
                "task_id": task_id,
                "status": status.as_str(),
            }),
        )?;
        Ok(())
    })
    .await?
}

pub async fn record_task_check_passed(task_id: &str) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        connection.execute(
            "UPDATE tasks SET check_retries = 0, failure_reason = NULL WHERE id = ?1",
            [&task_id],
        )?;
        insert_event(
            &connection,
            None,
            "task_check_passed",
            &serde_json::json!({ "task_id": task_id }),
        )?;
        Ok(())
    })
    .await?
}

#[cfg(test)]
pub async fn mirror_task_check_state(
    task_id: &str,
    status: TaskStatus,
    check_retries: u32,
    failure_reason: Option<&str>,
) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let failure_reason = failure_reason.map(str::to_string);
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        if status.clears_lease() {
            connection.execute(
                r#"
                UPDATE tasks
                SET status = ?1, check_retries = ?2, failure_reason = ?3,
                    claimed_by = NULL, lease_until = NULL, last_heartbeat = NULL
                WHERE id = ?4
                "#,
                params![status.as_str(), check_retries, failure_reason, task_id],
            )?;
        } else {
            connection.execute(
                "UPDATE tasks SET status = ?1, check_retries = ?2, failure_reason = ?3 WHERE id = ?4",
                params![status.as_str(), check_retries, failure_reason, task_id],
            )?;
        }
        insert_event(
            &connection,
            None,
            "task_check_state_mirrored",
            &serde_json::json!({
                "task_id": task_id,
                "status": status.as_str(),
                "check_retries": check_retries,
            }),
        )?;
        Ok(())
    })
    .await?
}

pub async fn record_task_integration_failed(
    task_id: &str,
    run_id: Option<&str>,
    failure_reason: &str,
) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let run_id = run_id.map(str::to_string);
    let failure_reason = failure_reason.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        connection.execute(
            "UPDATE tasks SET failure_reason = ?1 WHERE id = ?2",
            params![failure_reason, task_id],
        )?;
        insert_event(
            &connection,
            run_id.as_deref(),
            "task_integration_failed",
            &serde_json::json!({
                "task_id": task_id,
                "failure_reason": failure_reason,
            }),
        )?;
        Ok(())
    })
    .await?
}

pub async fn record_task_integration_failed_best_effort(
    task_id: &str,
    run_id: Option<&str>,
    failure_reason: &str,
) {
    if let Err(err) = record_task_integration_failed(task_id, run_id, failure_reason).await {
        warn!(error = ?err, task_id, "failed to mirror task integration failure into ferrus.db");
    }
}

pub async fn record_task_check_failed(
    task_id: &str,
    failure_reason: &str,
    max_retries: u32,
) -> Result<TaskCheckFailure> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let failure_reason = failure_reason.to_string();
    tokio::task::spawn_blocking(move || -> Result<TaskCheckFailure> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        let retries = task_check_retries(&transaction, &task_id)? + 1;
        if retries >= max_retries {
            let limit_failure_reason = format!(
                "Check failed {max_retries} consecutive times. Last failure:\n{failure_reason}"
            );
            transaction.execute(
                r#"
                UPDATE tasks
                SET status = ?1, check_retries = ?2, failure_reason = ?3,
                    claimed_by = NULL, lease_until = NULL, last_heartbeat = NULL
                WHERE id = ?4
                "#,
                params![
                    TaskStatus::Failed.as_str(),
                    retries,
                    limit_failure_reason,
                    task_id
                ],
            )?;
            insert_event_in_transaction(
                &transaction,
                None,
                "task_check_limit_exceeded",
                &serde_json::json!({
                    "task_id": task_id,
                    "retries": retries,
                    "max_retries": max_retries,
                }),
            )?;
            transaction.commit()?;
            Ok(TaskCheckFailure::LimitExceeded { retries })
        } else {
            transaction.execute(
                "UPDATE tasks SET check_retries = ?1, failure_reason = ?2 WHERE id = ?3",
                params![retries, failure_reason, task_id],
            )?;
            insert_event_in_transaction(
                &transaction,
                None,
                "task_check_failed",
                &serde_json::json!({
                    "task_id": task_id,
                    "retries": retries,
                    "max_retries": max_retries,
                }),
            )?;
            transaction.commit()?;
            Ok(TaskCheckFailure::Failed { retries })
        }
    })
    .await?
}

pub async fn record_task_review_rejected(
    task_id: &str,
    max_cycles: u32,
) -> Result<TaskReviewRejection> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<TaskReviewRejection> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        let cycles = task_review_cycles(&transaction, &task_id)? + 1;
        if cycles >= max_cycles {
            transaction.execute(
                r#"
                UPDATE tasks
                SET status = ?1, review_cycles = ?2,
                    failure_reason = ?3,
                    claimed_by = NULL, lease_until = NULL, last_heartbeat = NULL
                WHERE id = ?4
                "#,
                params![
                    TaskStatus::Failed.as_str(),
                    cycles,
                    format!("Task rejected {max_cycles} times without resolution."),
                    task_id
                ],
            )?;
            insert_event_in_transaction(
                &transaction,
                None,
                "task_review_limit_exceeded",
                &serde_json::json!({
                    "task_id": task_id,
                    "review_cycles": cycles,
                    "max_review_cycles": max_cycles,
                }),
            )?;
            transaction.commit()?;
            Ok(TaskReviewRejection::LimitExceeded { cycles })
        } else {
            transaction.execute(
                r#"
                UPDATE tasks
                SET status = ?1, review_cycles = ?2, check_retries = 0,
                    failure_reason = NULL,
                    claimed_by = NULL, lease_until = NULL, last_heartbeat = NULL
                WHERE id = ?3
                "#,
                params![TaskStatus::Addressing.as_str(), cycles, task_id],
            )?;
            insert_event_in_transaction(
                &transaction,
                None,
                "task_rejected",
                &serde_json::json!({
                    "task_id": task_id,
                    "review_cycles": cycles,
                    "max_review_cycles": max_cycles,
                }),
            )?;
            transaction.commit()?;
            Ok(TaskReviewRejection::Addressing { cycles })
        }
    })
    .await?
}

pub async fn record_task_consultation_requested(
    task_id: &str,
    paused_status: TaskStatus,
) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        connection.execute(
            "UPDATE tasks SET status = ?1, paused_status = ?2 WHERE id = ?3",
            params![
                TaskStatus::Consultation.as_str(),
                paused_status.as_str(),
                task_id
            ],
        )?;
        insert_event(
            &connection,
            None,
            "task_consultation_requested",
            &serde_json::json!({
                "task_id": task_id,
                "paused_status": paused_status.as_str(),
            }),
        )?;
        Ok(())
    })
    .await?
}

pub async fn restore_task_from_consultation(task_id: &str) -> Result<TaskConsultRestore> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<TaskConsultRestore> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        let row = transaction
            .query_row(
                "SELECT status, paused_status FROM tasks WHERE id = ?1",
                [&task_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        let Some((status, paused_status)) = row else {
            transaction.commit()?;
            return Ok(TaskConsultRestore::NotInConsultation);
        };
        if status != TaskStatus::Consultation.as_str() {
            transaction.commit()?;
            return Ok(TaskConsultRestore::NotInConsultation);
        }
        let resumed_status =
            paused_status.unwrap_or_else(|| TaskStatus::Executing.as_str().to_string());
        transaction.execute(
            "UPDATE tasks SET status = ?1, paused_status = NULL WHERE id = ?2",
            params![resumed_status, task_id],
        )?;
        insert_event_in_transaction(
            &transaction,
            None,
            "task_consultation_resolved",
            &serde_json::json!({
                "task_id": task_id,
                "resumed_status": resumed_status,
            }),
        )?;
        transaction.commit()?;
        Ok(TaskConsultRestore::Restored {
            status: resumed_status,
        })
    })
    .await?
}

#[cfg(test)]
pub async fn record_task_human_question_requested(
    task_id: &str,
    paused_status: TaskStatus,
    awaiting_human_by: &str,
) -> Result<()> {
    record_task_human_question_requested_with_resume(
        task_id,
        paused_status,
        Some(paused_status),
        awaiting_human_by,
    )
    .await
}

pub async fn record_task_human_question_requested_with_resume(
    task_id: &str,
    resume_status: TaskStatus,
    paused_status: Option<TaskStatus>,
    awaiting_human_by: &str,
) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let awaiting_human_by = awaiting_human_by.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        let paused_status = paused_status.map(TaskStatus::as_str);
        connection.execute(
            r#"
            UPDATE tasks
            SET status = ?1, paused_status = ?2, awaiting_human_by = ?3, awaiting_human_status = ?4
            WHERE id = ?5
            "#,
            params![
                TaskStatus::AwaitingHuman.as_str(),
                paused_status,
                awaiting_human_by,
                resume_status.as_str(),
                task_id
            ],
        )?;
        insert_event(
            &connection,
            None,
            "task_human_question_requested",
            &serde_json::json!({
                "task_id": task_id,
                "paused_status": paused_status,
                "resume_status": resume_status.as_str(),
                "awaiting_human_by": awaiting_human_by,
            }),
        )?;
        Ok(())
    })
    .await?
}

pub async fn task_human_question_owner(task_id: &str) -> Result<Option<String>> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let connection = open_runtime_database(&database_path)?;
        let owner = connection
            .query_row(
                "SELECT awaiting_human_by FROM tasks WHERE id = ?1",
                [&task_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(owner)
    })
    .await?
}

pub async fn task_awaiting_human_status(task_id: &str) -> Result<Option<String>> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let connection = open_runtime_database(&database_path)?;
        let status = connection
            .query_row(
                "SELECT awaiting_human_status FROM tasks WHERE id = ?1",
                [&task_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(status)
    })
    .await?
}

pub async fn restore_task_from_human_answer(task_id: &str) -> Result<TaskHumanAnswerRestore> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<TaskHumanAnswerRestore> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        let row = transaction
            .query_row(
                "SELECT status, paused_status, awaiting_human_status FROM tasks WHERE id = ?1",
                [&task_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((status, paused_status, awaiting_human_status)) = row else {
            transaction.commit()?;
            return Ok(TaskHumanAnswerRestore::NotAwaitingHuman);
        };
        if status != TaskStatus::AwaitingHuman.as_str() {
            transaction.commit()?;
            return Ok(TaskHumanAnswerRestore::NotAwaitingHuman);
        }
        let resumed_status = awaiting_human_status
            .or_else(|| paused_status.clone())
            .unwrap_or_else(|| TaskStatus::Executing.as_str().to_string());
        let restored_paused_status = if resumed_status == TaskStatus::Consultation.as_str() {
            paused_status
        } else {
            None
        };
        transaction.execute(
            r#"
            UPDATE tasks
            SET status = ?1, paused_status = ?2, awaiting_human_by = NULL, awaiting_human_status = NULL
            WHERE id = ?3
            "#,
            params![resumed_status, restored_paused_status, task_id],
        )?;
        insert_event_in_transaction(
            &transaction,
            None,
            "task_human_answered",
            &serde_json::json!({
                "task_id": task_id,
                "resumed_status": resumed_status,
            }),
        )?;
        transaction.commit()?;
        Ok(TaskHumanAnswerRestore::Restored {
            status: resumed_status,
        })
    })
    .await?
}

pub async fn claim_task(
    task_id: &str,
    task_path: &str,
    agent_id: &str,
    ttl_secs: u64,
) -> Result<TaskClaim> {
    let database_path = current_database_path().await?;
    claim_task_in_database(
        database_path,
        task_id.to_string(),
        task_path.to_string(),
        agent_id,
        ttl_secs,
    )
    .await
}

#[allow(dead_code)]
pub async fn claim_next_ready_task(agent_id: &str, ttl_secs: u64) -> Result<ReadyTaskClaim> {
    claim_next_task_with_statuses(
        agent_id,
        ttl_secs,
        &[
            TaskStatus::Pending,
            TaskStatus::Executing,
            TaskStatus::Addressing,
        ],
        true,
    )
    .await
}

pub async fn claim_ready_task_by_id(
    task_id: &str,
    agent_id: &str,
    ttl_secs: u64,
) -> Result<ReadyTaskClaim> {
    claim_task_by_id_with_statuses(
        task_id,
        agent_id,
        ttl_secs,
        &[
            TaskStatus::Pending,
            TaskStatus::Executing,
            TaskStatus::Addressing,
        ],
        true,
    )
    .await
}

pub async fn claim_review_task_by_id(
    task_id: &str,
    agent_id: &str,
    ttl_secs: u64,
) -> Result<ReadyTaskClaim> {
    claim_task_by_id_with_statuses(task_id, agent_id, ttl_secs, &[TaskStatus::Reviewing], false)
        .await
}

async fn claim_task_by_id_with_statuses(
    task_id: &str,
    agent_id: &str,
    ttl_secs: u64,
    allowed_statuses: &[TaskStatus],
    promote_pending: bool,
) -> Result<ReadyTaskClaim> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let agent_id = agent_id.to_string();
    let allowed_statuses = allowed_statuses.to_vec();
    tokio::task::spawn_blocking(move || -> Result<ReadyTaskClaim> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        let now = Utc::now();
        let Some(mut candidate) = task_candidate_by_id(&transaction, &task_id)? else {
            transaction.commit()?;
            return Ok(ReadyTaskClaim::NoAvailable);
        };

        if !allowed_statuses
            .iter()
            .any(|status| status.as_str() == candidate.status)
        {
            transaction.commit()?;
            return Ok(ReadyTaskClaim::NoAvailable);
        }

        if promote_pending && candidate.status == TaskStatus::Pending.as_str() {
            promote_pending_task_in_transaction(&transaction, &mut candidate)?;
        }

        let lease_until = parse_lease_until(candidate.lease_until.as_deref());
        let lease_active = lease_until
            .as_ref()
            .is_some_and(|lease_until| now < *lease_until);
        if lease_active && candidate.claimed_by.as_deref() == Some(agent_id.as_str()) {
            transaction.commit()?;
            return Ok(ReadyTaskClaim::AlreadyClaimed(TaskLease {
                task_id: candidate.id,
                task_path: candidate.path,
                status: candidate.status,
                paused_status: candidate.paused_status,
                check_retries: candidate.check_retries,
                review_cycles: candidate.review_cycles,
                failure_reason: candidate.failure_reason,
                claimed_by: agent_id,
                lease_until: lease_until.expect("active lease exists"),
            }));
        }
        if lease_active {
            transaction.commit()?;
            return Ok(ReadyTaskClaim::NoAvailable);
        }

        let lease_until =
            now + chrono::Duration::try_seconds(ttl_secs as i64).unwrap_or(chrono::Duration::MAX);
        claim_task_in_transaction(&transaction, &candidate.id, &agent_id, lease_until, now)?;
        transaction.commit()?;
        Ok(ReadyTaskClaim::Claimed(TaskLease {
            task_id: candidate.id,
            task_path: candidate.path,
            status: candidate.status,
            paused_status: candidate.paused_status,
            check_retries: candidate.check_retries,
            review_cycles: candidate.review_cycles,
            failure_reason: candidate.failure_reason,
            claimed_by: agent_id,
            lease_until,
        }))
    })
    .await?
}

pub async fn claim_next_review_task(agent_id: &str, ttl_secs: u64) -> Result<ReadyTaskClaim> {
    claim_next_task_with_statuses(agent_id, ttl_secs, &[TaskStatus::Reviewing], false).await
}

async fn claim_next_task_with_statuses(
    agent_id: &str,
    ttl_secs: u64,
    statuses: &[TaskStatus],
    promote_pending: bool,
) -> Result<ReadyTaskClaim> {
    let database_path = current_database_path().await?;
    let agent_id = agent_id.to_string();
    let statuses = statuses.to_vec();
    tokio::task::spawn_blocking(move || -> Result<ReadyTaskClaim> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        let now = Utc::now();
        let mut candidates = task_candidates_by_status(&transaction, &statuses)?;

        for candidate in &mut candidates {
            let lease_until = parse_lease_until(candidate.lease_until.as_deref());
            let lease_active = lease_until
                .as_ref()
                .is_some_and(|lease_until| now < *lease_until);
            if lease_active && candidate.claimed_by.as_deref() == Some(agent_id.as_str()) {
                if promote_pending && candidate.status == TaskStatus::Pending.as_str() {
                    promote_pending_task_in_transaction(&transaction, candidate)?;
                }
                transaction.commit()?;
                return Ok(ReadyTaskClaim::AlreadyClaimed(TaskLease {
                    task_id: candidate.id.clone(),
                    task_path: candidate.path.clone(),
                    status: candidate.status.clone(),
                    paused_status: candidate.paused_status.clone(),
                    check_retries: candidate.check_retries,
                    review_cycles: candidate.review_cycles,
                    failure_reason: candidate.failure_reason.clone(),
                    claimed_by: agent_id,
                    lease_until: lease_until.expect("active lease exists"),
                }));
            }
        }

        for mut candidate in candidates {
            let lease_until = parse_lease_until(candidate.lease_until.as_deref());
            let lease_active = lease_until
                .as_ref()
                .is_some_and(|lease_until| now < *lease_until);
            if lease_active {
                continue;
            }

            if promote_pending && candidate.status == TaskStatus::Pending.as_str() {
                promote_pending_task_in_transaction(&transaction, &mut candidate)?;
            }

            let lease_until = now
                + chrono::Duration::try_seconds(ttl_secs as i64).unwrap_or(chrono::Duration::MAX);
            claim_task_in_transaction(&transaction, &candidate.id, &agent_id, lease_until, now)?;
            transaction.commit()?;
            return Ok(ReadyTaskClaim::Claimed(TaskLease {
                task_id: candidate.id,
                task_path: candidate.path,
                status: candidate.status,
                paused_status: candidate.paused_status,
                check_retries: candidate.check_retries,
                review_cycles: candidate.review_cycles,
                failure_reason: candidate.failure_reason,
                claimed_by: agent_id,
                lease_until,
            }));
        }

        transaction.commit()?;
        Ok(ReadyTaskClaim::NoAvailable)
    })
    .await?
}

fn promote_pending_task_in_transaction(
    transaction: &Transaction<'_>,
    candidate: &mut ReadyTaskCandidate,
) -> Result<()> {
    transaction.execute(
        "UPDATE tasks SET status = ?1, paused_status = NULL WHERE id = ?2 AND status = ?3",
        params![
            TaskStatus::Executing.as_str(),
            candidate.id,
            TaskStatus::Pending.as_str()
        ],
    )?;
    insert_event_in_transaction(
        transaction,
        None,
        "task_scheduled",
        &serde_json::json!({
            "task_id": candidate.id,
            "previous_status": candidate.status,
            "status": TaskStatus::Executing.as_str(),
            "scheduled_at": timestamp(),
        }),
    )?;
    candidate.status = TaskStatus::Executing.as_str().to_string();
    candidate.paused_status = None;
    Ok(())
}

async fn claim_task_in_database(
    database_path: PathBuf,
    task_id: String,
    task_path: String,
    agent_id: &str,
    ttl_secs: u64,
) -> Result<TaskClaim> {
    let agent_id = agent_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<TaskClaim> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        ensure_task_exists(&transaction, &task_id, &task_path)?;
        let existing: Option<(Option<String>, Option<String>)> = transaction
            .query_row(
                "SELECT claimed_by, lease_until FROM tasks WHERE id = ?1",
                [&task_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (claimed_by, lease_until) = existing.unwrap_or((None, None));
        let now = Utc::now();
        let existing_lease = lease_until
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc));
        let lease_active = existing_lease.is_some_and(|lease_until| now < lease_until);

        if lease_active && claimed_by.as_deref() == Some(agent_id.as_str()) {
            renew_task_lease_in_transaction(
                &transaction,
                &task_id,
                &agent_id,
                ttl_secs,
                lease_until.as_deref(),
            )?;
            transaction.commit()?;
            return Ok(TaskClaim::AlreadyClaimed);
        }
        if lease_active {
            transaction.commit()?;
            return Ok(TaskClaim::ClaimedByOther {
                claimed_by: claimed_by.unwrap_or_else(|| "unknown".to_string()),
            });
        }

        let lease_until =
            now + chrono::Duration::try_seconds(ttl_secs as i64).unwrap_or(chrono::Duration::MAX);
        claim_task_in_transaction(&transaction, &task_id, &agent_id, lease_until, now)?;
        transaction.commit()?;
        Ok(TaskClaim::Claimed)
    })
    .await?
}

pub async fn renew_claimed_task_lease(agent_id: &str, ttl_secs: u64) -> Result<LeaseRenewal> {
    let database_path = current_database_path().await?;
    let agent_id = agent_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<LeaseRenewal> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        let existing: Option<(String, String, Option<String>)> = transaction
            .query_row(
                r#"
                SELECT id, path, lease_until
                FROM tasks
                WHERE claimed_by = ?1
                ORDER BY
                    CASE WHEN lease_until IS NULL THEN 1 ELSE 0 END,
                    lease_until DESC,
                    id
                LIMIT 1
                "#,
                [&agent_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((task_id, task_path, lease_until)) = existing else {
            transaction.commit()?;
            return Ok(LeaseRenewal::NotClaimed);
        };

        let Some(lease_until) = renew_task_lease_in_transaction(
            &transaction,
            &task_id,
            &agent_id,
            ttl_secs,
            lease_until.as_deref(),
        )?
        else {
            transaction.commit()?;
            return Ok(LeaseRenewal::Expired);
        };
        transaction.commit()?;
        Ok(LeaseRenewal::Renewed {
            task_id,
            task_path,
            claimed_by: agent_id,
            lease_until,
        })
    })
    .await?
}

pub async fn runtime_task_context_for_agent(agent_id: &str) -> Result<Option<RuntimeTaskContext>> {
    let database_path = current_database_path().await?;
    let agent_id = agent_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<RuntimeTaskContext>> {
        let connection = open_runtime_database(&database_path)?;
        if let Some((
            task_id,
            task_path,
            spec_path,
            milestone_id,
            status,
            paused_status,
            check_retries,
            review_cycles,
            failure_reason,
        )) = connection
            .query_row(
                r#"
                SELECT id, path, spec_path, milestone_id, status, paused_status,
                       check_retries, review_cycles, failure_reason
                FROM tasks
                WHERE claimed_by = ?1
                ORDER BY
                    CASE WHEN lease_until IS NULL THEN 1 ELSE 0 END,
                    lease_until DESC,
                    id
                LIMIT 1
                "#,
                [&agent_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, i64>(6)? as u32,
                        row.get::<_, i64>(7)? as u32,
                        row.get::<_, Option<String>>(8)?,
                    ))
                },
            )
            .optional()?
        {
            let run = latest_run_for_agent_task(&connection, &agent_id, &task_id)?;
            return Ok(Some(RuntimeTaskContext {
                run_dir: run_dir_for_task(&task_id),
                task_id,
                task_path,
                spec_path,
                milestone_id,
                status,
                paused_status,
                check_retries,
                review_cycles,
                failure_reason,
                run_id: run.as_ref().map(|run| run.id.clone()),
                workspace_path: run.map(|run| run.workspace_path),
            }));
        }

        let context = connection
            .query_row(
                r#"
                SELECT runs.id, runs.workspace_path,
                       tasks.id, tasks.path, tasks.spec_path, tasks.milestone_id,
                       tasks.status, tasks.paused_status,
                       tasks.check_retries, tasks.review_cycles, tasks.failure_reason
                FROM runs
                JOIN tasks ON tasks.id = runs.task_id
                WHERE runs.agent = ?1 AND runs.status IN ('running', 'checking', 'reviewing')
                ORDER BY runs.updated_at DESC, runs.started_at DESC, runs.id DESC
                LIMIT 1
                "#,
                [&agent_id],
                |row| {
                    let run_id = row.get::<_, String>(0)?;
                    let workspace_path = row.get::<_, String>(1)?;
                    let task_id = row.get::<_, String>(2)?;
                    Ok(RuntimeTaskContext {
                        run_dir: run_dir_for_task(&task_id),
                        task_id,
                        task_path: row.get(3)?,
                        spec_path: row.get(4)?,
                        milestone_id: row.get(5)?,
                        status: row.get(6)?,
                        paused_status: row.get(7)?,
                        check_retries: row.get::<_, i64>(8)? as u32,
                        review_cycles: row.get::<_, i64>(9)? as u32,
                        failure_reason: row.get(10)?,
                        run_id: Some(run_id),
                        workspace_path: Some(workspace_path),
                    })
                },
            )
            .optional()?;
        Ok(context)
    })
    .await?
}

fn renew_task_lease_in_transaction(
    transaction: &Transaction<'_>,
    task_id: &str,
    agent_id: &str,
    ttl_secs: u64,
    existing_lease: Option<&str>,
) -> Result<Option<DateTime<Utc>>> {
    let now = Utc::now();
    let existing_lease = existing_lease
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc));
    if existing_lease.is_none_or(|lease_until| now >= lease_until) {
        return Ok(None);
    }

    let lease_until =
        now + chrono::Duration::try_seconds(ttl_secs as i64).unwrap_or(chrono::Duration::MAX);
    let lease_until_text = lease_until.to_rfc3339_opts(SecondsFormat::Secs, true);
    let now_text = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    transaction.execute(
        "UPDATE tasks SET lease_until = ?1, last_heartbeat = ?2 WHERE id = ?3",
        params![lease_until_text, now_text, task_id],
    )?;
    insert_event_in_transaction(
        transaction,
        None,
        "task_lease_renewed",
        &serde_json::json!({
            "task_id": task_id,
            "claimed_by": agent_id,
            "lease_until": lease_until,
        }),
    )?;
    Ok(Some(lease_until))
}

#[derive(Debug, Clone)]
struct RuntimeRunIdentity {
    id: String,
    workspace_path: String,
}

fn latest_run_for_agent_task(
    connection: &Connection,
    agent_id: &str,
    task_id: &str,
) -> Result<Option<RuntimeRunIdentity>> {
    Ok(connection
        .query_row(
            r#"
            SELECT id, workspace_path
            FROM runs
            WHERE agent = ?1 AND task_id = ?2
            ORDER BY updated_at DESC, started_at DESC, id DESC
            LIMIT 1
            "#,
            params![agent_id, task_id],
            |row| {
                Ok(RuntimeRunIdentity {
                    id: row.get(0)?,
                    workspace_path: row.get(1)?,
                })
            },
        )
        .optional()?)
}

fn latest_active_run_for_agent(connection: &Connection, agent_id: &str) -> Result<Option<String>> {
    Ok(connection
        .query_row(
            r#"
            SELECT id
            FROM runs
            WHERE agent = ?1 AND status IN ('running', 'checking', 'reviewing')
            ORDER BY updated_at DESC, started_at DESC, id DESC
            LIMIT 1
            "#,
            [agent_id],
            |row| row.get(0),
        )
        .optional()?)
}

fn consultation_context_for_run(
    connection: &Connection,
    run_id: &str,
) -> Result<Option<RuntimeTaskContext>> {
    Ok(connection
        .query_row(
            r#"
            SELECT tasks.id, tasks.path, tasks.spec_path, tasks.milestone_id,
                   tasks.status, tasks.paused_status,
                   tasks.check_retries, tasks.review_cycles, tasks.failure_reason
            FROM runs
            JOIN tasks ON tasks.id = runs.task_id
            WHERE runs.id = ?1 AND tasks.status = ?2
            LIMIT 1
            "#,
            params![run_id, TaskStatus::Consultation.as_str()],
            |row| {
                let task_id = row.get::<_, String>(0)?;
                Ok(RuntimeTaskContext {
                    run_dir: run_dir_for_task(&task_id),
                    task_id,
                    task_path: row.get(1)?,
                    spec_path: row.get(2)?,
                    milestone_id: row.get(3)?,
                    status: row.get(4)?,
                    paused_status: row.get(5)?,
                    check_retries: row.get::<_, i64>(6)? as u32,
                    review_cycles: row.get::<_, i64>(7)? as u32,
                    failure_reason: row.get(8)?,
                    run_id: Some(run_id.to_string()),
                    workspace_path: None,
                })
            },
        )
        .optional()?)
}

fn run_dir_for_task(task_id: &str) -> String {
    format!(".ferrus/runs/{task_id}")
}

fn default_task_path_for_id(task_id: &str) -> String {
    if task_id == CURRENT_TASK_ID {
        CURRENT_TASK_PATH.to_string()
    } else {
        format!(".ferrus/tasks/{task_id}.md")
    }
}

pub async fn record_runtime_event(
    run_id: Option<String>,
    event_type: &str,
    payload: serde_json::Value,
) -> Result<()> {
    let database_path = current_database_path().await?;
    let event_type = event_type.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        insert_event(&connection, run_id.as_deref(), &event_type, &payload)?;
        Ok(())
    })
    .await?
}

pub async fn record_runtime_event_best_effort(
    run_id: Option<String>,
    event_type: &str,
    payload: serde_json::Value,
) {
    if let Err(err) = record_runtime_event(run_id, event_type, payload).await {
        warn!(error = ?err, event_type, "failed to write runtime event into ferrus.db");
    }
}

pub fn allocate_run_id(role: &str, agent: &str) -> String {
    generate_run_id(role, agent)
}

#[cfg(test)]
pub async fn record_run_started(role: &str, agent: &str, pid: u32) -> Result<RunRecord> {
    let run_id = allocate_run_id(role, agent);
    record_run_started_with_id(&run_id, role, agent, pid).await
}

#[cfg(test)]
pub async fn record_run_started_with_id(
    run_id: &str,
    role: &str,
    agent: &str,
    pid: u32,
) -> Result<RunRecord> {
    let workspace_path = path_string(&canonical_current_dir().await?);
    record_run_started_with_workspace(run_id, role, agent, pid, workspace_path).await
}

#[cfg(test)]
pub async fn record_run_started_with_workspace(
    run_id: &str,
    role: &str,
    agent: &str,
    pid: u32,
    workspace_path: String,
) -> Result<RunRecord> {
    record_run_started_for_task_with_workspace(run_id, role, agent, pid, None, workspace_path).await
}

pub async fn record_run_started_for_task_with_workspace(
    run_id: &str,
    role: &str,
    agent: &str,
    pid: u32,
    task_id: Option<&str>,
    workspace_path: String,
) -> Result<RunRecord> {
    let database_path = current_database_path().await?;
    let (task_id, task_path) = match task_id.map(str::trim).filter(|task_id| !task_id.is_empty()) {
        Some(task_id) => (task_id.to_string(), default_task_path_for_id(task_id)),
        None => current_task_identity().await,
    };
    let run_id = run_id.to_string();
    let role = role.to_string();
    let agent = agent.to_string();
    let started_at = timestamp();
    let updated_at = started_at.clone();
    let record = RunRecord {
        id: run_id.clone(),
        task_id: task_id.clone(),
        role,
        agent,
        status: "running".to_string(),
        started_at: started_at.clone(),
        updated_at: updated_at.clone(),
        pid: Some(pid),
        workspace_path,
    };
    let record_for_insert = record.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        ensure_task_exists(&connection, &task_id, &task_path)?;
        connection.execute(
            r#"
            INSERT INTO runs (
                id, task_id, role, agent, status, started_at, updated_at, pid, workspace_path
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            ON CONFLICT(id) DO UPDATE SET
                status = excluded.status,
                updated_at = excluded.updated_at,
                pid = excluded.pid,
                workspace_path = excluded.workspace_path
            "#,
            params![
                record_for_insert.id,
                record_for_insert.task_id,
                record_for_insert.role,
                record_for_insert.agent,
                record_for_insert.status,
                started_at,
                updated_at,
                record_for_insert.pid.map(i64::from),
                record_for_insert.workspace_path,
            ],
        )?;
        insert_event(
            &connection,
            Some(&run_id),
            "run_started",
            &serde_json::json!({
                "role": record_for_insert.role,
                "agent": record_for_insert.agent,
                "pid": record_for_insert.pid,
            }),
        )?;
        Ok(())
    })
    .await??;
    Ok(record)
}

pub async fn record_run_started_for_task_with_id_best_effort(
    run_id: &str,
    role: &str,
    agent: &str,
    pid: u32,
    task_id: Option<&str>,
    workspace_path: String,
) -> Option<String> {
    match record_run_started_for_task_with_workspace(
        run_id,
        role,
        agent,
        pid,
        task_id,
        workspace_path,
    )
    .await
    {
        Ok(record) => Some(record.id),
        Err(err) => {
            warn!(error = ?err, run_id, role, agent, pid, task_id, "failed to mirror task-scoped run start into ferrus.db");
            None
        }
    }
}

pub async fn attach_running_run_to_task(
    agent_id: &str,
    task_id: &str,
    task_path: &str,
) -> Result<Option<String>> {
    let database_path = current_database_path().await?;
    let agent_id = agent_id.to_string();
    let task_id = task_id.to_string();
    let task_path = task_path.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let connection = open_runtime_database(&database_path)?;
        ensure_task_exists(&connection, &task_id, &task_path)?;
        let run_id: Option<String> = connection
            .query_row(
                r#"
                SELECT id
                FROM runs
                WHERE agent = ?1 AND status IN ('running', 'checking', 'reviewing')
                ORDER BY started_at DESC, id DESC
                LIMIT 1
                "#,
                [&agent_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(run_id) = run_id else {
            return Ok(None);
        };

        connection.execute(
            "UPDATE runs SET task_id = ?1, updated_at = ?2 WHERE id = ?3",
            params![task_id, timestamp(), run_id],
        )?;
        insert_event(
            &connection,
            Some(&run_id),
            "run_task_attached",
            &serde_json::json!({
                "agent": agent_id,
                "task_id": task_id,
            }),
        )?;
        Ok(Some(run_id))
    })
    .await?
}

pub async fn attach_running_run_to_task_best_effort(
    agent_id: &str,
    task_id: &str,
    task_path: &str,
) {
    if let Err(err) = attach_running_run_to_task(agent_id, task_id, task_path).await {
        warn!(
            error = ?err,
            agent_id,
            task_id,
            "failed to attach running run to task in ferrus.db"
        );
    }
}

pub async fn attach_running_run_to_next_consultation(
    agent_id: &str,
) -> Result<Option<RuntimeTaskContext>> {
    let database_path = current_database_path().await?;
    let agent_id = agent_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<RuntimeTaskContext>> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(run_id) = latest_active_run_for_agent(&transaction, &agent_id)? else {
            transaction.commit()?;
            return Ok(None);
        };

        if let Some(context) = consultation_context_for_run(&transaction, &run_id)? {
            transaction.commit()?;
            return Ok(Some(context));
        }

        let candidate = transaction
            .query_row(
                r#"
                SELECT id, path, spec_path, milestone_id, status, paused_status,
                       check_retries, review_cycles, failure_reason
                FROM tasks
                WHERE status = ?1
                  AND NOT EXISTS (
                      SELECT 1
                      FROM runs
                      WHERE runs.task_id = tasks.id
                        AND runs.role = 'supervisor'
                        AND runs.status IN ('running', 'checking', 'reviewing')
                  )
                ORDER BY id
                LIMIT 1
                "#,
                [TaskStatus::Consultation.as_str()],
                |row| {
                    Ok(RuntimeTaskContext {
                        task_id: row.get(0)?,
                        task_path: row.get(1)?,
                        spec_path: row.get(2)?,
                        milestone_id: row.get(3)?,
                        run_dir: String::new(),
                        status: row.get(4)?,
                        paused_status: row.get(5)?,
                        check_retries: row.get::<_, i64>(6)? as u32,
                        review_cycles: row.get::<_, i64>(7)? as u32,
                        failure_reason: row.get(8)?,
                        run_id: Some(run_id.clone()),
                        workspace_path: None,
                    })
                },
            )
            .optional()?;
        let Some(mut context) = candidate else {
            transaction.commit()?;
            return Ok(None);
        };

        context.run_dir = run_dir_for_task(&context.task_id);
        let attached = transaction.execute(
            r#"
            UPDATE runs
            SET task_id = ?1, updated_at = ?2
            WHERE id = ?3
              AND NOT EXISTS (
                  SELECT 1
                  FROM runs active_runs
                  WHERE active_runs.task_id = ?1
                    AND active_runs.role = 'supervisor'
                    AND active_runs.status IN ('running', 'checking', 'reviewing')
                    AND active_runs.id <> ?3
              )
            "#,
            params![context.task_id, timestamp(), run_id],
        )?;
        if attached == 0 {
            transaction.commit()?;
            return Ok(None);
        }
        insert_event_in_transaction(
            &transaction,
            Some(&run_id),
            "run_consultation_attached",
            &serde_json::json!({
                "agent": agent_id,
                "task_id": context.task_id,
            }),
        )?;
        transaction.commit()?;
        Ok(Some(context))
    })
    .await?
}

pub async fn attach_running_run_to_consultation(
    task_id: &str,
    agent_id: &str,
) -> Result<Option<RuntimeTaskContext>> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let agent_id = agent_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<RuntimeTaskContext>> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(run_id) = latest_active_run_for_agent(&transaction, &agent_id)? else {
            transaction.commit()?;
            return Ok(None);
        };

        if let Some(context) = consultation_context_for_run(&transaction, &run_id)? {
            transaction.commit()?;
            return Ok((context.task_id == task_id).then_some(context));
        }

        let candidate = transaction
            .query_row(
                r#"
                SELECT id, path, spec_path, milestone_id, status, paused_status,
                       check_retries, review_cycles, failure_reason
                FROM tasks
                WHERE id = ?1
                  AND status = ?2
                  AND NOT EXISTS (
                      SELECT 1
                      FROM runs
                      WHERE runs.task_id = tasks.id
                        AND runs.role = 'supervisor'
                        AND runs.status IN ('running', 'checking', 'reviewing')
                  )
                LIMIT 1
                "#,
                params![task_id, TaskStatus::Consultation.as_str()],
                |row| {
                    Ok(RuntimeTaskContext {
                        task_id: row.get(0)?,
                        task_path: row.get(1)?,
                        spec_path: row.get(2)?,
                        milestone_id: row.get(3)?,
                        run_dir: String::new(),
                        status: row.get(4)?,
                        paused_status: row.get(5)?,
                        check_retries: row.get::<_, i64>(6)? as u32,
                        review_cycles: row.get::<_, i64>(7)? as u32,
                        failure_reason: row.get(8)?,
                        run_id: Some(run_id.clone()),
                        workspace_path: None,
                    })
                },
            )
            .optional()?;
        let Some(mut context) = candidate else {
            transaction.commit()?;
            return Ok(None);
        };

        context.run_dir = run_dir_for_task(&context.task_id);
        let attached = transaction.execute(
            r#"
            UPDATE runs
            SET task_id = ?1, updated_at = ?2
            WHERE id = ?3
              AND NOT EXISTS (
                  SELECT 1
                  FROM runs active_runs
                  WHERE active_runs.task_id = ?1
                    AND active_runs.role = 'supervisor'
                    AND active_runs.status IN ('running', 'checking', 'reviewing')
                    AND active_runs.id <> ?3
              )
            "#,
            params![context.task_id, timestamp(), run_id],
        )?;
        if attached == 0 {
            transaction.commit()?;
            return Ok(None);
        }
        insert_event_in_transaction(
            &transaction,
            Some(&run_id),
            "run_consultation_attached",
            &serde_json::json!({
                "agent": agent_id,
                "task_id": context.task_id,
            }),
        )?;
        transaction.commit()?;
        Ok(Some(context))
    })
    .await?
}

pub async fn record_run_finished(run_id: &str, exit_code: i32) -> Result<()> {
    let database_path = current_database_path().await?;
    let run_id = run_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        let status = if exit_code == 0 {
            "completed"
        } else {
            "failed"
        };
        connection.execute(
            "UPDATE runs SET status = ?1, updated_at = ?2, pid = NULL WHERE id = ?3",
            params![status, timestamp(), run_id],
        )?;
        insert_event(
            &connection,
            Some(&run_id),
            "run_finished",
            &serde_json::json!({
                "exit_code": exit_code,
                "status": status,
            }),
        )?;
        Ok(())
    })
    .await?
}

pub async fn record_run_finished_best_effort(run_id: &str, exit_code: i32) {
    if let Err(err) = record_run_finished(run_id, exit_code).await {
        warn!(error = ?err, run_id, exit_code, "failed to mirror run finish into ferrus.db");
    }
}

pub async fn recover_interrupted_runs() -> Result<usize> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<usize> {
        let connection = open_runtime_database(&database_path)?;
        let mut statement = connection.prepare(
            "SELECT id, pid FROM runs WHERE status IN ('running', 'checking', 'reviewing')",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
        })?;

        let mut interrupted = Vec::new();
        for row in rows {
            let (run_id, pid) = row?;
            if pid.is_none_or(|pid| !process_is_alive(pid as u32)) {
                interrupted.push(run_id);
            }
        }

        for run_id in &interrupted {
            connection.execute(
                "UPDATE runs SET status = 'interrupted', updated_at = ?1, pid = NULL WHERE id = ?2",
                params![timestamp(), run_id],
            )?;
            insert_event(
                &connection,
                Some(run_id),
                "run_interrupted",
                &serde_json::json!({}),
            )?;
        }

        Ok(interrupted.len())
    })
    .await?
}

pub async fn recover_expired_task_leases() -> Result<usize> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<usize> {
        let connection = open_runtime_database(&database_path)?;
        let now = Utc::now();
        let mut statement = connection.prepare(
            "SELECT id, claimed_by, lease_until FROM tasks WHERE claimed_by IS NOT NULL",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;

        let mut expired = Vec::new();
        for row in rows {
            let (task_id, claimed_by, lease_until) = row?;
            let parsed_lease = lease_until
                .as_deref()
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc));
            if parsed_lease.is_none_or(|lease_until| now >= lease_until) {
                expired.push((task_id, claimed_by, lease_until));
            }
        }

        for (task_id, claimed_by, lease_until) in &expired {
            clear_task_lease(&connection, task_id)?;
            insert_event(
                &connection,
                None,
                "task_lease_expired",
                &serde_json::json!({
                    "task_id": task_id,
                    "claimed_by": claimed_by,
                    "lease_until": lease_until,
                }),
            )?;
        }

        Ok(expired.len())
    })
    .await?
}

pub async fn recover_runtime_state() -> Result<RuntimeRecovery> {
    let interrupted_runs = recover_interrupted_runs().await?;
    let expired_task_leases = recover_expired_task_leases().await?;
    Ok(RuntimeRecovery {
        interrupted_runs,
        expired_task_leases,
    })
}

pub async fn preview_runtime_recovery() -> Result<RuntimeRecovery> {
    let database_path = current_database_path().await?;
    preview_runtime_recovery_from(&database_path).await
}

pub async fn preview_orphaned_worktrees() -> Result<usize> {
    Ok(orphaned_worktrees().await?.len())
}

pub async fn recover_orphaned_worktrees() -> Result<usize> {
    let registration = touch_current_project().await?;
    let project_root = PathBuf::from(&registration.metadata.workspace_dir);
    let worktrees = orphaned_worktrees_for(&registration).await?;
    let mut removed = 0usize;
    for worktree in worktrees {
        let output = Command::new("git")
            .arg("-C")
            .arg(&project_root)
            .args(["worktree", "remove", "--force"])
            .arg(&worktree)
            .output()
            .await
            .with_context(|| {
                format!(
                    "Failed to run git worktree remove for {}",
                    worktree.display()
                )
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            anyhow::bail!(
                "Failed to remove orphaned worktree at {}: {}",
                worktree.display(),
                if stderr.is_empty() {
                    output.status.to_string()
                } else {
                    stderr
                }
            );
        }
        removed += 1;
    }
    Ok(removed)
}

async fn orphaned_worktrees() -> Result<Vec<PathBuf>> {
    let registration = touch_current_project().await?;
    orphaned_worktrees_for(&registration).await
}

async fn orphaned_worktrees_for(registration: &ProjectRegistration) -> Result<Vec<PathBuf>> {
    let worktrees_dir = registration.data_dir.join("worktrees");
    if !tokio::fs::try_exists(&worktrees_dir).await? {
        return Ok(Vec::new());
    }

    let protected_task_ids = protected_worktree_task_ids(&registration.database_path).await?;
    let protected_paths = protected_worktree_paths(&registration.database_path).await?;
    let mut entries = tokio::fs::read_dir(&worktrees_dir)
        .await
        .with_context(|| format!("Failed to read {}", worktrees_dir.display()))?;
    let mut orphaned = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !entry.file_type().await?.is_dir() {
            continue;
        }
        let file_name = entry.file_name();
        if file_name.to_string_lossy() == BASELINE_WORKTREE_METADATA_DIR {
            continue;
        }
        let task_id = file_name.to_string_lossy().to_string();
        let canonical_path = tokio::fs::canonicalize(&path)
            .await
            .unwrap_or_else(|_| path.clone());
        if protected_task_ids.contains(&task_id) || protected_paths.contains(&canonical_path) {
            continue;
        }
        orphaned.push(path);
    }
    orphaned.sort();
    Ok(orphaned)
}

async fn protected_worktree_task_ids(
    database_path: &Path,
) -> Result<std::collections::HashSet<String>> {
    let database_path = database_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<std::collections::HashSet<String>> {
        let connection = open_runtime_database(&database_path)?;
        let mut statement =
            connection.prepare("SELECT id FROM tasks WHERE status NOT IN (?1, ?2, ?3)")?;
        let rows = statement.query_map(
            params![
                TaskStatus::Reset.as_str(),
                TaskStatus::Complete.as_str(),
                TaskStatus::Failed.as_str()
            ],
            |row| row.get::<_, String>(0),
        )?;
        let mut task_ids = std::collections::HashSet::new();
        for row in rows {
            task_ids.insert(row?);
        }
        Ok(task_ids)
    })
    .await?
}

async fn protected_worktree_paths(
    database_path: &Path,
) -> Result<std::collections::HashSet<PathBuf>> {
    let database_path = database_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<std::collections::HashSet<PathBuf>> {
        let connection = open_runtime_database(&database_path)?;
        let mut statement = connection.prepare(
            "SELECT workspace_path, pid FROM runs WHERE status IN ('running', 'checking', 'reviewing')",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
        })?;
        let mut paths = std::collections::HashSet::new();
        for row in rows {
            let (workspace_path, pid) = row?;
            if pid.is_none_or(|pid| !process_is_alive(pid as u32)) {
                continue;
            }
            let path = PathBuf::from(workspace_path);
            paths.insert(std::fs::canonicalize(&path).unwrap_or(path));
        }
        Ok(paths)
    })
    .await?
}

async fn preview_runtime_recovery_from(database_path: &Path) -> Result<RuntimeRecovery> {
    Ok(RuntimeRecovery {
        interrupted_runs: preview_interrupted_runs(database_path).await?,
        expired_task_leases: preview_expired_task_leases(database_path).await?,
    })
}

async fn preview_interrupted_runs(database_path: &Path) -> Result<usize> {
    let database_path = database_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<usize> {
        let connection =
            Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("Failed to open {}", database_path.display()))?;
        let mut statement = connection
            .prepare("SELECT pid FROM runs WHERE status IN ('running', 'checking', 'reviewing')")?;
        let rows = statement.query_map([], |row| row.get::<_, Option<i64>>(0))?;

        let mut interrupted = 0;
        for row in rows {
            if row?.is_none_or(|pid| !process_is_alive(pid as u32)) {
                interrupted += 1;
            }
        }
        Ok(interrupted)
    })
    .await?
}

async fn preview_expired_task_leases(database_path: &Path) -> Result<usize> {
    let database_path = database_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<usize> {
        let connection =
            Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("Failed to open {}", database_path.display()))?;
        let now = Utc::now();
        let mut statement =
            connection.prepare("SELECT lease_until FROM tasks WHERE claimed_by IS NOT NULL")?;
        let rows = statement.query_map([], |row| row.get::<_, Option<String>>(0))?;

        let mut expired = 0;
        for row in rows {
            let parsed_lease = row?
                .as_deref()
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc));
            if parsed_lease.is_none_or(|lease_until| now >= lease_until) {
                expired += 1;
            }
        }
        Ok(expired)
    })
    .await?
}

async fn initialize_database(path: &Path) -> Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = Connection::open(&path)
            .with_context(|| format!("Failed to open {}", path.display()))?;
        initialize_schema(&connection)?;
        Ok(())
    })
    .await?
}

async fn validate_database_schema(path: &Path) -> Result<bool> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<bool> {
        let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("Failed to open {}", path.display()))?;
        for table in ["tasks", "runs", "events"] {
            let exists: i64 = connection.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )?;
            if exists == 0 {
                return Ok(false);
            }
        }
        for column in [
            "paused_status",
            "claimed_by",
            "lease_until",
            "last_heartbeat",
            "check_retries",
            "review_cycles",
            "failure_reason",
            "awaiting_human_by",
            "awaiting_human_status",
        ] {
            if !column_exists(&connection, "tasks", column)? {
                return Ok(false);
            }
        }
        Ok(true)
    })
    .await?
}

async fn max_task_number_from_files(tasks_dir: &Path) -> Result<u32> {
    let mut max_number = 0;
    let mut entries = tokio::fs::read_dir(tasks_dir)
        .await
        .with_context(|| format!("Failed to read {}", tasks_dir.display()))?;
    while let Some(entry) = entries.next_entry().await? {
        let Some(file_name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if let Some(number) = parse_task_number(file_name.strip_suffix(".md").unwrap_or(&file_name))
        {
            max_number = max_number.max(number);
        }
    }
    Ok(max_number)
}

async fn max_task_number_from_database(path: &Path) -> Result<u32> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<u32> {
        let connection = open_runtime_database(&path)?;
        let mut statement = connection.prepare("SELECT id FROM tasks WHERE id LIKE 't-%'")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut max_number = 0;
        for row in rows {
            if let Some(number) = parse_task_number(&row?) {
                max_number = max_number.max(number);
            }
        }
        Ok(max_number)
    })
    .await?
}

async fn read_task_records_from_database(path: &Path) -> Result<Vec<TaskRecord>> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<Vec<TaskRecord>> {
        let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("Failed to open {}", path.display()))?;
        let mut statement = connection.prepare(
            r#"
            SELECT id, path, spec_path, milestone_id, status, paused_status, claimed_by,
                   lease_until, last_heartbeat, check_retries, review_cycles, failure_reason
            FROM tasks
            ORDER BY id
            "#,
        )?;
        let rows = statement.query_map([], task_record_from_row)?;
        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row?);
        }
        Ok(tasks)
    })
    .await?
}

async fn current_database_path() -> Result<PathBuf> {
    let local_ref = read_local_project_ref()
        .await
        .context(".ferrus/project.toml not found — run `ferrus migrate`")?;
    Ok(PathBuf::from(local_ref.data_dir).join("ferrus.db"))
}

async fn current_task_record() -> CurrentTaskRecord {
    CurrentTaskRecord {
        id: CURRENT_TASK_ID.to_string(),
        path: CURRENT_TASK_PATH.to_string(),
        #[cfg(test)]
        spec_path: None,
        #[cfg(test)]
        milestone_id: None,
    }
}

async fn current_task_identity() -> (String, String) {
    let task = current_task_record().await;
    (task.id, task.path)
}

fn open_runtime_database(path: &Path) -> Result<Connection> {
    let connection =
        Connection::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    initialize_schema(&connection)?;
    Ok(connection)
}

fn initialize_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        r#"
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS tasks (
            id TEXT PRIMARY KEY,
            path TEXT NOT NULL,
            status TEXT NOT NULL,
            paused_status TEXT,
            spec_path TEXT,
            milestone_id TEXT,
            claimed_by TEXT,
            lease_until TEXT,
            last_heartbeat TEXT,
            check_retries INTEGER NOT NULL DEFAULT 0,
            review_cycles INTEGER NOT NULL DEFAULT 0,
            failure_reason TEXT,
            awaiting_human_by TEXT,
            awaiting_human_status TEXT
        );

        CREATE TABLE IF NOT EXISTS runs (
            id TEXT PRIMARY KEY,
            task_id TEXT NOT NULL,
            role TEXT NOT NULL,
            agent TEXT NOT NULL,
            status TEXT NOT NULL,
            started_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            pid INTEGER,
            workspace_path TEXT NOT NULL,
            FOREIGN KEY(task_id) REFERENCES tasks(id)
        );

        CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id TEXT,
            type TEXT NOT NULL,
            payload_json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            FOREIGN KEY(run_id) REFERENCES runs(id)
        );

        CREATE TABLE IF NOT EXISTS project_runtime_state (
            row_id INTEGER PRIMARY KEY CHECK (row_id = 1),
            selected_spec TEXT,
            last_spec_path TEXT,
            updated_at TEXT NOT NULL
        );
        "#,
    )?;
    ensure_column(connection, "tasks", "paused_status", "TEXT")?;
    ensure_column(connection, "tasks", "spec_path", "TEXT")?;
    ensure_column(connection, "tasks", "milestone_id", "TEXT")?;
    ensure_column(connection, "tasks", "claimed_by", "TEXT")?;
    ensure_column(connection, "tasks", "lease_until", "TEXT")?;
    ensure_column(connection, "tasks", "last_heartbeat", "TEXT")?;
    ensure_column(
        connection,
        "tasks",
        "check_retries",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        connection,
        "tasks",
        "review_cycles",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(connection, "tasks", "failure_reason", "TEXT")?;
    ensure_column(connection, "tasks", "awaiting_human_by", "TEXT")?;
    ensure_column(connection, "tasks", "awaiting_human_status", "TEXT")?;
    ensure_column(connection, "project_runtime_state", "selected_spec", "TEXT")?;
    ensure_column(
        connection,
        "project_runtime_state",
        "last_spec_path",
        "TEXT",
    )?;
    migrate_legacy_runtime_metadata(connection)?;
    Ok(())
}

fn upsert_task(
    connection: &Connection,
    id: &str,
    path: &str,
    status: TaskStatus,
    spec_path: Option<&str>,
    milestone_id: Option<&str>,
) -> Result<()> {
    connection.execute(
        r#"
        INSERT INTO tasks (id, path, status, spec_path, milestone_id)
        VALUES (?1, ?2, ?3, ?4, ?5)
        ON CONFLICT(id) DO UPDATE SET
            path = excluded.path,
            status = excluded.status,
            spec_path = COALESCE(excluded.spec_path, tasks.spec_path),
            milestone_id = COALESCE(excluded.milestone_id, tasks.milestone_id)
        "#,
        params![id, path, status.as_str(), spec_path, milestone_id],
    )?;
    Ok(())
}

fn ensure_task_exists(connection: &Connection, id: &str, path: &str) -> Result<()> {
    connection.execute(
        "INSERT OR IGNORE INTO tasks (id, path, status) VALUES (?1, ?2, ?3)",
        params![id, path, TaskStatus::Unknown.as_str()],
    )?;
    Ok(())
}

fn read_project_selection_from_database(connection: &Connection) -> Result<ProjectSelection> {
    let selection = connection
        .query_row(
            r#"
            SELECT selected_spec
            FROM project_runtime_state
            WHERE row_id = 1
            "#,
            [],
            |row| {
                Ok(ProjectSelection {
                    selected_spec: normalize_optional_db_string(row.get(0)?),
                })
            },
        )
        .optional()?
        .unwrap_or_default();
    Ok(selection)
}

fn write_project_selection_to_database(
    connection: &Connection,
    selection: &ProjectSelection,
) -> Result<()> {
    ensure_project_runtime_state_row(connection)?;
    connection.execute(
        r#"
        UPDATE project_runtime_state
        SET selected_spec = ?1, updated_at = ?2
        WHERE row_id = 1
        "#,
        params![
            normalized_metadata_value(selection.selected_spec.as_deref()),
            timestamp()
        ],
    )?;
    Ok(())
}

fn read_last_spec_path_from_database(connection: &Connection) -> Result<Option<String>> {
    let value = connection
        .query_row(
            "SELECT last_spec_path FROM project_runtime_state WHERE row_id = 1",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    Ok(normalize_optional_db_string(value))
}

fn write_last_spec_path_to_database(connection: &Connection, path: Option<&str>) -> Result<()> {
    ensure_project_runtime_state_row(connection)?;
    connection.execute(
        r#"
        UPDATE project_runtime_state
        SET last_spec_path = ?1, updated_at = ?2
        WHERE row_id = 1
        "#,
        params![normalized_metadata_value(path), timestamp()],
    )?;
    Ok(())
}

fn ensure_project_runtime_state_row(connection: &Connection) -> Result<()> {
    connection.execute(
        r#"
        INSERT INTO project_runtime_state (row_id, updated_at)
        VALUES (1, ?1)
        ON CONFLICT(row_id) DO NOTHING
        "#,
        [timestamp()],
    )?;
    Ok(())
}

fn migrate_legacy_runtime_metadata(connection: &Connection) -> Result<()> {
    if !table_exists(connection, "runtime_metadata")? {
        return Ok(());
    }

    let current_selection = read_project_selection_from_database(connection)?;
    let current_last_spec_path = read_last_spec_path_from_database(connection)?;
    let selected_spec = current_selection
        .selected_spec
        .or(read_legacy_runtime_metadata(connection, "selected_spec")?);
    let last_spec_path =
        current_last_spec_path.or(read_legacy_runtime_metadata(connection, "last_spec_path")?);

    if selected_spec.is_none() && last_spec_path.is_none() {
        return Ok(());
    }

    ensure_project_runtime_state_row(connection)?;
    connection.execute(
        r#"
        UPDATE project_runtime_state
        SET selected_spec = ?1,
            last_spec_path = ?2,
            updated_at = ?3
        WHERE row_id = 1
        "#,
        params![selected_spec, last_spec_path, timestamp()],
    )?;
    Ok(())
}

fn read_legacy_runtime_metadata(
    connection: &Connection,
    metadata_name: &str,
) -> Result<Option<String>> {
    let value = connection
        .query_row(
            "SELECT value FROM runtime_metadata WHERE key = ?1",
            [metadata_name],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    Ok(normalize_optional_db_string(value))
}

fn table_exists(connection: &Connection, table_name: &str) -> Result<bool> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1 LIMIT 1",
            [table_name],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    Ok(exists)
}

fn normalized_metadata_value(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn normalize_optional_db_string(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn task_check_retries(connection: &Connection, task_id: &str) -> Result<u32> {
    let retries = connection
        .query_row(
            "SELECT check_retries FROM tasks WHERE id = ?1",
            [task_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0);
    Ok(retries as u32)
}

fn task_review_cycles(connection: &Connection, task_id: &str) -> Result<u32> {
    let cycles = connection
        .query_row(
            "SELECT review_cycles FROM tasks WHERE id = ?1",
            [task_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0);
    Ok(cycles as u32)
}

struct ReadyTaskCandidate {
    id: String,
    path: String,
    status: String,
    paused_status: Option<String>,
    check_retries: u32,
    review_cycles: u32,
    failure_reason: Option<String>,
    claimed_by: Option<String>,
    lease_until: Option<String>,
}

fn task_candidates_by_status(
    transaction: &Transaction<'_>,
    statuses: &[TaskStatus],
) -> Result<Vec<ReadyTaskCandidate>> {
    if statuses.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = std::iter::repeat_n("?", statuses.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        r#"
        SELECT id, path, status, paused_status, check_retries, review_cycles, failure_reason,
               claimed_by, lease_until
        FROM tasks
        WHERE status IN ({placeholders})
        ORDER BY id
        "#
    );
    let mut statement = transaction.prepare(&sql)?;
    let rows = statement.query_map(
        rusqlite::params_from_iter(statuses.iter().map(|status| status.as_str())),
        |row| {
            Ok(ReadyTaskCandidate {
                id: row.get(0)?,
                path: row.get(1)?,
                status: row.get(2)?,
                paused_status: row.get(3)?,
                check_retries: row.get::<_, i64>(4)? as u32,
                review_cycles: row.get::<_, i64>(5)? as u32,
                failure_reason: row.get(6)?,
                claimed_by: row.get(7)?,
                lease_until: row.get(8)?,
            })
        },
    )?;

    let mut tasks = Vec::new();
    for row in rows {
        tasks.push(row?);
    }
    Ok(tasks)
}

fn task_candidate_by_id(
    transaction: &Transaction<'_>,
    task_id: &str,
) -> Result<Option<ReadyTaskCandidate>> {
    let task = transaction
        .query_row(
            r#"
            SELECT id, path, status, paused_status, check_retries, review_cycles, failure_reason,
                   claimed_by, lease_until
            FROM tasks
            WHERE id = ?1
            LIMIT 1
            "#,
            [task_id],
            |row| {
                Ok(ReadyTaskCandidate {
                    id: row.get(0)?,
                    path: row.get(1)?,
                    status: row.get(2)?,
                    paused_status: row.get(3)?,
                    check_retries: row.get::<_, i64>(4)? as u32,
                    review_cycles: row.get::<_, i64>(5)? as u32,
                    failure_reason: row.get(6)?,
                    claimed_by: row.get(7)?,
                    lease_until: row.get(8)?,
                })
            },
        )
        .optional()?;
    Ok(task)
}

fn claim_task_in_transaction(
    transaction: &Transaction<'_>,
    task_id: &str,
    agent_id: &str,
    lease_until: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<()> {
    let lease_until_text = lease_until.to_rfc3339_opts(SecondsFormat::Secs, true);
    let now_text = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    transaction.execute(
        "UPDATE tasks SET claimed_by = ?1, lease_until = ?2, last_heartbeat = ?3 WHERE id = ?4",
        params![agent_id, lease_until_text, now_text, task_id],
    )?;
    insert_event_in_transaction(
        transaction,
        None,
        "task_claimed",
        &serde_json::json!({
            "task_id": task_id,
            "claimed_by": agent_id,
            "lease_until": lease_until,
        }),
    )?;
    Ok(())
}

fn parse_lease_until(value: Option<&str>) -> Option<DateTime<Utc>> {
    value
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn clear_task_lease(connection: &Connection, task_id: &str) -> Result<()> {
    connection.execute(
        "UPDATE tasks SET claimed_by = NULL, lease_until = NULL, last_heartbeat = NULL WHERE id = ?1",
        [task_id],
    )?;
    Ok(())
}

fn insert_event(
    connection: &Connection,
    run_id: Option<&str>,
    event_type: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    if let Some(run_id) = run_id {
        let exists = connection
            .query_row("SELECT 1 FROM runs WHERE id = ?1", [run_id], |_| Ok(()))
            .optional()?
            .is_some();
        if !exists {
            anyhow::bail!("Cannot insert event for unknown run id {run_id}");
        }
    }
    connection.execute(
        "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![
            run_id,
            event_type,
            serde_json::to_string(payload)?,
            timestamp()
        ],
    )?;
    Ok(())
}

fn insert_event_in_transaction(
    transaction: &Transaction<'_>,
    run_id: Option<&str>,
    event_type: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    transaction.execute(
        "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![
            run_id,
            event_type,
            serde_json::to_string(payload)?,
            timestamp()
        ],
    )?;
    Ok(())
}

fn ensure_column(
    connection: &Connection,
    table_name: &str,
    column_name: &str,
    column_type: &str,
) -> Result<()> {
    if column_exists(connection, table_name, column_name)? {
        return Ok(());
    }
    connection.execute(
        &format!("ALTER TABLE {table_name} ADD COLUMN {column_name} {column_type}"),
        [],
    )?;
    Ok(())
}

fn column_exists(connection: &Connection, table_name: &str, column_name: &str) -> Result<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table_name})"))?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    for column in columns {
        if column? == column_name {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn copy_legacy_artifacts() -> Result<()> {
    copy_if_nonempty(".ferrus/TASK.md", ".ferrus/tasks/t-001.md").await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn setup_project() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        let workspace = dir.path();
        let data_dir = workspace.join(".ferrus/projects/test-project");
        std::fs::create_dir_all(workspace.join(".ferrus")).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        write_toml(
            &workspace.join(".ferrus/project.toml"),
            &LocalProjectRef {
                project_id: "test-project".to_string(),
                name: "test".to_string(),
                data_dir: path_string(&data_dir),
            },
        )
        .await
        .unwrap();
        std::env::set_current_dir(workspace).unwrap();
        initialize_database(&data_dir.join("ferrus.db"))
            .await
            .unwrap();

        record_task_status(
            "t-001",
            ".ferrus/tasks/t-001.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();

        (dir, previous)
    }

    fn teardown(previous: PathBuf) {
        std::env::set_current_dir(previous).unwrap();
    }

    #[tokio::test]
    async fn project_selection_round_trips_through_runtime_database() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;

        write_project_selection(&ProjectSelection {
            selected_spec: Some("docs/specs/spec.md".to_string()),
        })
        .await
        .unwrap();

        let selection = read_project_selection().await.unwrap();
        assert_eq!(
            selection,
            ProjectSelection {
                selected_spec: Some("docs/specs/spec.md".to_string()),
            }
        );
        teardown(previous);
    }

    #[tokio::test]
    async fn last_spec_path_round_trips_through_runtime_database() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;

        assert_eq!(read_last_spec_path().await.unwrap(), None);
        write_last_spec_path("docs/specs/spec.md").await.unwrap();
        assert_eq!(
            read_last_spec_path().await.unwrap().as_deref(),
            Some("docs/specs/spec.md")
        );
        clear_last_spec_path().await.unwrap();
        assert_eq!(read_last_spec_path().await.unwrap(), None);

        teardown(previous);
    }

    #[tokio::test]
    async fn project_runtime_state_migrates_temporary_runtime_metadata_table() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        let database_path = current_database_path().await.unwrap();
        {
            let connection = Connection::open(&database_path).unwrap();
            connection
                .execute(
                    r#"
                    CREATE TABLE IF NOT EXISTS runtime_metadata (
                        key TEXT PRIMARY KEY,
                        value TEXT,
                        updated_at TEXT NOT NULL
                    )
                    "#,
                    [],
                )
                .unwrap();
            for (key, value) in [
                ("selected_spec", "docs/specs/spec.md"),
                ("last_spec_path", "docs/specs/spec.md"),
            ] {
                connection
                    .execute(
                        "INSERT OR REPLACE INTO runtime_metadata (key, value, updated_at) VALUES (?1, ?2, ?3)",
                        params![key, value, timestamp()],
                    )
                    .unwrap();
            }
        }

        let selection = read_project_selection().await.unwrap();
        let last_spec_path = read_last_spec_path().await.unwrap();

        assert_eq!(
            selection.selected_spec.as_deref(),
            Some("docs/specs/spec.md")
        );
        assert_eq!(last_spec_path.as_deref(), Some("docs/specs/spec.md"));

        teardown(previous);
    }

    #[tokio::test]
    async fn migration_preserves_idle_legacy_selected_spec() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        let legacy = legacy_state::LegacyStateData {
            state: Some(LegacyTaskState::Idle),
            task_spec: None,
            task_milestone: None,
            selected_spec: Some("docs/specs/selected.md".to_string()),
            selected_milestone: None,
            updated_at: None,
        };

        migrate_legacy_project_selection(&legacy).await.unwrap();

        let selection = read_project_selection().await.unwrap();
        assert_eq!(
            selection.selected_spec.as_deref(),
            Some("docs/specs/selected.md")
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn migration_copies_paused_legacy_interaction_artifacts() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        tokio::fs::create_dir_all(".ferrus/tasks").await.unwrap();
        for (path, contents) in [
            (".ferrus/QUESTION.md", "legacy question"),
            (".ferrus/ANSWER.md", "legacy answer"),
            (".ferrus/CONSULT_REQUEST.md", "legacy consult request"),
            (".ferrus/CONSULT_RESPONSE.md", "legacy consult response"),
        ] {
            tokio::fs::write(path, contents).await.unwrap();
        }

        copy_legacy_artifacts().await.unwrap();

        for (path, expected) in [
            (".ferrus/runs/t-001/QUESTION.md", "legacy question"),
            (".ferrus/runs/t-001/ANSWER.md", "legacy answer"),
            (
                ".ferrus/runs/t-001/CONSULT_REQUEST.md",
                "legacy consult request",
            ),
            (
                ".ferrus/runs/t-001/CONSULT_RESPONSE.md",
                "legacy consult response",
            ),
        ] {
            let contents = tokio::fs::read_to_string(path).await.unwrap();
            assert_eq!(contents, expected);
        }

        teardown(previous);
    }

    #[tokio::test]
    async fn sqlite_task_claim_is_exclusive_and_renewable() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;

        let first = claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 60)
            .await
            .unwrap();
        assert!(matches!(first, TaskClaim::Claimed));

        let second = claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 60)
            .await
            .unwrap();
        assert!(matches!(second, TaskClaim::AlreadyClaimed));

        let other = claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:2", 60)
            .await
            .unwrap();
        match other {
            TaskClaim::ClaimedByOther { claimed_by } => {
                assert_eq!(claimed_by, "executor:codex:1");
            }
            _ => panic!("expected claimed_by_other"),
        }

        let renewed = renew_claimed_task_lease("executor:codex:1", 60)
            .await
            .unwrap();
        assert!(matches!(renewed, LeaseRenewal::Renewed { .. }));

        teardown(previous);
    }

    #[tokio::test]
    async fn sqlite_task_claim_can_target_non_current_task() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;

        let first = claim_task("t-002", ".ferrus/tasks/t-002.md", "executor:codex:2", 60)
            .await
            .unwrap();
        assert!(matches!(first, TaskClaim::Claimed));

        let second = claim_task("t-002", ".ferrus/tasks/t-002.md", "executor:codex:3", 60)
            .await
            .unwrap();
        match second {
            TaskClaim::ClaimedByOther { claimed_by } => {
                assert_eq!(claimed_by, "executor:codex:2");
            }
            _ => panic!("expected claimed_by_other"),
        }

        let tasks = list_tasks().await.unwrap();
        let current = tasks.iter().find(|task| task.id == "t-001").unwrap();
        let targeted = tasks.iter().find(|task| task.id == "t-002").unwrap();
        assert_eq!(current.claimed_by, None);
        assert_eq!(targeted.path, ".ferrus/tasks/t-002.md");
        assert_eq!(targeted.status, "unknown");
        assert_eq!(targeted.claimed_by.as_deref(), Some("executor:codex:2"));

        teardown(previous);
    }

    #[tokio::test]
    async fn sqlite_task_lease_can_be_renewed_by_claiming_agent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;

        let first = claim_task("t-002", ".ferrus/tasks/t-002.md", "executor:codex:2", 60)
            .await
            .unwrap();
        assert!(matches!(first, TaskClaim::Claimed));

        let renewed = renew_claimed_task_lease("executor:codex:2", 60)
            .await
            .unwrap();
        match renewed {
            LeaseRenewal::Renewed {
                task_id,
                task_path,
                claimed_by,
                ..
            } => {
                assert_eq!(task_id, "t-002");
                assert_eq!(task_path, ".ferrus/tasks/t-002.md");
                assert_eq!(claimed_by, "executor:codex:2");
            }
            _ => panic!("expected claimed task lease to renew"),
        }

        let missing = renew_claimed_task_lease("executor:codex:3", 60)
            .await
            .unwrap();
        assert!(matches!(missing, LeaseRenewal::NotClaimed));

        teardown(previous);
    }

    #[tokio::test]
    async fn runtime_task_context_resolves_claimed_task_by_agent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;

        record_task_status(
            "t-002",
            ".ferrus/tasks/t-002.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        claim_task("t-002", ".ferrus/tasks/t-002.md", "executor:codex:2", 60)
            .await
            .unwrap();

        let context = runtime_task_context_for_agent("executor:codex:2")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(context.task_id, "t-002");
        assert_eq!(context.task_path, ".ferrus/tasks/t-002.md");
        assert_eq!(context.run_dir, ".ferrus/runs/t-002");
        assert_eq!(context.status, "executing");
        assert!(context.run_id.is_none());

        teardown(previous);
    }

    #[tokio::test]
    async fn sqlite_claim_next_ready_task_skips_active_claims_and_preserves_agent_lease() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        record_task_status(
            "t-002",
            ".ferrus/tasks/t-002.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        record_task_status(
            "t-003",
            ".ferrus/tasks/t-003.md",
            crate::project::TaskStatus::Reviewing,
        )
        .await
        .unwrap();

        let first = claim_next_ready_task("executor:codex:1", 60).await.unwrap();
        match first {
            ReadyTaskClaim::Claimed(task) => {
                assert_eq!(task.task_id, "t-001");
                assert_eq!(task.task_path, ".ferrus/tasks/t-001.md");
                assert_eq!(task.status, "executing");
                assert_eq!(task.claimed_by, "executor:codex:1");
            }
            _ => panic!("expected first ready task to be claimed"),
        }

        let same_agent = claim_next_ready_task("executor:codex:1", 60).await.unwrap();
        match same_agent {
            ReadyTaskClaim::AlreadyClaimed(task) => {
                assert_eq!(task.task_id, "t-001");
                assert_eq!(task.claimed_by, "executor:codex:1");
            }
            _ => panic!("expected existing agent lease"),
        }

        let other_agent = claim_next_ready_task("executor:codex:2", 60).await.unwrap();
        match other_agent {
            ReadyTaskClaim::Claimed(task) => {
                assert_eq!(task.task_id, "t-002");
                assert_eq!(task.task_path, ".ferrus/tasks/t-002.md");
                assert_eq!(task.status, "executing");
                assert_eq!(task.claimed_by, "executor:codex:2");
            }
            _ => panic!("expected second ready task to be claimed"),
        }

        let no_available = claim_next_ready_task("executor:codex:3", 60).await.unwrap();
        assert!(matches!(no_available, ReadyTaskClaim::NoAvailable));

        let tasks = list_tasks().await.unwrap();
        let reviewing = tasks.iter().find(|task| task.id == "t-003").unwrap();
        assert_eq!(reviewing.claimed_by, None);

        teardown(previous);
    }

    #[tokio::test]
    async fn sqlite_claim_ready_task_by_id_promotes_pending_task() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        record_task_status(
            "t-002",
            ".ferrus/tasks/t-002.md",
            crate::project::TaskStatus::Pending,
        )
        .await
        .unwrap();

        let claim = claim_ready_task_by_id("t-002", "executor:codex:t-002", 60)
            .await
            .unwrap();

        match claim {
            ReadyTaskClaim::Claimed(task) => {
                assert_eq!(task.task_id, "t-002");
                assert_eq!(task.task_path, ".ferrus/tasks/t-002.md");
                assert_eq!(task.status, "executing");
                assert_eq!(task.claimed_by, "executor:codex:t-002");
            }
            _ => panic!("expected pending task to be promoted and claimed"),
        }

        let tasks = list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-002").unwrap();
        assert_eq!(task.status, "executing");
        assert_eq!(task.claimed_by.as_deref(), Some("executor:codex:t-002"));

        teardown(previous);
    }

    #[tokio::test]
    async fn sqlite_claim_next_review_task_claims_reviewing_rows_only() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        record_task_status(
            "t-002",
            ".ferrus/tasks/t-002.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        record_task_status(
            "t-003",
            ".ferrus/tasks/t-003.md",
            crate::project::TaskStatus::Reviewing,
        )
        .await
        .unwrap();

        let claim = claim_next_review_task("supervisor:codex:1", 60)
            .await
            .unwrap();

        match claim {
            ReadyTaskClaim::Claimed(task) => {
                assert_eq!(task.task_id, "t-003");
                assert_eq!(task.task_path, ".ferrus/tasks/t-003.md");
                assert_eq!(task.status, "reviewing");
                assert_eq!(task.claimed_by, "supervisor:codex:1");
            }
            _ => panic!("expected reviewing task to be claimed"),
        }

        let tasks = list_tasks().await.unwrap();
        let executing = tasks.iter().find(|task| task.id == "t-002").unwrap();
        let reviewing = tasks.iter().find(|task| task.id == "t-003").unwrap();
        assert_eq!(executing.claimed_by, None);
        assert_eq!(reviewing.claimed_by.as_deref(), Some("supervisor:codex:1"));

        teardown(previous);
    }

    #[tokio::test]
    async fn sqlite_claim_review_task_by_id_does_not_steal_another_review() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        record_task_status(
            "t-002",
            ".ferrus/tasks/t-002.md",
            crate::project::TaskStatus::Reviewing,
        )
        .await
        .unwrap();
        record_task_status(
            "t-003",
            ".ferrus/tasks/t-003.md",
            crate::project::TaskStatus::Reviewing,
        )
        .await
        .unwrap();

        let missing = claim_review_task_by_id("t-999", "supervisor:codex:t-999", 60)
            .await
            .unwrap();
        assert!(matches!(missing, ReadyTaskClaim::NoAvailable));

        let claim = claim_review_task_by_id("t-003", "supervisor:codex:t-003", 60)
            .await
            .unwrap();
        match claim {
            ReadyTaskClaim::Claimed(task) => {
                assert_eq!(task.task_id, "t-003");
                assert_eq!(task.task_path, ".ferrus/tasks/t-003.md");
                assert_eq!(task.status, "reviewing");
                assert_eq!(task.claimed_by, "supervisor:codex:t-003");
            }
            _ => panic!("expected targeted reviewing task to be claimed"),
        }

        let tasks = list_tasks().await.unwrap();
        let other = tasks.iter().find(|task| task.id == "t-002").unwrap();
        let targeted = tasks.iter().find(|task| task.id == "t-003").unwrap();
        assert_eq!(other.claimed_by, None);
        assert_eq!(
            targeted.claimed_by.as_deref(),
            Some("supervisor:codex:t-003")
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn list_human_questions_reads_scoped_awaiting_human_tasks() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        record_task_status(
            "t-002",
            ".ferrus/tasks/t-002.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        record_task_human_question_requested(
            "t-002",
            crate::project::TaskStatus::Executing,
            "executor:codex:2",
        )
        .await
        .unwrap();
        crate::state::store::write_question_for_run_dir(
            ".ferrus/runs/t-002",
            "Which option should I use?",
        )
        .await
        .unwrap();

        let questions = list_human_questions().await.unwrap();

        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].task_id, "t-002");
        assert_eq!(questions[0].task_path, ".ferrus/tasks/t-002.md");
        assert_eq!(questions[0].run_dir, ".ferrus/runs/t-002");
        assert_eq!(questions[0].question, "Which option should I use?");

        teardown(previous);
    }

    #[tokio::test]
    async fn list_tasks_reads_runtime_rows() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 60)
            .await
            .unwrap();

        let tasks = list_tasks().await.unwrap();

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "t-001");
        assert_eq!(tasks[0].path, ".ferrus/tasks/t-001.md");
        assert_eq!(tasks[0].status, "executing");
        assert_eq!(tasks[0].claimed_by.as_deref(), Some("executor:codex:1"));
        assert!(tasks[0].lease_until.is_some());

        teardown(previous);
    }

    #[tokio::test]
    async fn current_task_status_does_not_read_legacy_state_origin() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;

        record_current_task_status(crate::project::TaskStatus::Executing)
            .await
            .unwrap();

        let tasks = list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
        assert!(task.spec_path.is_none());
        assert!(task.milestone_id.is_none());

        teardown(previous);
    }

    #[tokio::test]
    async fn task_status_update_preserves_existing_origin_metadata() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        record_task_status_with_origin(
            "t-001",
            ".ferrus/tasks/t-001.md",
            crate::project::TaskStatus::Executing,
            Some("docs/specs/spec.md"),
            Some("m1.0"),
        )
        .await
        .unwrap();

        record_task_status(
            "t-001",
            ".ferrus/tasks/t-001.md",
            crate::project::TaskStatus::Reviewing,
        )
        .await
        .unwrap();

        let tasks = list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
        assert_eq!(task.status, "reviewing");
        assert_eq!(task.spec_path.as_deref(), Some("docs/specs/spec.md"));
        assert_eq!(task.milestone_id.as_deref(), Some("m1.0"));

        teardown(previous);
    }

    #[tokio::test]
    async fn finds_non_terminal_task_by_origin() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        record_task_status_with_origin(
            "t-002",
            ".ferrus/tasks/t-002.md",
            crate::project::TaskStatus::Pending,
            Some("docs/specs/spec.md"),
            Some("m1.1"),
        )
        .await
        .unwrap();

        let task = find_non_terminal_task_by_origin("docs/specs/spec.md", "m1.1")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(task.id, "t-002");

        record_task_status(
            "t-002",
            ".ferrus/tasks/t-002.md",
            crate::project::TaskStatus::Complete,
        )
        .await
        .unwrap();
        let task = find_non_terminal_task_by_origin("docs/specs/spec.md", "m1.1")
            .await
            .unwrap();
        assert!(task.is_none());

        teardown(previous);
    }

    #[tokio::test]
    async fn sqlite_task_check_failures_use_per_task_retry_budget() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 60)
            .await
            .unwrap();

        let first = record_task_check_failed("t-001", "fmt failed", 2)
            .await
            .unwrap();
        assert!(matches!(first, TaskCheckFailure::Failed { retries: 1 }));

        let tasks = list_tasks().await.unwrap();
        assert_eq!(tasks[0].status, "executing");
        assert_eq!(tasks[0].check_retries, 1);
        assert_eq!(tasks[0].failure_reason.as_deref(), Some("fmt failed"));
        assert_eq!(tasks[0].claimed_by.as_deref(), Some("executor:codex:1"));

        let second = record_task_check_failed("t-001", "tests failed", 2)
            .await
            .unwrap();
        assert!(matches!(
            second,
            TaskCheckFailure::LimitExceeded { retries: 2 }
        ));

        let tasks = list_tasks().await.unwrap();
        assert_eq!(tasks[0].status, "failed");
        assert_eq!(tasks[0].check_retries, 2);
        assert_eq!(tasks[0].claimed_by, None);
        assert_eq!(tasks[0].lease_until, None);
        assert!(
            tasks[0]
                .failure_reason
                .as_deref()
                .unwrap_or_default()
                .contains("Last failure:\ntests failed")
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn mirrored_check_state_can_fail_task_and_clear_lease() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        record_task_status(
            "t-001",
            ".ferrus/tasks/t-001.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 60)
            .await
            .unwrap();

        mirror_task_check_state(
            "t-001",
            crate::project::TaskStatus::Failed,
            2,
            Some("tests failed"),
        )
        .await
        .unwrap();

        let tasks = list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
        assert_eq!(task.status, "failed");
        assert_eq!(task.check_retries, 2);
        assert_eq!(task.failure_reason.as_deref(), Some("tests failed"));
        assert_eq!(task.claimed_by, None);

        teardown(previous);
    }

    #[tokio::test]
    async fn sqlite_task_review_rejections_use_per_task_cycle_budget() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        record_task_status(
            "t-001",
            ".ferrus/tasks/t-001.md",
            crate::project::TaskStatus::Reviewing,
        )
        .await
        .unwrap();
        claim_task("t-001", ".ferrus/tasks/t-001.md", "supervisor:codex:1", 60)
            .await
            .unwrap();

        let first = record_task_review_rejected("t-001", 2).await.unwrap();
        assert!(matches!(
            first,
            TaskReviewRejection::Addressing { cycles: 1 }
        ));

        let tasks = list_tasks().await.unwrap();
        assert_eq!(tasks[0].status, "addressing");
        assert_eq!(tasks[0].review_cycles, 1);
        assert_eq!(tasks[0].check_retries, 0);
        assert_eq!(tasks[0].claimed_by, None);

        record_task_status(
            "t-001",
            ".ferrus/tasks/t-001.md",
            crate::project::TaskStatus::Reviewing,
        )
        .await
        .unwrap();
        let second = record_task_review_rejected("t-001", 2).await.unwrap();
        assert!(matches!(
            second,
            TaskReviewRejection::LimitExceeded { cycles: 2 }
        ));

        let tasks = list_tasks().await.unwrap();
        assert_eq!(tasks[0].status, "failed");
        assert_eq!(tasks[0].review_cycles, 2);
        assert_eq!(tasks[0].claimed_by, None);
        assert!(
            tasks[0]
                .failure_reason
                .as_deref()
                .unwrap_or_default()
                .contains("Task rejected 2 times")
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn handoff_task_statuses_clear_database_lease() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 60)
            .await
            .unwrap();

        record_task_status(
            "t-001",
            ".ferrus/tasks/t-001.md",
            crate::project::TaskStatus::Reviewing,
        )
        .await
        .unwrap();
        let tasks = list_tasks().await.unwrap();
        assert_eq!(tasks[0].status, "reviewing");
        assert_eq!(tasks[0].claimed_by, None);
        assert_eq!(tasks[0].lease_until, None);
        assert_eq!(tasks[0].last_heartbeat, None);

        claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:2", 60)
            .await
            .unwrap();
        record_task_status(
            "t-001",
            ".ferrus/tasks/t-001.md",
            crate::project::TaskStatus::Addressing,
        )
        .await
        .unwrap();
        let tasks = list_tasks().await.unwrap();
        assert_eq!(tasks[0].status, "addressing");
        assert_eq!(tasks[0].claimed_by, None);
        assert_eq!(tasks[0].lease_until, None);
        assert_eq!(tasks[0].last_heartbeat, None);

        teardown(previous);
    }

    #[tokio::test]
    async fn runtime_doctor_checks_detect_missing_active_artifacts() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        let database_path = current_database_path().await.unwrap();
        let mut checks = Vec::new();

        add_runtime_doctor_checks(&mut checks, &database_path).await;

        assert!(checks.iter().any(|check| {
            !check.ok && check.message == "task artifact exists for t-001 at .ferrus/tasks/t-001.md"
        }));
        assert!(checks.iter().any(|check| {
            !check.ok
                && check.message == "run artifact directory exists for t-001 at .ferrus/runs/t-001"
        }));

        teardown(previous);
    }

    #[tokio::test]
    async fn runtime_doctor_checks_accept_consistent_active_task() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        tokio::fs::create_dir_all(".ferrus/tasks").await.unwrap();
        tokio::fs::write(".ferrus/tasks/t-001.md", "task")
            .await
            .unwrap();
        tokio::fs::create_dir_all(".ferrus/runs/t-001")
            .await
            .unwrap();
        let database_path = current_database_path().await.unwrap();
        let mut checks = Vec::new();

        add_runtime_doctor_checks(&mut checks, &database_path).await;

        assert!(
            checks.iter().all(|check| check.ok),
            "unexpected failed checks: {:?}",
            checks
                .iter()
                .filter(|check| !check.ok)
                .map(|check| check.message.as_str())
                .collect::<Vec<_>>()
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn runtime_doctor_ignores_unknown_current_compatibility_task() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        tokio::fs::create_dir_all(".ferrus/tasks").await.unwrap();
        tokio::fs::write(".ferrus/tasks/t-001.md", "task")
            .await
            .unwrap();
        tokio::fs::create_dir_all(".ferrus/runs/t-001")
            .await
            .unwrap();
        record_run_started("supervisor", "supervisor:codex", std::process::id())
            .await
            .unwrap();
        let database_path = current_database_path().await.unwrap();
        let mut checks = Vec::new();

        add_runtime_doctor_checks(&mut checks, &database_path).await;

        assert!(
            checks.iter().all(|check| check.ok),
            "unexpected failed checks: {:?}",
            checks
                .iter()
                .filter(|check| !check.ok)
                .map(|check| check.message.as_str())
                .collect::<Vec<_>>()
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn migrate_retires_legacy_current_task_row() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        record_task_status(
            CURRENT_TASK_ID,
            CURRENT_TASK_PATH,
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        let run = record_run_started("supervisor", "supervisor:codex", std::process::id())
            .await
            .unwrap();
        assert_eq!(run.task_id, CURRENT_TASK_ID);

        retire_legacy_current_task_row().await.unwrap();

        let tasks = list_tasks().await.unwrap();
        let current = tasks
            .iter()
            .find(|task| task.id == CURRENT_TASK_ID)
            .unwrap();
        assert_eq!(current.status, TaskStatus::Reset.as_str());
        let runs = list_runs(10).await.unwrap();
        let run = runs
            .iter()
            .find(|candidate| candidate.id == run.id)
            .unwrap();
        assert_eq!(run.task_id, "t-001");

        teardown(previous);
    }

    #[tokio::test]
    async fn recover_expired_task_leases_releases_stale_claims() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 0)
            .await
            .unwrap();

        let recovered = recover_expired_task_leases().await.unwrap();
        let tasks = list_tasks().await.unwrap();
        let events = list_events(10, None).await.unwrap();

        assert_eq!(recovered, 1);
        assert_eq!(tasks[0].claimed_by, None);
        assert_eq!(tasks[0].lease_until, None);
        assert_eq!(tasks[0].last_heartbeat, None);
        assert!(
            events
                .iter()
                .any(|event| event.event_type == "task_lease_expired")
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn runtime_doctor_checks_database_task_artifacts() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup_project().await;
        tokio::fs::create_dir_all(dir.path().join(".ferrus/tasks"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(dir.path().join(".ferrus/runs/t-010"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join(".ferrus/tasks/t-010.md"), "task")
            .await
            .unwrap();
        record_task_status(
            "t-010",
            ".ferrus/tasks/t-010.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        let database_path = current_database_path().await.unwrap();
        let mut checks = Vec::new();

        add_runtime_doctor_checks(&mut checks, &database_path).await;

        assert!(
            checks
                .iter()
                .any(|check| check.ok && check.message == "task rows can be read from ferrus.db")
        );
        assert!(
            checks
                .iter()
                .any(|check| check.ok && check.message.contains("task artifact exists for t-010"))
        );
        assert!(checks.iter().any(|check| {
            check.ok
                && check
                    .message
                    .contains("run artifact directory exists for t-010")
        }));

        teardown(previous);
    }

    #[tokio::test]
    async fn preview_orphaned_worktrees_ignores_active_tasks_and_runs() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup_project().await;
        let workspace = dir.path();
        let worktrees_dir = workspace.join(".ferrus/projects/test-project/worktrees");
        tokio::fs::create_dir_all(worktrees_dir.join("t-active"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(worktrees_dir.join("t-run"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(worktrees_dir.join("t-orphan"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(worktrees_dir.join(BASELINE_WORKTREE_METADATA_DIR))
            .await
            .unwrap();

        record_task_status(
            "t-active",
            ".ferrus/tasks/t-active.md",
            crate::project::TaskStatus::Addressing,
        )
        .await
        .unwrap();
        let run = record_run_started_with_workspace(
            "executor-run-t-run",
            "executor",
            "executor:codex:t-run",
            std::process::id(),
            path_string(&worktrees_dir.join("t-run")),
        )
        .await
        .unwrap();
        record_task_status(
            "t-run",
            ".ferrus/tasks/t-run.md",
            crate::project::TaskStatus::Complete,
        )
        .await
        .unwrap();
        let attached =
            attach_running_run_to_task("executor:codex:t-run", "t-run", ".ferrus/tasks/t-run.md")
                .await
                .unwrap();
        assert_eq!(attached.as_deref(), Some(run.id.as_str()));

        let registration = ProjectRegistration {
            local_ref: LocalProjectRef {
                project_id: "test-project".to_string(),
                name: "test".to_string(),
                data_dir: path_string(&workspace.join(".ferrus/projects/test-project")),
            },
            metadata: ProjectMetadata {
                id: "test-project".to_string(),
                name: "test".to_string(),
                workspace_dir: path_string(workspace),
                ferrus_dir: path_string(&workspace.join(".ferrus")),
                vcs: None,
                origin_repo: None,
                default_branch: None,
                current_head: None,
                created_at: "2026-05-16T10:00:00Z".to_string(),
                last_opened_at: "2026-05-16T10:00:00Z".to_string(),
                version: PROJECT_VERSION,
            },
            data_dir: workspace.join(".ferrus/projects/test-project"),
            database_path: workspace.join(".ferrus/projects/test-project/ferrus.db"),
        };
        let orphaned = orphaned_worktrees_for(&registration).await.unwrap();

        assert_eq!(orphaned, vec![worktrees_dir.join("t-orphan")]);

        teardown(previous);
    }

    #[tokio::test]
    async fn preview_runtime_recovery_reports_pending_work_without_mutating() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 0)
            .await
            .unwrap();
        let database_path = current_database_path().await.unwrap();
        let mut checks = Vec::new();

        let preview = preview_runtime_recovery_from(&database_path).await.unwrap();
        add_recovery_doctor_checks(&mut checks, &database_path).await;
        let tasks = list_tasks().await.unwrap();

        assert_eq!(preview.interrupted_runs, 0);
        assert_eq!(preview.expired_task_leases, 1);
        assert_eq!(tasks[0].claimed_by.as_deref(), Some("executor:codex:1"));
        assert!(checks.iter().any(|check| {
            !check.ok
                && check
                    .message
                    .contains("expired task lease recovery pending (1")
        }));

        teardown(previous);
    }

    #[tokio::test]
    async fn list_runs_and_events_reads_runtime_rows() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;

        let run = record_run_started("executor", "executor:codex:1", std::process::id())
            .await
            .unwrap();
        record_task_status(
            "t-002",
            ".ferrus/tasks/t-002.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        let attached =
            attach_running_run_to_task("executor:codex:1", "t-002", ".ferrus/tasks/t-002.md")
                .await
                .unwrap();
        assert_eq!(attached.as_deref(), Some(run.id.as_str()));
        record_runtime_event(
            Some(run.id.clone()),
            "test_event",
            serde_json::json!({ "ok": true }),
        )
        .await
        .unwrap();
        record_run_finished(&run.id, 0).await.unwrap();

        let runs = list_runs(10).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, run.id);
        assert_eq!(runs[0].task_id, "t-002");
        assert_eq!(runs[0].role, "executor");
        assert_eq!(runs[0].agent, "executor:codex:1");
        assert_eq!(runs[0].status, "completed");
        assert!(runs[0].pid.is_none());
        assert!(!runs[0].started_at.is_empty());
        assert!(!runs[0].updated_at.is_empty());

        let events = list_events(10, Some(run.id.clone())).await.unwrap();
        assert!(events.iter().any(|event| event.event_type == "run_started"));
        assert!(
            events
                .iter()
                .any(|event| event.event_type == "run_task_attached")
        );
        assert!(events.iter().any(|event| event.event_type == "test_event"));
        assert!(
            events
                .iter()
                .any(|event| event.event_type == "run_finished")
        );
        assert!(
            events
                .iter()
                .all(|event| event.run_id.as_deref() == Some(run.id.as_str()))
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn record_run_started_can_use_preallocated_run_id() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        let run_id = allocate_run_id("executor", "executor:codex:t-002");

        let run = record_run_started_with_id(
            &run_id,
            "executor",
            "executor:codex:t-002",
            std::process::id(),
        )
        .await
        .unwrap();

        assert_eq!(run.id, run_id);
        let runs = list_runs(10).await.unwrap();
        assert!(runs.iter().any(|run| run.id == run_id));

        teardown(previous);
    }

    #[tokio::test]
    async fn record_run_started_can_store_explicit_workspace_path() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup_project().await;
        let run_id = allocate_run_id("executor", "executor:codex:t-003");
        let workspace_path = path_string(&dir.path().join("worktrees").join("t-003"));

        let run = record_run_started_with_workspace(
            &run_id,
            "executor",
            "executor:codex:t-003",
            std::process::id(),
            workspace_path.clone(),
        )
        .await
        .unwrap();

        assert_eq!(run.id, run_id);
        assert_eq!(run.workspace_path, workspace_path);
        let runs = list_runs(10).await.unwrap();
        assert!(
            runs.iter()
                .any(|run| run.id == run_id && run.workspace_path == workspace_path)
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn record_run_started_can_target_requested_task_without_lease() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup_project().await;
        record_task_status(
            "t-012",
            ".ferrus/tasks/t-012.md",
            crate::project::TaskStatus::AwaitingHuman,
        )
        .await
        .unwrap();
        let run_id = allocate_run_id("executor", "executor:codex:t-012");
        let workspace_path = path_string(&dir.path().join("worktrees").join("t-012"));

        let run = record_run_started_for_task_with_workspace(
            &run_id,
            "executor",
            "executor:codex:t-012",
            std::process::id(),
            Some("t-012"),
            workspace_path.clone(),
        )
        .await
        .unwrap();

        assert_eq!(run.id, run_id);
        assert_eq!(run.task_id, "t-012");
        let context = runtime_task_context_for_agent("executor:codex:t-012")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(context.task_id, "t-012");
        assert_eq!(context.task_path, ".ferrus/tasks/t-012.md");
        assert_eq!(
            context.workspace_path.as_deref(),
            Some(workspace_path.as_str())
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn consultation_attachment_is_exclusive_to_one_supervisor_run() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        record_task_consultation_requested("t-007", crate::project::TaskStatus::Executing)
            .await
            .unwrap();
        let first_run = record_run_started("supervisor", "supervisor:codex:1", std::process::id())
            .await
            .unwrap();
        let second_run = record_run_started("supervisor", "supervisor:codex:2", std::process::id())
            .await
            .unwrap();

        let first = attach_running_run_to_next_consultation("supervisor:codex:1")
            .await
            .unwrap();
        let second = attach_running_run_to_next_consultation("supervisor:codex:2")
            .await
            .unwrap();

        assert_eq!(
            first.as_ref().map(|context| context.task_id.as_str()),
            Some("t-007")
        );
        assert!(second.is_none());

        let runs = list_runs(10).await.unwrap();
        let first = runs.iter().find(|run| run.id == first_run.id).unwrap();
        let second = runs.iter().find(|run| run.id == second_run.id).unwrap();
        assert_eq!(first.task_id, "t-007");
        assert_eq!(second.task_id, CURRENT_TASK_ID);

        teardown(previous);
    }

    #[tokio::test]
    async fn targeted_consultation_attachment_does_not_steal_another_task() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        record_task_status(
            "t-008",
            ".ferrus/tasks/t-008.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        record_task_consultation_requested("t-007", crate::project::TaskStatus::Executing)
            .await
            .unwrap();
        record_task_consultation_requested("t-008", crate::project::TaskStatus::Executing)
            .await
            .unwrap();
        let first_run =
            record_run_started("supervisor", "supervisor:codex:t-008", std::process::id())
                .await
                .unwrap();
        let second_run =
            record_run_started("supervisor", "supervisor:codex:t-009", std::process::id())
                .await
                .unwrap();

        let first = attach_running_run_to_consultation("t-008", "supervisor:codex:t-008")
            .await
            .unwrap();
        let second = attach_running_run_to_consultation("t-009", "supervisor:codex:t-009")
            .await
            .unwrap();

        assert_eq!(
            first.as_ref().map(|context| context.task_id.as_str()),
            Some("t-008")
        );
        assert!(second.is_none());

        let runs = list_runs(10).await.unwrap();
        let first = runs.iter().find(|run| run.id == first_run.id).unwrap();
        let second = runs.iter().find(|run| run.id == second_run.id).unwrap();
        assert_eq!(first.task_id, "t-008");
        assert_eq!(second.task_id, CURRENT_TASK_ID);

        teardown(previous);
    }

    #[tokio::test]
    async fn list_registered_projects_reads_valid_and_invalid_entries() {
        let dir = TempDir::new().unwrap();
        let projects_dir = dir.path().join("projects");
        let valid_dir = projects_dir.join("PVALID");
        let invalid_dir = projects_dir.join("PBROKEN");
        std::fs::create_dir_all(&valid_dir).unwrap();
        std::fs::create_dir_all(&invalid_dir).unwrap();
        std::fs::write(valid_dir.join("ferrus.db"), "").unwrap();
        write_toml(
            &valid_dir.join("project.toml"),
            &ProjectMetadata {
                id: "PVALID".to_string(),
                name: "ferrus".to_string(),
                workspace_dir: "/tmp/ferrus".to_string(),
                ferrus_dir: "/tmp/ferrus/.ferrus".to_string(),
                vcs: Some("git".to_string()),
                origin_repo: None,
                default_branch: Some("main".to_string()),
                current_head: None,
                created_at: "2026-05-16T10:00:00Z".to_string(),
                last_opened_at: "2026-05-17T10:00:00Z".to_string(),
                version: PROJECT_VERSION,
            },
        )
        .await
        .unwrap();
        std::fs::write(invalid_dir.join("project.toml"), "not = [toml").unwrap();

        let projects = list_registered_projects_from(&projects_dir).await.unwrap();

        assert_eq!(projects.len(), 2);
        let valid = projects
            .iter()
            .find(|project| project.id == "PVALID")
            .unwrap();
        assert_eq!(valid.name.as_deref(), Some("ferrus"));
        assert_eq!(valid.workspace_dir.as_deref(), Some("/tmp/ferrus"));
        assert!(valid.database_exists);
        assert!(valid.error.is_none());

        let invalid = projects
            .iter()
            .find(|project| project.id == "PBROKEN")
            .unwrap();
        assert!(invalid.name.is_none());
        assert!(!invalid.database_exists);
        assert!(invalid.error.is_some());
    }

    #[tokio::test]
    async fn touch_current_project_updates_last_opened_without_rewriting_local_ref() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup_project().await;
        let workspace = dir.path();
        let data_dir = workspace.join(".ferrus/projects/test-project");
        let metadata_path = data_dir.join("project.toml");
        let created_at = "2026-05-16T10:00:00Z";
        write_toml(
            &metadata_path,
            &ProjectMetadata {
                id: "test-project".to_string(),
                name: "old-name".to_string(),
                workspace_dir: "/old/workspace".to_string(),
                ferrus_dir: "/old/workspace/.ferrus".to_string(),
                vcs: None,
                origin_repo: None,
                default_branch: None,
                current_head: None,
                created_at: created_at.to_string(),
                last_opened_at: "2026-05-16T11:00:00Z".to_string(),
                version: PROJECT_VERSION,
            },
        )
        .await
        .unwrap();
        let local_ref_before = tokio::fs::read_to_string(workspace.join(".ferrus/project.toml"))
            .await
            .unwrap();

        let registration = touch_current_project().await.unwrap();
        let metadata = read_project_metadata_from(&metadata_path).await.unwrap();
        let local_ref_after = tokio::fs::read_to_string(workspace.join(".ferrus/project.toml"))
            .await
            .unwrap();
        let canonical_workspace = tokio::fs::canonicalize(workspace).await.unwrap();

        assert_eq!(registration.local_ref.project_id, "test-project");
        assert_eq!(metadata.id, "test-project");
        assert_eq!(metadata.created_at, created_at);
        assert_ne!(metadata.last_opened_at, "2026-05-16T11:00:00Z");
        assert_eq!(metadata.workspace_dir, path_string(&canonical_workspace));
        assert_eq!(local_ref_after, local_ref_before);

        teardown(previous);
    }
}
