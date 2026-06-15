use anyhow::Result;
use neva::prelude::*;
use std::path::{Path, PathBuf};
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

pub const DESCRIPTION: &str = "Approve the current submission. Transitions state Reviewing → Complete. \
     Must be called after /review_pending.";

pub async fn handler(ctx: neva::Context) -> Result<String, Error> {
    let agent_id = super::agent_id_from_context(&ctx)?;
    handler_for_agent(&agent_id).await
}

pub async fn handler_for_agent(agent_id: &str) -> Result<String, Error> {
    run(agent_id).await.map_err(tool_err)
}

async fn run(agent_id: &str) -> Result<String> {
    let config = Config::load().await?;
    let context = require_runtime_task_context(agent_id).await?;

    if context.status.parse::<project::TaskStatus>()? != project::TaskStatus::Reviewing {
        anyhow::bail!(
            "Cannot approve from state {}. Call /review_pending first.",
            context.status
        );
    }
    ensure_lease_owner_or_reclaim(agent_id, config.lease.ttl_secs).await?;

    let patch_applied = apply_approved_patch(&context).await?;
    if patch_applied {
        let integration_checks = run_post_apply_integration_checks(&context, &config).await;
        if let Err(err) = integration_checks {
            if let Err(rollback_err) = rollback_approved_patch(&context).await {
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
            specs::complete_milestone(spec_path, milestone_id).await?;
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
        if patch_applied && let Err(rollback_err) = rollback_approved_patch(&context).await {
            anyhow::bail!(
                "{err}\n\nAdditionally failed to roll back the already-applied task patch: {rollback_err}"
            );
        }
        return Err(err);
    }
    cleanup_approved_workspace_best_effort(&context).await;
    project::record_runtime_event_best_effort(
        context.run_id.clone(),
        "approved",
        serde_json::json!({
            "task_id": context.task_id.as_str(),
        }),
    )
    .await;

    info!("Task approved, state → Complete");
    Ok("Task approved. State: Complete. Well done!".to_string())
}

async fn apply_approved_patch(context: &RuntimeTaskContext) -> Result<bool> {
    let patch = store::read_patch_for_run_dir(&context.run_dir).await?;
    if patch.trim().is_empty() {
        return Ok(false);
    }

    let project_root = std::env::current_dir()?;
    let patch_path = store::resolve_project_path(Path::new(&context.run_dir).join("PATCH.diff"));
    let output = Command::new("git")
        .arg("-C")
        .arg(&project_root)
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
) -> Result<()> {
    if config.checks.commands.is_empty() {
        info!("No check commands configured; skipping post-approve integration gate");
        return Ok(());
    }

    info!("Running post-approve integration gate");
    let attempt = context.check_retries + 1;
    match check_gate::run(config, attempt).await? {
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

async fn rollback_approved_patch(context: &RuntimeTaskContext) -> Result<()> {
    let project_root = std::env::current_dir()?;
    let patch_path = store::resolve_project_path(Path::new(&context.run_dir).join("PATCH.diff"));
    let output = Command::new("git")
        .arg("-C")
        .arg(&project_root)
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

async fn cleanup_approved_workspace_best_effort(context: &RuntimeTaskContext) {
    if let Err(err) = cleanup_approved_workspace(context).await {
        tracing::warn!(
            error = ?err,
            task_id = context.task_id,
            workspace_path = context.workspace_path.as_deref(),
            "failed to remove approved task worktree"
        );
    }
}

async fn cleanup_approved_workspace(context: &RuntimeTaskContext) -> Result<bool> {
    let Some(workspace_path) = context
        .workspace_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
    else {
        return Ok(false);
    };
    if !tokio::fs::try_exists(&workspace_path).await? {
        return Ok(false);
    }

    let local_ref_content =
        tokio::fs::read_to_string(store::resolve_project_path(".ferrus/project.toml")).await?;
    let local_ref: project::LocalProjectRef = toml::from_str(&local_ref_content)?;
    let managed_root = PathBuf::from(local_ref.data_dir).join("worktrees");
    let canonical_workspace = tokio::fs::canonicalize(&workspace_path)
        .await
        .unwrap_or(workspace_path);
    let canonical_managed_root = tokio::fs::canonicalize(&managed_root)
        .await
        .unwrap_or(managed_root);
    if !is_managed_workspace_path(&canonical_workspace, &canonical_managed_root) {
        return Ok(false);
    }

    let project_root = std::env::current_dir()?;
    let output = Command::new("git")
        .arg("-C")
        .arg(&project_root)
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
    Ok(true)
}

fn is_managed_workspace_path(path: &Path, managed_root: &Path) -> bool {
    path.starts_with(managed_root) && path != managed_root
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::store;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus/tasks")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        tokio::fs::write(
            "ferrus.toml",
            "[checks]\ncommands = []\n\n[limits]\nmax_check_retries = 20\nmax_review_cycles = 3\nmax_feedback_lines = 30\nwait_timeout_secs = 1\n\n[lease]\nttl_secs = 60\n",
        )
        .await
        .unwrap();
        let data_dir = dir.path().join(".ferrus/projects/test-project");
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        let local_ref = crate::project::LocalProjectRef {
            project_id: "test-project".to_string(),
            name: "test".to_string(),
            data_dir: data_dir.to_string_lossy().into_owned(),
        };
        tokio::fs::write(
            ".ferrus/project.toml",
            toml::to_string_pretty(&local_ref).unwrap(),
        )
        .await
        .unwrap();
        (dir, previous)
    }

    fn teardown(previous: std::path::PathBuf) {
        std::env::set_current_dir(previous).unwrap();
    }

    #[tokio::test]
    async fn approve_updates_agent_review_task_in_database() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::Reviewing,
        )
        .await
        .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "supervisor:codex:7", 60)
            .await
            .unwrap();
        run("supervisor:codex:7").await.unwrap();

        crate::test_support::assert_no_state_json();
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "complete");
        assert_eq!(task.claimed_by, None);

        teardown(previous);
    }

    #[tokio::test]
    async fn approve_uses_database_context_when_state_json_is_absent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::Reviewing,
        )
        .await
        .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "supervisor:codex:7", 60)
            .await
            .unwrap();

        run("supervisor:codex:7").await.unwrap();

        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "complete");
        assert_eq!(task.claimed_by, None);

        teardown(previous);
    }

    #[tokio::test]
    async fn approve_applies_scoped_patch_before_marking_task_complete() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup().await;
        if !git(dir.path(), ["init"]).success() {
            teardown(previous);
            return;
        }
        tokio::fs::write("file.txt", "old\n").await.unwrap();
        assert!(git(dir.path(), ["add", "file.txt"]).success());
        assert!(
            git(
                dir.path(),
                [
                    "-c",
                    "user.email=ferrus@example.invalid",
                    "-c",
                    "user.name=Ferrus",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-m",
                    "initial",
                ],
            )
            .success()
        );
        tokio::fs::write("file.txt", "new\n").await.unwrap();
        let patch = git_output(dir.path(), ["diff", "--binary", "HEAD", "--", "file.txt"]);
        tokio::fs::write("file.txt", "old\n").await.unwrap();
        assert!(!patch.trim().is_empty());

        store::write_patch_for_run_dir(".ferrus/runs/t-007", &patch)
            .await
            .unwrap();
        crate::project::record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::Reviewing,
        )
        .await
        .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "supervisor:codex:7", 60)
            .await
            .unwrap();

        run("supervisor:codex:7").await.unwrap();

        let file = tokio::fs::read_to_string("file.txt").await.unwrap();
        assert_eq!(file.replace("\r\n", "\n"), "new\n");
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "complete");

        teardown(previous);
    }

    #[tokio::test]
    async fn approve_patch_conflict_records_recoverable_integration_error() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup().await;
        if !git(dir.path(), ["init"]).success() {
            teardown(previous);
            return;
        }
        tokio::fs::write("file.txt", "old\n").await.unwrap();
        assert!(git(dir.path(), ["add", "file.txt"]).success());
        assert!(
            git(
                dir.path(),
                [
                    "-c",
                    "user.email=ferrus@example.invalid",
                    "-c",
                    "user.name=Ferrus",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-m",
                    "initial",
                ],
            )
            .success()
        );
        tokio::fs::write("file.txt", "new\n").await.unwrap();
        let patch = git_output(dir.path(), ["diff", "--binary", "HEAD", "--", "file.txt"]);
        tokio::fs::write("file.txt", "conflicting local change\n")
            .await
            .unwrap();
        assert!(!patch.trim().is_empty());

        store::write_patch_for_run_dir(".ferrus/runs/t-007", &patch)
            .await
            .unwrap();
        crate::project::record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::Reviewing,
        )
        .await
        .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "supervisor:codex:7", 60)
            .await
            .unwrap();

        let error = run("supervisor:codex:7").await.unwrap_err().to_string();

        assert!(error.contains("INTEGRATION_ERROR.md"));
        assert!(error.contains("Reject this review"));
        let integration_error = store::read_integration_error_for_run_dir(".ferrus/runs/t-007")
            .await
            .unwrap();
        assert!(integration_error.contains("Cannot approve task t-007"));
        assert!(integration_error.contains("Suggested next step"));
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "reviewing");
        assert!(
            task.failure_reason
                .as_deref()
                .is_some_and(|reason| { reason.contains("patch could not be applied") })
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn approve_rolls_back_patch_when_post_apply_checks_fail() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup().await;
        tokio::fs::write(
            "ferrus.toml",
            "[checks]\ncommands = [\"git grep -q base -- file.txt\"]\n\n[limits]\nmax_check_retries = 20\nmax_review_cycles = 3\nmax_feedback_lines = 30\nwait_timeout_secs = 1\n\n[lease]\nttl_secs = 60\n",
        )
        .await
        .unwrap();
        if !git(dir.path(), ["init"]).success() {
            teardown(previous);
            return;
        }
        tokio::fs::write("file.txt", "base\n").await.unwrap();
        assert!(git(dir.path(), ["add", "file.txt"]).success());
        assert!(
            git(
                dir.path(),
                [
                    "-c",
                    "user.email=ferrus@example.invalid",
                    "-c",
                    "user.name=Ferrus",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-m",
                    "initial",
                ],
            )
            .success()
        );
        tokio::fs::write("file.txt", "broken\n").await.unwrap();
        let patch = git_output(dir.path(), ["diff", "--binary", "HEAD", "--", "file.txt"]);
        tokio::fs::write("file.txt", "base\n").await.unwrap();
        assert!(!patch.trim().is_empty());

        store::write_patch_for_run_dir(".ferrus/runs/t-007", &patch)
            .await
            .unwrap();
        crate::project::record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::Reviewing,
        )
        .await
        .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "supervisor:codex:7", 60)
            .await
            .unwrap();

        let error = run("supervisor:codex:7").await.unwrap_err().to_string();

        assert!(error.contains("configured checks failed"));
        assert!(error.contains("rolled back"));
        let file = tokio::fs::read_to_string("file.txt").await.unwrap();
        assert_eq!(file.replace("\r\n", "\n"), "base\n");
        let integration_error = store::read_integration_error_for_run_dir(".ferrus/runs/t-007")
            .await
            .unwrap();
        assert!(integration_error.contains("configured checks failed"));
        assert!(integration_error.contains("git grep -q base -- file.txt"));
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "reviewing");
        assert!(
            task.failure_reason
                .as_deref()
                .is_some_and(|reason| { reason.contains("Commands failed") })
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn approve_keeps_task_reviewing_when_spec_update_fails() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        tokio::fs::create_dir_all("docs/specs/spec.md")
            .await
            .unwrap();
        crate::project::record_task_status_with_origin(
            "t-009",
            ".ferrus/tasks/t-009.md",
            crate::project::TaskStatus::Reviewing,
            Some("docs/specs/spec.md"),
            Some("m1.0"),
        )
        .await
        .unwrap();
        crate::project::claim_task("t-009", ".ferrus/tasks/t-009.md", "supervisor:codex:9", 60)
            .await
            .unwrap();

        let error = run("supervisor:codex:9").await.unwrap_err().to_string();

        assert!(error.contains("docs/specs/spec.md"));
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-009").unwrap();
        assert_eq!(task.status, "reviewing");
        assert_eq!(task.claimed_by.as_deref(), Some("supervisor:codex:9"));

        teardown(previous);
    }

    #[tokio::test]
    async fn approve_rolls_back_patch_when_spec_update_fails() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup().await;
        if !git(dir.path(), ["init"]).success() {
            teardown(previous);
            return;
        }
        tokio::fs::write("file.txt", "old\n").await.unwrap();
        assert!(git(dir.path(), ["add", "file.txt"]).success());
        assert!(
            git(
                dir.path(),
                [
                    "-c",
                    "user.email=ferrus@example.invalid",
                    "-c",
                    "user.name=Ferrus",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-m",
                    "initial",
                ],
            )
            .success()
        );
        tokio::fs::write("file.txt", "new\n").await.unwrap();
        let patch = git_output(dir.path(), ["diff", "--binary", "HEAD", "--", "file.txt"]);
        tokio::fs::write("file.txt", "old\n").await.unwrap();
        assert!(!patch.trim().is_empty());
        tokio::fs::create_dir_all("docs/specs/spec.md")
            .await
            .unwrap();

        store::write_patch_for_run_dir(".ferrus/runs/t-010", &patch)
            .await
            .unwrap();
        crate::project::record_task_status_with_origin(
            "t-010",
            ".ferrus/tasks/t-010.md",
            crate::project::TaskStatus::Reviewing,
            Some("docs/specs/spec.md"),
            Some("m1.0"),
        )
        .await
        .unwrap();
        crate::project::claim_task("t-010", ".ferrus/tasks/t-010.md", "supervisor:codex:10", 60)
            .await
            .unwrap();

        let error = run("supervisor:codex:10").await.unwrap_err().to_string();

        assert!(error.contains("docs/specs/spec.md"));
        let file = tokio::fs::read_to_string("file.txt").await.unwrap();
        assert_eq!(file.replace("\r\n", "\n"), "old\n");
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-010").unwrap();
        assert_eq!(task.status, "reviewing");
        assert_eq!(task.claimed_by.as_deref(), Some("supervisor:codex:10"));

        teardown(previous);
    }

    #[tokio::test]
    async fn approve_removes_managed_worktree_after_completion() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup().await;
        if !git(dir.path(), ["init"]).success() {
            teardown(previous);
            return;
        }
        tokio::fs::write("file.txt", "base\n").await.unwrap();
        assert!(git(dir.path(), ["add", "file.txt"]).success());
        assert!(
            git(
                dir.path(),
                [
                    "-c",
                    "user.email=ferrus@example.invalid",
                    "-c",
                    "user.name=Ferrus",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-m",
                    "initial",
                ],
            )
            .success()
        );

        let workspace_path = dir
            .path()
            .join(".ferrus/projects/test-project/worktrees/t-007");
        assert!(
            git_path(
                dir.path(),
                ["worktree", "add", "--detach"],
                &workspace_path,
                ["HEAD"],
            )
            .success()
        );
        assert!(workspace_path.is_dir());

        crate::project::record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::Reviewing,
        )
        .await
        .unwrap();
        let run_record = crate::project::record_run_started_with_workspace(
            "supervisor-run-t-007",
            "supervisor",
            "supervisor:codex:7",
            std::process::id(),
            workspace_path.to_string_lossy().into_owned(),
        )
        .await
        .unwrap();
        let attached = crate::project::attach_running_run_to_task(
            "supervisor:codex:7",
            "t-007",
            ".ferrus/tasks/t-007.md",
        )
        .await
        .unwrap();
        assert_eq!(attached.as_deref(), Some(run_record.id.as_str()));
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "supervisor:codex:7", 60)
            .await
            .unwrap();
        let context = crate::project::runtime_task_context_for_agent("supervisor:codex:7")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            context.workspace_path.as_deref(),
            Some(workspace_path.to_string_lossy().as_ref())
        );

        run("supervisor:codex:7").await.unwrap();

        assert!(!workspace_path.exists());
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "complete");

        teardown(previous);
    }

    fn git<const N: usize>(cwd: &std::path::Path, args: [&str; N]) -> std::process::ExitStatus {
        std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .status()
            .unwrap()
    }

    fn git_path<const N: usize, const M: usize>(
        cwd: &std::path::Path,
        before: [&str; N],
        path: &std::path::Path,
        after: [&str; M],
    ) -> std::process::ExitStatus {
        std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(before)
            .arg(path)
            .args(after)
            .status()
            .unwrap()
    }

    fn git_output<const N: usize>(cwd: &std::path::Path, args: [&str; N]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}
