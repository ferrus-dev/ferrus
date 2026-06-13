use anyhow::Result;
use neva::prelude::*;
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use tokio::process::Command;
use tracing::info;

use crate::{
    agent_id::{ENV_BASELINE_TREE, ENV_PROJECT_ROOT},
    config::Config,
    project::{self, RuntimeTaskContext, TaskCheckFailure},
    state::store,
};

use super::{
    check_gate::{self, CheckGateResult},
    ensure_lease_owner_or_reclaim, require_runtime_task_context, tool_err,
};

static TEMP_INDEX_COUNTER: AtomicU64 = AtomicU64::new(0);

pub const DESCRIPTION: &str = "\
Run the final check gate and, if it passes, submit work for Supervisor review. \
Can be called from Executing or Addressing. \
On pass: state → Reviewing. On fail: stay in the current work state (or state \
→ Failed if the retry limit is exhausted).

The `content` parameter must be a Markdown document with the following sections:

## Summary
Brief description of what was changed and why.

## How to verify manually
Step-by-step instructions for the Supervisor to spot-check the work.

## Known limitations
Anything deliberately left out, edge cases not handled, or follow-up work needed. \
Omit this section if there are none.";

pub const INPUT_SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "content": {
            "type": "string",
            "description": "Submission notes in Markdown (summary, how to verify, known limitations)"
        }
    },
    "required": ["content"]
}"#;

pub async fn handler_for_agent(agent_id: &str, content: String) -> Result<String, Error> {
    run(Some(agent_id), content).await.map_err(tool_err)
}

async fn run(agent_id: Option<&str>, content: String) -> Result<String> {
    let Some(agent_id) = agent_id else {
        anyhow::bail!("Cannot submit without an agent runtime context");
    };
    let config = Config::load().await?;
    let context = require_runtime_task_context(agent_id).await?;

    if !context
        .status
        .parse::<project::TaskStatus>()?
        .is_executor_working()
    {
        anyhow::bail!(
            "Cannot submit from state {}. Submit is only valid from Executing or Addressing after the implementation is ready.",
            context.status
        );
    }
    ensure_lease_owner_or_reclaim(agent_id, config.lease.ttl_secs).await?;

    if config.checks.commands.is_empty() {
        info!("No check commands configured; treating final check gate as pass");
        project::record_task_check_passed(&context.task_id).await?;
        write_submission(&context, &content).await?;
        write_submission_patch(&context).await?;
        record_task_status(&context, project::TaskStatus::Reviewing).await?;
        project::record_runtime_event_best_effort(
            context.run_id.clone(),
            "submitted",
            serde_json::json!({ "content_bytes": content.len(), "check_gate": "skipped" }),
        )
        .await;

        return Ok(
            "Submitted for review. Warning: no check commands are configured in ferrus.toml, so the final check gate was treated as a pass. State: Reviewing."
                .to_string(),
        );
    }

    info!("Running final check gate before review submission");
    let attempt = context.check_retries + 1;
    match check_gate::run(&config, attempt).await? {
        CheckGateResult::Passed => {
            project::record_task_check_passed(&context.task_id).await?;
            write_submission(&context, &content).await?;
            write_submission_patch(&context).await?;
            record_task_status(&context, project::TaskStatus::Reviewing).await?;
            project::record_runtime_event_best_effort(
                context.run_id.clone(),
                "submitted",
                serde_json::json!({ "content_bytes": content.len(), "check_gate": "passed" }),
            )
            .await;

            info!("Work submitted for review, state → Reviewing");
            Ok(
                "Submitted for review. State: Reviewing. The Supervisor can now call /review_pending."
                    .to_string(),
            )
        }
        CheckGateResult::Failed(failure) => {
            match project::record_task_check_failed(
                &context.task_id,
                &failure.failure_reason,
                config.limits.max_check_retries,
            )
            .await?
            {
                TaskCheckFailure::Failed { retries } => {
                    project::record_runtime_event_best_effort(
                        context.run_id.clone(),
                        "submit_check_failed",
                        serde_json::json!({
                            "task_id": context.task_id,
                            "retries": retries,
                            "max_retries": config.limits.max_check_retries,
                            "state": context.status,
                        }),
                    )
                    .await;
                    Ok(format!(
                        "Final review gate failed during /submit (retry {}/{}).\n\n{}\n\nState remains {}. Fix the issues and run /check or /submit again.",
                        retries, config.limits.max_check_retries, failure.report, context.status,
                    ))
                }
                TaskCheckFailure::LimitExceeded { retries } => {
                    project::record_runtime_event_best_effort(
                        context.run_id.clone(),
                        "submit_check_limit_exceeded",
                        serde_json::json!({
                            "task_id": context.task_id,
                            "retries": retries,
                            "max_retries": config.limits.max_check_retries,
                        }),
                    )
                    .await;
                    Ok(format!(
                        "Final review gate failed during /submit and hit the retry limit ({retries}/{}).\n\n{}\n\nState is now Failed. A human must call /reset to recover.",
                        config.limits.max_check_retries, failure.report,
                    ))
                }
            }
        }
    }
}

