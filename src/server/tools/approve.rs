//! Integrate reviewed work into the canonical workspace, check it, and roll back on failure.
//! Completion follows successful integration; graph refresh observes the resulting source state.

use anyhow::{Context, Result};
use fs2::FileExt;
use neva::prelude::*;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::info;

use crate::{
    config::Config,
    project::{self, RuntimeTaskContext},
    specs,
    state::store,
};

use super::{
    check_gate::{self, CheckGateResult},
    ensure_lease_owner_or_reclaim, require_runtime_task_context, tool_err,
};

pub const DESCRIPTION: &str = "Approve the current submission. Transitions state Reviewing -> Complete. \
     Must be called after /review_pending.";

/// Approval changed external state and could not confirm that rollback restored it.
#[derive(Debug, thiserror::Error)]
#[error("{error}\n\nApproval rollback failed; manual reconciliation is required: {rollback}")]
pub(crate) struct ApprovalEffectUnknown {
    error: anyhow::Error,
    rollback: anyhow::Error,
}

fn rollback_failed(error: anyhow::Error, rollback: anyhow::Error) -> anyhow::Error {
    ApprovalEffectUnknown { error, rollback }.into()
}

pub async fn handler(ctx: neva::di::Dc<crate::server::ServerContext>) -> Result<String, Error> {
    handler_for_agent(ctx.agent_id()).await
}

pub async fn handler_for_agent(agent_id: &str) -> Result<String, Error> {
    run(agent_id).await.map_err(tool_err)
}

async fn run(agent_id: &str) -> Result<String> {
    let project_root = project::canonical_project_root().await?;
    let config = Config::load_from(&project_root).await?;
    let context = require_runtime_task_context(agent_id).await?;

    if context.status.parse::<project::TaskStatus>()? != project::TaskStatus::Reviewing {
        anyhow::bail!(
            "Cannot approve from state {}. Call /review_pending first.",
            context.status
        );
    }

    ensure_lease_owner_or_reclaim(agent_id, config.lease.ttl_secs).await?;

    run_for_context(context, project_root, None).await
}

pub(crate) async fn run_for_context(
    context: RuntimeTaskContext,
    project_root: PathBuf,
    native_scope: Option<project::ExecutorSessionScope>,
) -> Result<String> {
    // Observe both manifests while integration is serialized, then release the
    // approval lock before waiting for a potentially long graph refresh.
    let (integration_result, refresh_canonical_graph) = {
        let _approval_lock = acquire_canonical_approval_lock(&context).await?;
        if let Some(scope) = native_scope.as_ref() {
            let expected = context.repository_view.frozen_source_tree.clone();
            project::with_supervisor_session(scope, false, move |tx, scope, current| {
                project::require_supervisor_review(tx, scope, &current, expected.as_ref())
            })
            .await?;
        }

        let observer = CanonicalIntegrationObserver::capture(&project_root).await;
        let integration_result =
            integrate_approved_task(&context, &project_root, native_scope.as_ref()).await;
        let refresh_canonical_graph = observer
            .finish(&context, &project_root, integration_result.is_ok())
            .await;
        (integration_result, refresh_canonical_graph)
    };
    if integration_result.is_ok() {
        crate::repository_graph_runtime::release_submitted_tree_pin_best_effort(&context).await;
        cleanup_approved_workspace_best_effort(&context, &project_root).await;
    }

    if refresh_canonical_graph {
        crate::repository_graph_runtime::refresh_canonical_graph_after_approval(
            project_root.clone(),
            context.task_id.clone(),
            context.run_id.clone(),
        )
        .await;
    }

    integration_result?;

    project::record_runtime_event_best_effort(
        context.run_id.clone(),
        "approved",
        serde_json::json!({
            "task_id": context.task_id.as_str(),
        }),
    )
    .await;

    info!("Task approved, state -> Complete");
    Ok("Task approved. State: Complete. Well done!".to_string())
}

