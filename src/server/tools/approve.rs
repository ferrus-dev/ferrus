use anyhow::{Context, Result};
use fs2::FileExt;
use neva::prelude::*;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
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

    let (integration_result, refresh_canonical_graph) = {
        let _approval_lock = acquire_canonical_approval_lock(&context).await?;
        let observer = CanonicalIntegrationObserver::capture(&project_root).await;
        let integration_result = integrate_approved_task(&context, &project_root).await;
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

async fn integrate_approved_task(context: &RuntimeTaskContext, project_root: &Path) -> Result<()> {
    let patch_applied = apply_approved_patch(context, project_root).await?;
    if patch_applied {
        // The approved patch may have changed ferrus.toml, so run the integration
        // gate against the configuration as it exists in the post-apply repository
        // state rather than the config loaded before the patch was applied.
        let post_apply_config = match Config::load_from(project_root).await {
            Ok(config) => config,
            Err(err) => {
                if let Err(rollback_err) = rollback_approved_patch(context, project_root).await {
                    anyhow::bail!(
                        "{err}\n\nAdditionally failed to roll back the already-applied task patch: {rollback_err}"
                    );
                }
                return Err(err);
            }
        };
        let integration_checks =
            run_post_apply_integration_checks(context, &post_apply_config, project_root).await;
        if let Err(err) = integration_checks {
            if let Err(rollback_err) = rollback_approved_patch(context, project_root).await {
                anyhow::bail!(
                    "{err}\n\nAdditionally failed to roll back the already-applied task patch: {rollback_err}"
                );
            }
            return Err(err);
        }
    }
    let transition = async {
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
        if patch_applied
            && let Err(rollback_err) = rollback_approved_patch(context, project_root).await
        {
            anyhow::bail!(
                "{err}\n\nAdditionally failed to roll back the already-applied task patch: {rollback_err}"
            );
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

async fn apply_approved_patch(context: &RuntimeTaskContext, project_root: &Path) -> Result<bool> {
    let patch = store::read_patch_for_run_dir(&context.run_dir).await?;
    if patch.trim().is_empty() {
        return Ok(false);
    }

    let patch_path = store::resolve_project_path(Path::new(&context.run_dir).join("PATCH.diff"));
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["apply", "--whitespace=nowarn"])
        .arg(&patch_path)
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let reason = format!(
            "Cannot approve task {} because its patch could not be applied to {}: {}",
            context.task_id,
            project_root.display(),
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        );
        write_integration_error(context, &reason).await?;
        project::record_task_integration_failed_best_effort(
            &context.task_id,
            context.run_id.as_deref(),
            &reason,
        )
        .await;
        anyhow::bail!(
            "{reason}\n\nIntegration error was saved to {}/INTEGRATION_ERROR.md. \
             Reject this review with the conflict details so an Executor can address it.",
            context.run_dir
        );
    }
    Ok(true)
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

async fn rollback_approved_patch(context: &RuntimeTaskContext, project_root: &Path) -> Result<()> {
    let patch_path = store::resolve_project_path(Path::new(&context.run_dir).join("PATCH.diff"));
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["apply", "-R", "--whitespace=nowarn"])
        .arg(&patch_path)
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "Failed to roll back approved patch for task {} in {}: {}",
            context.task_id,
            project_root.display(),
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        );
    }
    Ok(())
}

async fn write_integration_error(context: &RuntimeTaskContext, reason: &str) -> Result<()> {
    let content = format!(
        "# Integration Error\n\nTask: {}\n\n{}\n\nSuggested next step: call `/reject` with these conflict details so the Executor can rebase or adjust the patch.\n",
        context.task_id, reason
    );
    store::write_integration_error_for_run_dir(&context.run_dir, &content).await
}

struct CanonicalApprovalLock {
    path: PathBuf,
    _file: std::fs::File,
}

struct CanonicalApprovalLockGuard {
    _file: std::fs::File,
}

impl Drop for CanonicalApprovalLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn acquire_canonical_approval_lock(
    context: &RuntimeTaskContext,
) -> Result<CanonicalApprovalLock> {
    let local_ref_content =
        tokio::fs::read_to_string(store::resolve_project_path(".ferrus/project.toml")).await?;
    let local_ref: project::LocalProjectRef = toml::from_str(&local_ref_content)?;
    let lock_path = PathBuf::from(local_ref.data_dir).join("canonical-approval.lock");
    acquire_canonical_approval_lock_at(&lock_path, &context.task_id).await
}