async fn write_submission(context: &RuntimeTaskContext, content: &str) -> Result<()> {
    store::write_submission_for_run_dir(&context.run_dir, content).await
}

async fn write_submission_patch(context: &RuntimeTaskContext) -> Result<()> {
    if !is_isolated_executor_workspace(context).await {
        return Ok(());
    }

    let patch = workspace_patch().await?;
    store::write_patch_for_run_dir(&context.run_dir, &patch).await
}

async fn is_isolated_executor_workspace(context: &RuntimeTaskContext) -> bool {
    let Some(project_root) = std::env::var(ENV_PROJECT_ROOT)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    let current_dir = std::env::current_dir().ok();
    let workspace_path = context
        .workspace_path
        .as_deref()
        .map(Path::new)
        .map(|path| path.to_path_buf())
        .or(current_dir);
    let Some(workspace_path) = workspace_path else {
        return false;
    };
    !equivalent_paths(&workspace_path, Path::new(&project_root)).await
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

async fn workspace_patch() -> Result<String> {
    let baseline = std::env::var(ENV_BASELINE_TREE)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "HEAD".to_string());
    workspace_patch_against_baseline(&baseline).await
}

async fn workspace_patch_against_baseline(baseline: &str) -> Result<String> {
    let temp_index = temporary_index_path().await?;
    let patch = workspace_patch_against_baseline_with_temp_index(baseline, &temp_index).await;
    let _ = tokio::fs::remove_file(&temp_index).await;
    patch
}

async fn workspace_patch_against_baseline_with_temp_index(
    baseline: &str,
    temp_index: &Path,
) -> Result<String> {
    run_git_with_temp_index(temp_index, ["read-tree", baseline]).await?;
    run_git_with_temp_index(temp_index, ["add", "-A", "."]).await?;
    let tree = git_output_with_temp_index(temp_index, ["write-tree"]).await?;
    let output = Command::new("git")
        .args(["diff", "--binary"])
        .arg(baseline)
        .arg(tree.trim())
        .arg("--")
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "Failed to capture executor workspace patch: {}",
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn temporary_index_path() -> Result<PathBuf> {
    let git_dir = git_output(["rev-parse", "--git-dir"]).await?;
    let counter = TEMP_INDEX_COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(PathBuf::from(git_dir.trim()).join(format!(
        "ferrus-submit-index-{}-{counter}",
        std::process::id()
    )))
}

async fn run_git_with_temp_index<const N: usize>(temp_index: &Path, args: [&str; N]) -> Result<()> {
    let output = Command::new("git")
        .env("GIT_INDEX_FILE", temp_index)
        .args(args)
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "Failed to capture executor workspace patch: {}",
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        );
    }
    Ok(())
}