// Tokio output() inherits stdin. Git must never receive an agent's open
// command-protocol pipe; commands requiring input explicitly replace this with a pipe.
fn approval_git_command() -> Command {
    let mut command = Command::new("git");
    command.stdin(Stdio::null());
    command
}

async fn integrate_approved_task(
    context: &RuntimeTaskContext,
    project_root: &Path,
    native_scope: Option<&project::ExecutorSessionScope>,
) -> Result<()> {
    let canonical_snapshot = apply_approved_patch(context, project_root).await?;
    if let Some(canonical_snapshot) = canonical_snapshot.as_ref() {
        // The approved patch may have changed ferrus.toml, so run the integration
        // gate against the configuration as it exists in the post-apply repository
        // state rather than the config loaded before the patch was applied.
        let post_apply_config = match Config::load_from(project_root).await {
            Ok(config) => config,
            Err(err) => {
                if let Err(rollback_err) = canonical_snapshot.restore(project_root).await {
                    return Err(rollback_failed(err, rollback_err));
                }

                return Err(err);
            }
        };
        let integration_checks =
            run_post_apply_integration_checks(context, &post_apply_config, project_root).await;
        if let Err(err) = integration_checks {
            if let Err(rollback_err) = canonical_snapshot.restore(project_root).await {
                return Err(rollback_failed(err, rollback_err));
            }

            return Err(err);
        }

        if let Err(err) = canonical_snapshot.restore_index().await {
            if let Err(rollback_err) = canonical_snapshot.restore(project_root).await {
                return Err(rollback_failed(err, rollback_err));
            }

            return Err(err).context("Failed to preserve the pre-integration canonical index");
        }
    }

    let transition = async {
        if let Some(scope) = native_scope {
            // Checks can outlive the lease. Fence completion and its milestone
            // update together, then use the normal rollback path on lost authority.
            let expected = context.repository_view.frozen_source_tree.clone();
            let project_root = project_root.to_path_buf();
            let milestone_backup = Arc::new(Mutex::new(None::<(PathBuf, Vec<u8>)>));
            let backup_for_transaction = Arc::clone(&milestone_backup);
            let result =
                project::with_supervisor_session(scope, true, move |tx, scope, current| {
                    project::require_supervisor_review(tx, scope, &current, expected.as_ref())?;
                    project::record_task_status_on_connection(
                        tx,
                        &current.task_id,
                        &current.task_path,
                        project::TaskStatus::Complete,
                        None,
                        None,
                    )?;
                    if let (Some(spec_path), Some(milestone_id)) = (
                        current.spec_path.as_deref(),
                        current.milestone_id.as_deref(),
                    ) {
                        let spec_path = canonical_approval_path(&project_root, spec_path);
                        match std::fs::read(&spec_path) {
                            Ok(contents) => {
                                // Keep this outside the transaction result: COMMIT can
                                // fail after the closure has successfully written the spec.
                                *backup_for_transaction
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner()) =
                                    Some((spec_path.clone(), contents));
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => {
                                return Err(error).context("Failed to back up approval milestone");
                            }
                        }

                        specs::complete_milestone_blocking(
                            &spec_path.to_string_lossy(),
                            milestone_id,
                        )?;
                    }

                    Ok(())
                })
                .await;
            if let Err(error) = result {
                let backup = milestone_backup
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                if let Some((path, contents)) = backup
                    && let Err(rollback_error) = tokio::fs::write(&path, contents).await
                {
                    return Err(rollback_failed(
                        error,
                        anyhow::Error::new(rollback_error).context(format!(
                            "Failed to restore approval milestone {}",
                            path.display()
                        )),
                    ));
                }

                return Err(error);
            }

            return Ok(());
        }

        if let (Some(spec_path), Some(milestone_id)) = (
            context.spec_path.as_deref(),
            context.milestone_id.as_deref(),
        ) {
            let spec_path = canonical_approval_path(project_root, spec_path);
            specs::complete_milestone(&spec_path.to_string_lossy(), milestone_id).await?;
        }

        project::record_task_status(
            &context.task_id,
            &context.task_path,
            project::TaskStatus::Complete,
        )
        .await
    }
    .await;
    if let Err(err) = transition {
        if let Some(canonical_snapshot) = canonical_snapshot.as_ref()
            && let Err(rollback_err) = canonical_snapshot.restore(project_root).await
        {
            return Err(rollback_failed(err, rollback_err));
        }

        return Err(err);
    }

    Ok(())
}