async fn acquire_canonical_approval_lock_at(
    lock_path: &Path,
    task_id: &str,
) -> Result<CanonicalApprovalLock> {
    if let Some(parent) = lock_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    let _guard = acquire_canonical_approval_lock_guard(lock_path)?;
    loop {
        match try_create_canonical_approval_lock(lock_path, task_id).await {
            Ok(Some(lock)) => return Ok(lock),
            Ok(None) => {
                if remove_stale_canonical_approval_lock(lock_path).await? {
                    continue;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(err) => return Err(err),
        }
    }
}

fn acquire_canonical_approval_lock_guard(lock_path: &Path) -> Result<CanonicalApprovalLockGuard> {
    let guard_path = canonical_approval_lock_guard_path(lock_path);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&guard_path)
        .with_context(|| {
            format!(
                "Failed to open canonical approval lock guard {}",
                guard_path.display()
            )
        })?;
    file.lock_exclusive().with_context(|| {
        format!(
            "Failed to acquire canonical approval lock guard {}",
            guard_path.display()
        )
    })?;
    Ok(CanonicalApprovalLockGuard { _file: file })
}

fn canonical_approval_lock_guard_path(lock_path: &Path) -> PathBuf {
    lock_path.with_file_name(".canonical-approval.lock.guard")
}

async fn try_create_canonical_approval_lock(
    lock_path: &Path,
    task_id: &str,
) -> Result<Option<CanonicalApprovalLock>> {
    let temp_path = canonical_approval_lock_temp_path(lock_path, task_id);
    let content = format!("pid={}\ntask_id={task_id}\n", std::process::id());
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .with_context(|| {
            format!(
                "Failed to create temporary canonical approval lock {}",
                temp_path.display()
            )
        })?;
    if let Err(err) = file.write_all(content.as_bytes()) {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(err).with_context(|| {
            format!(
                "Failed to write temporary canonical approval lock {}",
                temp_path.display()
            )
        });
    }
    drop(file);

    match std::fs::hard_link(&temp_path, lock_path) {
        Ok(()) => {
            let owner_file = match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(lock_path)
                .and_then(|file| {
                    file.lock_exclusive()?;
                    Ok(file)
                }) {
                Ok(file) => file,
                Err(err) => {
                    let _ = tokio::fs::remove_file(&temp_path).await;
                    let _ = tokio::fs::remove_file(lock_path).await;
                    return Err(err).with_context(|| {
                        format!(
                            "Failed to hold canonical approval lock {}",
                            lock_path.display()
                        )
                    });
                }
            };
            let _ = tokio::fs::remove_file(&temp_path).await;
            Ok(Some(CanonicalApprovalLock {
                path: lock_path.to_path_buf(),
                _file: owner_file,
            }))
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            Ok(None)
        }
        Err(err) => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            Err(err).with_context(|| {
                format!(
                    "Failed to publish canonical approval lock {}",
                    lock_path.display()
                )
            })
        }
    }
}

fn canonical_approval_lock_temp_path(lock_path: &Path, task_id: &str) -> PathBuf {
    let task_id = task_id.replace(std::path::MAIN_SEPARATOR, "_");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    lock_path.with_file_name(format!(
        ".canonical-approval.lock.{}.{}.{}.tmp",
        std::process::id(),
        task_id,
        nonce
    ))
}

async fn remove_stale_canonical_approval_lock(lock_path: &Path) -> Result<bool> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "Failed to open canonical approval lock {}",
                    lock_path.display()
                )
            });
        }
    };
    match file.try_lock_exclusive() {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "Failed to inspect canonical approval lock {}",
                    lock_path.display()
                )
            });
        }
    }
    drop(file);

    match tokio::fs::remove_file(lock_path).await {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(err) => Err(err).with_context(|| {
            format!(
                "Failed to remove stale canonical approval lock {}",
                lock_path.display()
            )
        }),
    }
}

#[cfg(test)]
fn canonical_approval_lock_pid(contents: &str) -> Option<u32> {
    contents.lines().find_map(|line| {
        line.strip_prefix("pid=")
            .and_then(|value| value.parse().ok())
    })
}

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
        let output = Command::new("git")
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

#[cfg(test)]
#[path = "approve_tests.rs"]
mod tests;