async fn git_output<const N: usize>(args: [&str; N]) -> Result<String> {
    let output = Command::new("git").args(args).output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "Failed to capture executor workspace patch: {}",
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn git_output_with_temp_index<const N: usize>(
    temp_index: &Path,
    args: [&str; N],
) -> Result<String> {
    let output = Command::new("git")
        .env("GIT_INDEX_FILE", temp_index)
        .args(args)
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "Failed to capture executor workspace patch: {}",
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn record_task_status(
    context: &RuntimeTaskContext,
    status: project::TaskStatus,
) -> Result<()> {
    project::record_task_status(&context.task_id, &context.task_path, status).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
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
        tokio::fs::write(
            "ferrus.toml",
            "[checks]\ncommands = []\n\n[limits]\nmax_check_retries = 20\nmax_review_cycles = 3\nmax_feedback_lines = 30\nwait_timeout_secs = 60\n",
        )
        .await
        .unwrap();
        (dir, previous)
    }

    fn teardown(previous: std::path::PathBuf) {
        std::env::set_current_dir(previous).unwrap();
    }

    #[tokio::test]
    async fn workspace_patch_excludes_seeded_baseline_changes() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        std::process::Command::new("git")
            .args(["init"])
            .status()
            .unwrap();
        tokio::fs::write("tracked.txt", "base\n").await.unwrap();
        std::process::Command::new("git")
            .args(["add", "tracked.txt"])
            .status()
            .unwrap();
        let commit_status = std::process::Command::new("git")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Test User",
                "commit",
                "-m",
                "initial",
            ])
            .status()
            .unwrap();
        assert!(commit_status.success());

        tokio::fs::write("tracked.txt", "base\napproved\n")
            .await
            .unwrap();
        tokio::fs::write("seeded.txt", "seeded canonical file\n")
            .await
            .unwrap();
        std::process::Command::new("git")
            .args(["add", "-A", "."])
            .status()
            .unwrap();
        let baseline = std::process::Command::new("git")
            .arg("write-tree")
            .output()
            .unwrap();
        assert!(baseline.status.success());
        let baseline = String::from_utf8_lossy(&baseline.stdout).trim().to_string();
        std::process::Command::new("git")
            .args(["read-tree", "HEAD"])
            .status()
            .unwrap();

        tokio::fs::write("tracked.txt", "base\napproved\ncurrent\n")
            .await
            .unwrap();
        tokio::fs::write("current.txt", "current task file\n")
            .await
            .unwrap();

        let patch = workspace_patch_against_baseline(&baseline).await.unwrap();

        assert!(patch.contains("+current"));
        assert!(patch.contains("current.txt"));
        assert!(!patch.contains("seeded.txt"));
        assert!(!patch.contains("+approved"));

        teardown(previous);
    }

    #[tokio::test]
    async fn workspace_patch_includes_untracked_greenfield_files_without_mutating_index() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        std::process::Command::new("git")
            .args(["init"])
            .status()
            .unwrap();
        let empty_index = dir.path().join(".git/empty-index");
        assert!(git_env(dir.path(), &empty_index, ["read-tree", "--empty"]).success());
        let baseline = git_env_output(dir.path(), &empty_index, ["write-tree"]);
        std::fs::remove_file(&empty_index).unwrap();

        tokio::fs::create_dir_all("src").await.unwrap();
        tokio::fs::create_dir_all("target/debug").await.unwrap();
        tokio::fs::write(".gitignore", ".ferrus/\n/target\n**/*.rs.bk\n")
            .await
            .unwrap();
        tokio::fs::write("Cargo.toml", "[package]\nname = \"demo\"\n")
            .await
            .unwrap();
        tokio::fs::write("src/main.rs", "fn main() {}\n")
            .await
            .unwrap();
        tokio::fs::write("target/debug/ignored", "ignored\n")
            .await
            .unwrap();

        let patch = workspace_patch_against_baseline(baseline.trim())
            .await
            .unwrap();

        assert!(patch.contains("diff --git a/.gitignore b/.gitignore"));
        assert!(patch.contains("+/target"));
        assert!(patch.contains("diff --git a/Cargo.toml b/Cargo.toml"));
        assert!(patch.contains("diff --git a/src/main.rs b/src/main.rs"));
        assert!(!patch.contains("target/debug/ignored"));
        assert_eq!(git_output(dir.path(), ["ls-files", "--stage"]), "");

        teardown(previous);
    }

    #[tokio::test]
    async fn submit_reclaims_expired_same_agent_lease_before_guarding() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status(
            "t-001",
            ".ferrus/tasks/t-001.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        crate::project::claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 0)
            .await
            .unwrap();

        run(
            Some("executor:codex:1"),
            "## Summary\nDone.\n\n## How to verify manually\nInspect it.\n".to_string(),
        )
        .await
        .unwrap();

        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
        assert_eq!(task.status, "reviewing");
        assert_eq!(task.claimed_by, None);
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-001/SUBMISSION.md")
                .await
                .unwrap(),
            "## Summary\nDone.\n\n## How to verify manually\nInspect it.\n"
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn submit_pass_clears_database_retry_metadata() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status(
            "t-001",
            ".ferrus/tasks/t-001.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        crate::project::record_task_check_failed("t-001", "fmt failed", 2)
            .await
            .unwrap();
        crate::project::claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 60)
            .await
            .unwrap();

        run(
            Some("executor:codex:1"),
            "## Summary\nDone.\n\n## How to verify manually\nInspect it.\n".to_string(),
        )
        .await
        .unwrap();

        crate::test_support::assert_no_state_json();
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
        assert_eq!(task.status, "reviewing");
        assert_eq!(task.check_retries, 0);
        assert_eq!(task.failure_reason, None);
        assert_eq!(task.claimed_by, None);

        teardown(previous);
    }

    #[tokio::test]
    async fn submit_writes_submission_to_agent_runtime_task_context() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();

        run(
            Some("executor:codex:7"),
            "## Summary\nDone.\n\n## How to verify manually\nInspect it.\n".to_string(),
        )
        .await
        .unwrap();

        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-007/SUBMISSION.md")
                .await
                .unwrap(),
            "## Summary\nDone.\n\n## How to verify manually\nInspect it.\n"
        );
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "reviewing");
        assert_eq!(task.check_retries, 0);
        assert_eq!(task.claimed_by, None);
        crate::test_support::assert_no_state_json();

        teardown(previous);
    }

    #[tokio::test]
    async fn submit_uses_database_context_when_state_json_is_absent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        crate::project::record_task_check_failed("t-007", "fmt failed", 2)
            .await
            .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();

        run(
            Some("executor:codex:7"),
            "## Summary\nDone.\n\n## How to verify manually\nInspect it.\n".to_string(),
        )
        .await
        .unwrap();

        crate::test_support::assert_no_state_json();
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-007/SUBMISSION.md")
                .await
                .unwrap(),
            "## Summary\nDone.\n\n## How to verify manually\nInspect it.\n"
        );
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "reviewing");
        assert_eq!(task.check_retries, 0);
        assert_eq!(task.failure_reason, None);
        assert_eq!(task.claimed_by, None);

        teardown(previous);
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

    fn git_env<const N: usize>(
        cwd: &std::path::Path,
        index: &std::path::Path,
        args: [&str; N],
    ) -> std::process::ExitStatus {
        std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .env("GIT_INDEX_FILE", index)
            .args(args)
            .status()
            .unwrap()
    }

    fn git_env_output<const N: usize>(
        cwd: &std::path::Path,
        index: &std::path::Path,
        args: [&str; N],
    ) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .env("GIT_INDEX_FILE", index)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}