#[derive(Debug)]
enum CanonicalSourceObservation {
    Disabled,
    Observed(project::CanonicalSourceIdentity),
    Unavailable,
}

struct CanonicalIntegrationObserver {
    before: CanonicalSourceObservation,
}

impl CanonicalIntegrationObserver {
    async fn capture(project_root: &Path) -> Self {
        Self {
            before: observe_canonical_source(project_root).await,
        }
    }

    async fn finish(
        self,
        context: &RuntimeTaskContext,
        project_root: &Path,
        integration_succeeded: bool,
    ) -> bool {
        // Measure the actual post-rollback tree: a failed integration may leave
        // partial changes, while a clean rollback must not invalidate the graph.
        let after = observe_canonical_source(project_root).await;
        if matches!(
            (&self.before, &after),
            (
                CanonicalSourceObservation::Observed(before),
                CanonicalSourceObservation::Observed(after)
            ) if before == after
        ) || matches!(
            (&self.before, &after),
            (
                CanonicalSourceObservation::Disabled,
                CanonicalSourceObservation::Disabled
            )
        ) {
            return false;
        }

        let source = match &after {
            CanonicalSourceObservation::Observed(source) => Some(source),
            CanonicalSourceObservation::Disabled | CanonicalSourceObservation::Unavailable => None,
        };
        let reason = match (integration_succeeded, source.is_some()) {
            (true, true) => project::CanonicalInvalidationReason::ApprovedIntegration,
            (false, true) => project::CanonicalInvalidationReason::PartialMutation,
            (_, false) => project::CanonicalInvalidationReason::SourceComparisonUnavailable,
        };
        project::record_canonical_graph_invalidation_best_effort(
            &context.task_id,
            context.run_id.as_deref(),
            source,
            reason,
        )
        .await;

        !matches!(after, CanonicalSourceObservation::Disabled)
    }
}

async fn observe_canonical_source(project_root: &Path) -> CanonicalSourceObservation {
    match crate::repository_graph_runtime::canonical_source_identity_at(project_root).await {
        Ok(Some(source)) => CanonicalSourceObservation::Observed(source),
        Ok(None) => CanonicalSourceObservation::Disabled,
        Err(error) => {
            tracing::warn!(
                error = ?error,
                "canonical source manifest could not be observed during approval"
            );
            CanonicalSourceObservation::Unavailable
        }
    }
}

#[derive(Debug)]
struct CanonicalWorkspaceSnapshot {
    tree: String,
    index: CanonicalIndexSnapshot,
    ignored_paths: Vec<Vec<u8>>,
}

impl CanonicalWorkspaceSnapshot {
    async fn capture(project_root: &Path) -> Result<Self> {
        let index = CanonicalIndexSnapshot::capture(project_root).await?;
        let ignored_paths = ignored_untracked_paths(project_root).await?;
        let tree = capture_canonical_worktree_tree(project_root).await?;
        Ok(Self {
            tree,
            index,
            ignored_paths,
        })
    }

