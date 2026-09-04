//! Run the final check gate and persist submission artifacts with the Reviewing handoff.
//! A successful graph freeze pins the submitted source tree for stable review and patch generation.

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
    repository_graph::source::{
        GitWorktreeInventory, parse_git_tree_digest, release_submitted_tree_pin,
    },
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
On pass: state -> Reviewing. On fail: stay in the current work state (or state \
-> Failed if the retry limit is exhausted).

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
        persist_submission(&context, &content, frozen_view).await?;
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
            persist_submission(&context, &content, frozen_view).await?;
            project::record_runtime_event_best_effort(
                context.run_id.clone(),
                "submitted",
                serde_json::json!({ "content_bytes": content.len(), "check_gate": "passed" }),
            )
            .await;

            info!("Work submitted for review, state -> Reviewing");
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

async fn write_submission_patch(
    context: &RuntimeTaskContext,
    freeze: &crate::repository_graph_runtime::RepositoryViewFreeze,
) -> Result<()> {
    if !is_isolated_executor_workspace(context).await {
        store::clear_patch_for_run_dir(&context.run_dir).await?;
        return Ok(());
    }

    let patch = match freeze {
        crate::repository_graph_runtime::RepositoryViewFreeze::Frozen(view) => {
            let source_tree = view.frozen_source_tree.as_ref().ok_or_else(|| {
                anyhow::anyhow!("frozen repository view is missing its source tree")
            })?;
            frozen_tree_patch(context, source_tree).await?
        }
        _ => workspace_patch(context).await?,
    };
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

async fn frozen_tree_patch(
    context: &RuntimeTaskContext,
    source_tree: &crate::repository_graph::domain::Digest,
) -> Result<String> {
    let baseline = baseline_tree(context).unwrap_or_else(|| "HEAD".to_string());
    let baseline = parse_git_tree_digest(&baseline)?;
    let workspace_root = context
        .workspace_path
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    tree_patch_between(&workspace_root, &baseline, source_tree).await
}

async fn tree_patch_between(
    workspace_root: &Path,
    baseline: &crate::repository_graph::domain::Digest,
    source_tree: &crate::repository_graph::domain::Digest,
) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["diff", "--binary"])
        .arg(baseline.value())
        .arg(source_tree.value())
        .arg("--")
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "Failed to capture frozen executor patch: {}",
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        );
    }
    submitted_patch_from_utf8(output.stdout)
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
        patch.push_str(&submitted_patch_from_utf8(output.stdout)?);
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
    let patch = submitted_patch_from_utf8(output.stdout)?;
    Ok(rewrite_no_index_patch_paths(
        &patch, path, old_null, new_null,
    ))
}

fn submitted_patch_from_utf8(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes).map_err(|error| {
        let offset = error.utf8_error().valid_up_to();
        anyhow::anyhow!(
            "Cannot store submitted patch because Git emitted non-UTF-8 bytes at byte offset {offset}"
        )
    })
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

async fn persist_submission(
    context: &RuntimeTaskContext,
    content: &str,
    freeze: crate::repository_graph_runtime::RepositoryViewFreeze,
) -> Result<()> {
    // Keep the tree reachable only if artifacts and the Reviewing handoff succeed.
    // The guard also releases the pin if this future is dropped before the handoff.
    let mut pin_cleanup = SubmittedTreePinCleanup::new(context, &freeze);
    project::record_task_check_passed(&context.task_id).await?;
    write_submission(context, content).await?;
    write_submission_patch(context, &freeze).await?;
    record_submission(context, freeze).await?;
    pin_cleanup.disarm();
    Ok(())
}

struct SubmittedTreePinCleanup {
    workspace_root: Option<PathBuf>,
    task_id: String,
    armed: bool,
}

impl SubmittedTreePinCleanup {
    fn new(
        context: &RuntimeTaskContext,
        freeze: &crate::repository_graph_runtime::RepositoryViewFreeze,
    ) -> Self {
        Self {
            workspace_root: context.workspace_path.as_deref().map(PathBuf::from),
            task_id: context.task_id.clone(),
            armed: matches!(
                freeze,
                crate::repository_graph_runtime::RepositoryViewFreeze::Frozen(_)
            ),
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SubmittedTreePinCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(workspace_root) = self.workspace_root.as_deref() else {
            tracing::warn!(
                task_id = self.task_id,
                "cannot release abandoned submitted tree pin without a workspace"
            );
            return;
        };
        if let Err(error) = release_submitted_tree_pin(workspace_root, &self.task_id) {
            tracing::warn!(
                task_id = self.task_id,
                error = ?error,
                "failed to release abandoned submitted tree pin"
            );
        }
    }
}

#[cfg(test)]
#[path = "submit_tests.rs"]
mod tests;
