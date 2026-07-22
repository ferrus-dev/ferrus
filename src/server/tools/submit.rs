use anyhow::Result;
use neva::prelude::*;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use tokio::process::Command;
use tracing::info;

use crate::{
    agent_id::{ENV_BASELINE_TREE, ENV_PROJECT_ROOT},
    config::Config,
    project::{self, RuntimeTaskContext, TaskCheckFailure},
    repository_graph::source::{GitWorktreeInventory, parse_git_tree_digest},
    state::store,
};

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

use super::{
    check_gate::{self, CheckGateResult},
    ensure_lease_owner_or_reclaim, require_runtime_task_context, tool_err,
};

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

pub async fn handler(
    ctx: neva::di::Dc<crate::server::ServerContext>,
    content: String,
) -> Result<String, Error> {
    handler_for_agent(ctx.agent_id(), content).await
}

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
        let frozen_view =
            crate::repository_graph_runtime::prepare_submitted_repository_view(&context).await;
        project::record_task_check_passed(&context.task_id).await?;
        write_submission(&context, &content).await?;
        write_submission_patch(&context).await?;
        record_submission(&context, frozen_view).await?;
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
    let log_scope = context
        .run_id
        .as_deref()
        .unwrap_or(context.task_id.as_str());
    let gate = check_gate::run(&config, attempt, log_scope).await?;
    match gate {
        CheckGateResult::Passed => {
            let frozen_view =
                crate::repository_graph_runtime::prepare_submitted_repository_view(&context).await;
            project::record_task_check_passed(&context.task_id).await?;
            write_submission(&context, &content).await?;
            write_submission_patch(&context).await?;
            record_submission(&context, frozen_view).await?;
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
    store::clear_integration_error_for_run_dir(&context.run_dir).await?;
    store::write_submission_for_run_dir(&context.run_dir, content).await
}

async fn write_submission_patch(context: &RuntimeTaskContext) -> Result<()> {
    if !is_isolated_executor_workspace(context).await {
        store::clear_patch_for_run_dir(&context.run_dir).await?;
        return Ok(());
    }

    let patch = workspace_patch(context).await?;
    store::write_patch_for_run_dir(&context.run_dir, &patch).await
}

async fn is_isolated_executor_workspace(context: &RuntimeTaskContext) -> bool {
    let Some(project_root) = project_root_for_isolation().await else {
        return false;
    };
    is_workspace_isolated_from_project_root(context, &project_root).await
}

async fn project_root_for_isolation() -> Option<PathBuf> {
    if let Some(project_root) = project_root_from_env() {
        return Some(project_root);
    }
    project_root_from_local_ref().await
}

fn project_root_from_env() -> Option<PathBuf> {
    std::env::var(ENV_PROJECT_ROOT)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

async fn project_root_from_local_ref() -> Option<PathBuf> {
    let local_ref = tokio::fs::read_to_string(".ferrus/project.toml")
        .await
        .ok()?;
    let local_ref = toml::from_str::<project::LocalProjectRef>(&local_ref).ok()?;
    let metadata_path = Path::new(&local_ref.data_dir).join("project.toml");
    let metadata = tokio::fs::read_to_string(metadata_path).await.ok()?;
    let metadata = toml::from_str::<project::ProjectMetadata>(&metadata).ok()?;
    Some(PathBuf::from(metadata.workspace_dir))
}

async fn is_workspace_isolated_from_project_root(
    context: &RuntimeTaskContext,
    project_root: &Path,
) -> bool {
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
    !equivalent_paths(&workspace_path, project_root).await
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

async fn workspace_patch(context: &RuntimeTaskContext) -> Result<String> {
    let baseline = baseline_tree(context).unwrap_or_else(|| "HEAD".to_string());
    workspace_patch_against_baseline(&baseline).await
}

fn baseline_tree(context: &RuntimeTaskContext) -> Option<String> {
    baseline_tree_from_env().or_else(|| baseline_tree_from_project_data(&context.task_id))
}

fn baseline_tree_from_env() -> Option<String> {
    std::env::var(ENV_BASELINE_TREE)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn baseline_tree_from_project_data(task_id: &str) -> Option<String> {
    let local_ref = std::fs::read_to_string(".ferrus/project.toml").ok()?;
    let local_ref = toml::from_str::<project::LocalProjectRef>(&local_ref).ok()?;
    let baseline_path = Path::new(&local_ref.data_dir)
        .join("worktrees")
        .join(".baseline-trees")
        .join(format!("{task_id}.txt"));
    std::fs::read_to_string(baseline_path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

async fn workspace_patch_against_baseline(baseline: &str) -> Result<String> {
    let root = std::env::current_dir()?;
    let baseline_digest = parse_git_tree_digest(baseline)?;
    let inventory =
        tokio::task::spawn_blocking(move || GitWorktreeInventory::discover(root, baseline_digest))
            .await??;
    let tracked = inventory
        .tracked_paths()
        .iter()
        .map(|path| path.as_str().to_string())
        .collect::<Vec<_>>();
    let tracked_set = tracked.iter().cloned().collect::<HashSet<_>>();
    let baseline_files = inventory
        .baseline_paths()
        .iter()
        .map(|path| path.as_str().to_string())
        .collect::<Vec<_>>();
    let baseline_set = baseline_files.iter().cloned().collect::<HashSet<_>>();
    let mut patch = tracked_workspace_patch(baseline, &tracked).await?;

    for path in baseline_files {
        if tracked_set.contains(&path) {
            continue;
        }
        patch.push_str(&baseline_untracked_path_patch(baseline, &path).await?);
    }

    for path in inventory
        .untracked_paths()
        .iter()
        .map(|path| path.as_str().to_string())
    {
        if baseline_set.contains(&path) {
            continue;
        }
        patch.push_str(&new_file_patch(&path).await?);
    }
    Ok(patch)
}

async fn tracked_workspace_patch(baseline: &str, tracked: &[String]) -> Result<String> {
    if tracked.is_empty() {
        return Ok(String::new());
    }
    // Restricting the diff to tracked pathspecs keeps baseline-only paths (handled separately
    // as untracked) out of this patch. Spread the pathspecs across batched invocations rather
    // than expanding every tracked path into a single argv, which can exceed the OS arg limit
    // in repos with many files (`git diff` does not support `--pathspec-from-file`).
    let mut patch = String::new();
    for batch in tracked_pathspec_batches(tracked) {
        let mut command = Command::new("git");
        command.args(["diff", "--binary"]).arg(baseline).arg("--");
        for path in batch {
            command.arg(path);
        }
        let output = command.output().await?;
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
        patch.push_str(&String::from_utf8_lossy(&output.stdout));
    }
    Ok(patch)
}

/// Conservative per-invocation cap on the cumulative byte length of pathspec arguments.
/// Sized for the tightest platform rather than per-OS: Windows caps the whole command line
/// at 32767 characters, far below the Unix `ARG_MAX` (1 MiB on macOS, ~2 MiB on Linux). This
/// leaves headroom under that limit for the `git diff` prefix and per-argument quoting.
const PATHSPEC_ARGV_BUDGET: usize = 24_000;

fn tracked_pathspec_batches(tracked: &[String]) -> Vec<&[String]> {
    let mut batches = Vec::new();
    let mut start = 0;
    let mut used = 0;
    for (index, path) in tracked.iter().enumerate() {
        // +1 accounts for the NUL terminator each argv entry carries.
        let cost = path.len() + 1;
        if index > start && used + cost > PATHSPEC_ARGV_BUDGET {
            batches.push(&tracked[start..index]);
            start = index;
            used = 0;
        }
        used += cost;
    }
    batches.push(&tracked[start..]);
    batches
}

async fn baseline_untracked_path_patch(baseline: &str, path: &str) -> Result<String> {
    let old = temporary_file_path("baseline-blob");
    let content = git_show_path(baseline, path).await?;
    tokio::fs::write(&old, content).await?;
    let result = if tokio::fs::try_exists(path).await? {
        no_index_patch(&old, Path::new(path), path, false, false).await
    } else {
        no_index_patch(&old, Path::new("/dev/null"), path, false, true).await
    };
    let _ = tokio::fs::remove_file(&old).await;
    result
}

async fn git_show_path(baseline: &str, path: &str) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("show")
        .arg(format!("{baseline}:{path}"))
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
    Ok(output.stdout)
}

async fn new_file_patch(path: &str) -> Result<String> {
    no_index_patch(Path::new("/dev/null"), Path::new(path), path, true, false).await
}

async fn no_index_patch(
    old: &Path,
    new: &Path,
    path: &str,
    old_null: bool,
    new_null: bool,
) -> Result<String> {
    let output = Command::new("git")
        .args(["diff", "--no-index", "--binary", "--"])
        .arg(old)
        .arg(new)
        .output()
        .await?;
    if !(output.status.success() || output.status.code() == Some(1)) {
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
    Ok(rewrite_no_index_patch_paths(
        &String::from_utf8_lossy(&output.stdout),
        path,
        old_null,
        new_null,
    ))
}

fn rewrite_no_index_patch_paths(patch: &str, path: &str, old_null: bool, new_null: bool) -> String {
    let mut rewritten = String::new();
    for line in patch.split_inclusive('\n') {
        let line_without_newline = line.trim_end_matches('\n');
        let newline = if line.ends_with('\n') { "\n" } else { "" };
        if line_without_newline.starts_with("diff --git ") {
            rewritten.push_str(&format!("diff --git a/{path} b/{path}{newline}"));
        } else if line_without_newline.starts_with("--- ") {
            let old_path = if old_null {
                "/dev/null".to_string()
            } else {
                format!("a/{path}")
            };
            rewritten.push_str(&format!("--- {old_path}{newline}"));
        } else if line_without_newline.starts_with("+++ ") {
            let new_path = if new_null {
                "/dev/null".to_string()
            } else {
                format!("b/{path}")
            };
            rewritten.push_str(&format!("+++ {new_path}{newline}"));
        } else {
            rewritten.push_str(line);
        }
    }
    rewritten
}

fn temporary_file_path(prefix: &str) -> PathBuf {
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{prefix}-{}-{counter}", std::process::id()))
}

async fn record_submission(
    context: &RuntimeTaskContext,
    freeze: crate::repository_graph_runtime::RepositoryViewFreeze,
) -> Result<()> {
    let (frozen_view, failed) = match &freeze {
        crate::repository_graph_runtime::RepositoryViewFreeze::NotAttempted => (None, false),
        crate::repository_graph_runtime::RepositoryViewFreeze::Frozen(view) => (Some(view), false),
        crate::repository_graph_runtime::RepositoryViewFreeze::Failed => (None, true),
    };
    project::record_task_submitted(
        &context.task_id,
        &context.task_path,
        context.run_id.as_deref(),
        frozen_view,
        failed,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn tracked_pathspec_batches_keeps_short_lists_in_one_batch() {
        let tracked = vec!["a.txt".to_string(), "b.txt".to_string()];
        let batches = tracked_pathspec_batches(&tracked);
        assert_eq!(batches, vec![tracked.as_slice()]);
    }

    #[test]
    fn tracked_pathspec_batches_splits_when_argv_budget_exceeded() {
        let path = "x".repeat(1024);
        let count = (PATHSPEC_ARGV_BUDGET / (path.len() + 1)) + 5;
        let tracked = vec![path; count];
        let batches = tracked_pathspec_batches(&tracked);
        assert!(batches.len() > 1, "expected multiple batches");
        assert_eq!(
            batches.iter().map(|batch| batch.len()).sum::<usize>(),
            count,
            "every path must appear exactly once across batches"
        );
        for batch in &batches {
            assert!(!batch.is_empty(), "batches must be non-empty");
        }
    }

    #[test]
    fn tracked_pathspec_batches_keeps_oversized_single_path() {
        let tracked = vec!["y".repeat(PATHSPEC_ARGV_BUDGET * 2)];
        let batches = tracked_pathspec_batches(&tracked);
        assert_eq!(batches, vec![tracked.as_slice()]);
    }

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
    async fn workspace_patch_uses_stored_baseline_when_env_is_absent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        let _env_guard = EnvVarGuard::remove(ENV_BASELINE_TREE);
        std::env::set_current_dir(dir.path()).unwrap();

        std::process::Command::new("git")
            .args(["init"])
            .status()
            .unwrap();
        let data_dir = dir.path().join("runtime");
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        tokio::fs::create_dir_all(".ferrus").await.unwrap();
        let local_ref = project::LocalProjectRef {
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
        let empty_index = dir.path().join(".git/empty-index");
        assert!(git_env(dir.path(), &empty_index, ["read-tree", "--empty"]).success());
        let baseline = git_env_output(dir.path(), &empty_index, ["write-tree"]);
        std::fs::remove_file(&empty_index).unwrap();
        let baseline_path = data_dir.join("worktrees/.baseline-trees/t-test.txt");
        tokio::fs::create_dir_all(baseline_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&baseline_path, &baseline).await.unwrap();

        tokio::fs::write("Cargo.toml", "[package]\nname = \"demo\"\n")
            .await
            .unwrap();

        let context = runtime_context_with_workspace(dir.path());
        let patch = workspace_patch(&context).await.unwrap();

        assert!(patch.contains("diff --git a/Cargo.toml b/Cargo.toml"));
        assert!(patch.contains("+name = \"demo\""));
        assert_eq!(git_output(dir.path(), ["ls-files", "--stage"]), "");

        teardown(previous);
    }

    #[tokio::test]
    async fn workspace_patch_includes_changes_to_seeded_untracked_files() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        std::process::Command::new("git")
            .args(["init"])
            .status()
            .unwrap();
        tokio::fs::write(".gitignore", ".ferrus/\n").await.unwrap();
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
            .args(["read-tree", "--empty"])
            .status()
            .unwrap();
        tokio::fs::write(".gitignore", ".ferrus/\n/target\n**/*.rs.bk\n")
            .await
            .unwrap();

        let patch = workspace_patch_against_baseline(&baseline).await.unwrap();

        assert!(patch.contains("diff --git a/.gitignore b/.gitignore"));
        assert!(patch.contains("+/target"));
        assert!(patch.contains("+**/*.rs.bk"));
        assert_eq!(git_output(dir.path(), ["ls-files", "--stage"]), "");

        teardown(previous);
    }

    #[tokio::test]
    async fn isolated_workspace_detection_falls_back_to_project_metadata() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        let canonical = dir.path().join("canonical");
        let worktree = dir.path().join("worktree");
        let data_dir = dir.path().join("runtime");
        tokio::fs::create_dir_all(canonical.join(".ferrus"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(worktree.join(".ferrus"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        let local_ref = project::LocalProjectRef {
            project_id: "test-project".to_string(),
            name: "test".to_string(),
            data_dir: data_dir.to_string_lossy().into_owned(),
        };
        tokio::fs::write(
            worktree.join(".ferrus/project.toml"),
            toml::to_string_pretty(&local_ref).unwrap(),
        )
        .await
        .unwrap();
        let metadata = project::ProjectMetadata {
            id: "test-project".to_string(),
            name: "test".to_string(),
            workspace_dir: canonical.to_string_lossy().into_owned(),
            ferrus_dir: canonical.join(".ferrus").to_string_lossy().into_owned(),
            vcs: Some("git".to_string()),
            origin_repo: None,
            default_branch: None,
            current_head: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            last_opened_at: "2026-01-01T00:00:00Z".to_string(),
            version: 1,
        };
        tokio::fs::write(
            data_dir.join("project.toml"),
            toml::to_string_pretty(&metadata).unwrap(),
        )
        .await
        .unwrap();
        std::env::set_current_dir(&worktree).unwrap();

        let context = runtime_context_with_workspace(&worktree);

        assert!(is_isolated_executor_workspace(&context).await);

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
    async fn submit_clears_stale_integration_error_on_success() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::Addressing,
        )
        .await
        .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();
        store::write_integration_error_for_run_dir(
            ".ferrus/runs/t-007",
            "# Integration Error\n\nold conflict\n",
        )
        .await
        .unwrap();

        run(
            Some("executor:codex:7"),
            "## Summary\nFixed.\n\n## How to verify manually\nInspect it.\n".to_string(),
        )
        .await
        .unwrap();

        assert_eq!(
            store::read_integration_error_for_run_dir(".ferrus/runs/t-007")
                .await
                .unwrap(),
            ""
        );
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "reviewing");

        teardown(previous);
    }

    #[tokio::test]
    async fn canonical_submit_clears_stale_isolated_patch() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::Addressing,
        )
        .await
        .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();
        store::write_patch_for_run_dir(
            ".ferrus/runs/t-007",
            "diff --git a/old.txt b/old.txt\n+stale\n",
        )
        .await
        .unwrap();

        run(
            Some("executor:codex:7"),
            "## Summary\nFixed in canonical workspace.\n\n## How to verify manually\nInspect it.\n"
                .to_string(),
        )
        .await
        .unwrap();

        assert_eq!(
            store::read_patch_for_run_dir(".ferrus/runs/t-007")
                .await
                .unwrap(),
            ""
        );
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "reviewing");

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

    fn runtime_context_with_workspace(workspace: &std::path::Path) -> RuntimeTaskContext {
        RuntimeTaskContext {
            task_id: "t-test".to_string(),
            task_path: ".ferrus/tasks/t-test.md".to_string(),
            spec_path: None,
            milestone_id: None,
            run_dir: ".ferrus/runs/t-test".to_string(),
            status: project::TaskStatus::Executing.as_str().to_string(),
            paused_status: None,
            check_retries: 0,
            review_cycles: 0,
            failure_reason: None,
            run_id: None,
            run_role: Some("executor".to_string()),
            workspace_path: Some(workspace.to_string_lossy().into_owned()),
            repository_workspace_path: Some(workspace.to_string_lossy().into_owned()),
            repository_view: project::RepositoryViewReference::default(),
        }
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn remove(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match self.previous.take() {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }
}