    async fn restore(&self, project_root: &Path) -> Result<()> {
        let current = capture_canonical_worktree_tree(project_root).await?;
        let current = tree_without_paths(project_root, &current, &self.ignored_paths).await?;
        let worktree_result = materialize_tree(project_root, &current, &self.tree)
            .await
            .context("Failed to restore the pre-integration canonical worktree");
        let index_result = self.index.restore().await;
        match (worktree_result, index_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(worktree_error), Err(index_error)) => anyhow::bail!(
                "{worktree_error}\n\nAdditionally failed to restore the canonical index: {index_error}"
            ),
        }
    }

    async fn restore_index(&self) -> Result<()> {
        self.index.restore().await
    }
}

async fn capture_canonical_worktree_tree(project_root: &Path) -> Result<String> {
    let project_root = project_root.to_path_buf();
    let tree = tokio::task::spawn_blocking(move || {
        crate::repository_graph::source::capture_worktree_tree(project_root)
    })
    .await??;
    Ok(tree.value().to_string())
}

async fn ignored_untracked_paths(project_root: &Path) -> Result<Vec<Vec<u8>>> {
    let output = approval_git_command()
        .arg("-C")
        .arg(project_root)
        .args([
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ])
        .output()
        .await?;
    git_success(
        &output,
        "capture ignored canonical paths before integration",
    )?;
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}

#[derive(Debug)]
struct CanonicalIndexSnapshot {
    path: PathBuf,
    contents: Option<Vec<u8>>,
}

impl CanonicalIndexSnapshot {
    async fn capture(project_root: &Path) -> Result<Self> {
        let output = approval_git_command()
            .arg("-C")
            .arg(project_root)
            .args(["rev-parse", "--git-path", "index"])
            .output()
            .await?;
        let path = git_stdout(output, "resolve canonical Git index")?;
        let path = PathBuf::from(path);
        let path = if path.is_absolute() {
            path
        } else {
            project_root.join(path)
        };
        let contents = match tokio::fs::read(&path).await {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to read Git index {}", path.display()));
            }
        };
        Ok(Self { path, contents })
    }

    async fn restore(&self) -> Result<()> {
        let path = self.path.clone();
        let contents = self.contents.clone();
        tokio::task::spawn_blocking(move || restore_index_file(&path, contents.as_deref())).await?
    }
}

fn restore_index_file(path: &Path, contents: Option<&[u8]>) -> Result<()> {
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock_path = PathBuf::from(lock_path);
    let mut lock = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .with_context(|| format!("Failed to lock Git index {}", path.display()))?;
    let result = (|| -> Result<()> {
        if let Some(contents) = contents {
            lock.write_all(contents)
                .with_context(|| format!("Failed to restore Git index {}", path.display()))?;
            lock.sync_all()
                .with_context(|| format!("Failed to sync Git index {}", path.display()))?;
        }

        drop(lock);

        if contents.is_some() {
            if path.exists() {
                std::fs::remove_file(path)
                    .with_context(|| format!("Failed to replace Git index {}", path.display()))?;
            }

            std::fs::rename(&lock_path, path).with_context(|| {
                format!("Failed to publish restored Git index {}", path.display())
            })?;
        } else {
            if path.exists() {
                std::fs::remove_file(path)
                    .with_context(|| format!("Failed to remove Git index {}", path.display()))?;
            }

            std::fs::remove_file(&lock_path).with_context(|| {
                format!(
                    "Failed to remove temporary Git index lock {}",
                    lock_path.display()
                )
            })?;
        }

        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&lock_path);
    }

    result
}

async fn run_post_apply_integration_checks(
    context: &RuntimeTaskContext,
    config: &Config,
    project_root: &Path,
) -> Result<()> {
    if config.checks.commands.is_empty() {
        info!("No check commands configured; skipping post-approve integration gate");
        return Ok(());
    }

    info!("Running post-approve integration gate");
    let attempt = context.check_retries + 1;
    let log_scope = context
        .run_id
        .as_deref()
        .unwrap_or(context.task_id.as_str());
    match check_gate::run_in(config, attempt, log_scope, project_root).await? {
        CheckGateResult::Passed => {
            project::record_runtime_event_best_effort(
                context.run_id.clone(),
                "approve_integration_check_passed",
                serde_json::json!({
                    "task_id": context.task_id.as_str(),
                    "commands": config.checks.commands.len(),
                }),
            )
            .await;
            Ok(())
        }
        CheckGateResult::Failed(failure) => {
            let reason = format!(
                "Cannot approve task {} because configured checks failed after applying its patch to the canonical workspace.\n\n{}",
                context.task_id, failure.report
            );
            write_integration_error(context, &reason).await?;
            project::record_task_integration_failed_best_effort(
                &context.task_id,
                context.run_id.as_deref(),
                &failure.failure_reason,
            )
            .await;
            project::record_runtime_event_best_effort(
                context.run_id.clone(),
                "approve_integration_check_failed",
                serde_json::json!({
                    "task_id": context.task_id.as_str(),
                    "failure_reason": failure.failure_reason,
                }),
            )
            .await;
            anyhow::bail!(
                "{reason}\n\nIntegration error was saved to {}/INTEGRATION_ERROR.md. \
                 The task remains in review and the applied patch was rolled back; reject this review with the check details so an Executor can address it.",
                context.run_dir
            );
        }
    }
}

async fn write_integration_error(context: &RuntimeTaskContext, reason: &str) -> Result<()> {
    let content = format!(
        "# Integration Error\n\nTask: {}\n\n{}\n\nSuggested next step: call `/reject` with these conflict details so the Executor can rebase or adjust the patch.\n",
        context.task_id, reason
    );
    store::write_integration_error_for_run_dir(&context.run_dir, &content).await
}

// The held owner-file lock is authoritative; marker PIDs can be reused after a crash.

async fn cleanup_approved_workspace_best_effort(context: &RuntimeTaskContext, project_root: &Path) {
    if let Err(err) = cleanup_approved_workspace(context, project_root).await {
        tracing::warn!(
            error = ?err,
            task_id = context.task_id,
            workspace_path = context.workspace_path.as_deref(),
            "failed to remove approved task worktree"
        );
    }
}

async fn cleanup_approved_workspace(
    context: &RuntimeTaskContext,
    project_root: &Path,
) -> Result<bool> {
    let local_ref_content =
        tokio::fs::read_to_string(store::resolve_project_path(".ferrus/project.toml")).await?;
    let local_ref: project::LocalProjectRef = toml::from_str(&local_ref_content)?;
    let data_dir = PathBuf::from(local_ref.data_dir);
    let managed_root = data_dir.join("worktrees");
    let task_workspace = managed_root.join(&context.task_id);
    let canonical_workspace = tokio::fs::canonicalize(&task_workspace)
        .await
        .unwrap_or(task_workspace);
    let canonical_managed_root = tokio::fs::canonicalize(&managed_root)
        .await
        .unwrap_or(managed_root);
    if !is_managed_workspace_path(&canonical_workspace, &canonical_managed_root) {
        return Ok(false);
    }

    if tokio::fs::try_exists(&canonical_workspace).await? {
        let output = approval_git_command()
            .arg("-C")
            .arg(project_root)
            .args(["worktree", "remove", "--force"])
            .arg(&canonical_workspace)
            .output()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            anyhow::bail!(
                "Failed to remove approved task worktree at {}: {}",
                canonical_workspace.display(),
                if stderr.is_empty() {
                    output.status.to_string()
                } else {
                    stderr
                }
            );
        }
    }

    project::remove_executor_baseline(project_root, &data_dir, &context.task_id).await?;
    Ok(true)
}

fn canonical_approval_path(project_root: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    }
}

fn is_managed_workspace_path(path: &Path, managed_root: &Path) -> bool {
    path.starts_with(managed_root) && path != managed_root
}

mod lock;
pub(crate) use lock::*;

mod integration;
use integration::*;

#[cfg(test)]
#[path = "approve_tests.rs"]
mod tests;
