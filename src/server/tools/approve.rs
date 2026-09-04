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
use std::sync::{Mutex, OnceLock};
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

    // Observe both manifests while integration is serialized, then release the
    // approval lock before waiting for a potentially long graph refresh.
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
    let canonical_snapshot = apply_approved_patch(context, project_root).await?;
    if let Some(canonical_snapshot) = canonical_snapshot.as_ref() {
        // The approved patch may have changed ferrus.toml, so run the integration
        // gate against the configuration as it exists in the post-apply repository
        // state rather than the config loaded before the patch was applied.
        let post_apply_config = match Config::load_from(project_root).await {
            Ok(config) => config,
            Err(err) => {
                if let Err(rollback_err) = canonical_snapshot.restore(project_root).await {
                    anyhow::bail!(
                        "{err}\n\nAdditionally failed to restore the pre-integration canonical workspace: {rollback_err}"
                    );
                }
                return Err(err);
            }
        };
        let integration_checks =
            run_post_apply_integration_checks(context, &post_apply_config, project_root).await;
        if let Err(err) = integration_checks {
            if let Err(rollback_err) = canonical_snapshot.restore(project_root).await {
                anyhow::bail!(
                    "{err}\n\nAdditionally failed to restore the pre-integration canonical workspace: {rollback_err}"
                );
            }
            return Err(err);
        }
        if let Err(err) = canonical_snapshot.restore_index().await {
            if let Err(rollback_err) = canonical_snapshot.restore(project_root).await {
                anyhow::bail!(
                    "{err}\n\nAdditionally failed to restore the pre-integration canonical workspace: {rollback_err}"
                );
            }
            return Err(err).context("Failed to preserve the pre-integration canonical index");
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
        if let Some(canonical_snapshot) = canonical_snapshot.as_ref()
            && let Err(rollback_err) = canonical_snapshot.restore(project_root).await
        {
            anyhow::bail!(
                "{err}\n\nAdditionally failed to restore the pre-integration canonical workspace: {rollback_err}"
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
    let output = Command::new("git")
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
        let output = Command::new("git")
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

async fn apply_approved_patch(
    context: &RuntimeTaskContext,
    project_root: &Path,
) -> Result<Option<CanonicalWorkspaceSnapshot>> {
    let patch = store::read_patch_for_run_dir(&context.run_dir).await?;
    if patch.trim().is_empty() {
        return Ok(None);
    }

    let integration = async {
        let (baseline, submitted) = task_submission_trees(context, project_root).await?;
        let submitted_changes = tree_changes(project_root, &baseline, &submitted).await?;
        reject_sparse_checkout_exclusions(project_root, &submitted_changes).await?;
        let snapshot = CanonicalWorkspaceSnapshot::capture(project_root).await?;
        integrate_patch_three_way(project_root, &snapshot, &baseline, &submitted).await?;
        Ok::<_, anyhow::Error>(snapshot)
    }
    .await;
    let snapshot = match integration {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let detail = bounded_git_detail(&error.to_string());
            let reason = format!(
                "Cannot approve task {} because its submitted changes could not be merged into {}: {}",
                context.task_id,
                project_root.display(),
                detail
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
    };
    Ok(Some(snapshot))
}

async fn task_submission_trees(
    context: &RuntimeTaskContext,
    project_root: &Path,
) -> Result<(String, String)> {
    let frozen_submitted = context.repository_view.lifecycle
        == crate::repository_graph::domain::TaskViewLifecycle::FrozenSubmitted;
    let baseline = task_baseline_tree(project_root, &context.task_id, frozen_submitted).await?;
    let submitted = match context.repository_view.frozen_source_tree.as_ref() {
        Some(tree) if frozen_submitted => {
            verify_tree(project_root, tree.value()).await?;
            tree.value().to_string()
        }
        _ => submitted_tree_from_patch(context, project_root, &baseline).await?,
    };
    Ok((baseline, submitted))
}

async fn integrate_patch_three_way(
    project_root: &Path,
    snapshot: &CanonicalWorkspaceSnapshot,
    baseline: &str,
    submitted: &str,
) -> Result<()> {
    let merged = merge_trees(project_root, baseline, &snapshot.tree, submitted).await?;
    validate_tree_materialization(project_root, &snapshot.tree, &merged).await?;
    if let Err(error) = materialize_tree(project_root, &snapshot.tree, &merged).await {
        if let Err(restore_error) = snapshot.restore(project_root).await {
            anyhow::bail!(
                "{error}\n\nAdditionally failed to restore the pre-integration canonical workspace: {restore_error}"
            );
        }
        return Err(error);
    }
    Ok(())
}

async fn task_baseline_tree(
    project_root: &Path,
    task_id: &str,
    require_pinned: bool,
) -> Result<String> {
    let baseline_ref = format!("refs/ferrus/baselines/{task_id}");
    match resolve_tree(project_root, &baseline_ref).await {
        Ok(tree) => Ok(tree),
        Err(error) if require_pinned => {
            Err(error).context("Pinned task baseline tree is unavailable")
        }
        Err(_) => resolve_tree(project_root, "HEAD^{tree}")
            .await
            .context("Task baseline tree is unavailable"),
    }
}

async fn resolve_tree(project_root: &Path, revision: &str) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["rev-parse", "--verify"])
        .arg(revision)
        .output()
        .await?;
    git_stdout(output, "resolve Git tree")
}

async fn verify_tree(project_root: &Path, tree: &str) -> Result<()> {
    let object = format!("{tree}^{{tree}}");
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["cat-file", "-e"])
        .arg(object)
        .output()
        .await?;
    git_success(&output, "verify frozen submitted tree")
}

async fn submitted_tree_from_patch(
    context: &RuntimeTaskContext,
    project_root: &Path,
    baseline: &str,
) -> Result<String> {
    let index = TemporaryIntegrationIndex::new();
    read_tree(project_root, index.path(), baseline, false).await?;
    let patch_path = store::resolve_project_path(Path::new(&context.run_dir).join("PATCH.diff"));
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .env("GIT_INDEX_FILE", index.path())
        .args(["apply", "--cached", "--whitespace=nowarn"])
        .arg(patch_path)
        .output()
        .await?;
    git_success(&output, "apply submitted patch to its baseline")?;
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .env("GIT_INDEX_FILE", index.path())
        .arg("write-tree")
        .output()
        .await?;
    git_stdout(output, "write submitted tree")
}

async fn merge_trees(
    project_root: &Path,
    baseline: &str,
    ours: &str,
    theirs: &str,
) -> Result<String> {
    // Older Git versions require commit operands for merge-tree --write-tree.
    // Give both sides the explicit baseline as their common parent.
    let baseline_commit = commit_tree(project_root, baseline, None, "baseline").await?;
    let ours_commit = commit_tree(project_root, ours, Some(&baseline_commit), "canonical").await?;
    let theirs_commit =
        commit_tree(project_root, theirs, Some(&baseline_commit), "submitted").await?;
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["merge-tree", "--write-tree", "--messages"])
        .arg(ours_commit)
        .arg(theirs_commit)
        .output()
        .await?;
    git_stdout(output, "three-way merge approved task")
}

#[derive(Debug)]
struct TreeChange {
    path: Vec<u8>,
    is_addition: bool,
    changes_gitlink: bool,
}

async fn validate_tree_materialization(
    project_root: &Path,
    current_tree: &str,
    target_tree: &str,
) -> Result<()> {
    let changes = tree_changes(project_root, current_tree, target_tree).await?;
    if changes.iter().any(|change| change.changes_gitlink) {
        anyhow::bail!(
            "Approved submissions that change submodules are not supported because their gitlinks cannot be materialized safely"
        );
    }
    reject_ignored_path_collisions(project_root, &changes).await?;
    reject_sparse_checkout_exclusions(project_root, &changes).await
}

async fn tree_changes(
    project_root: &Path,
    current_tree: &str,
    target_tree: &str,
) -> Result<Vec<TreeChange>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args([
            "diff-tree",
            "--no-commit-id",
            "--raw",
            "-r",
            "-z",
            "--no-renames",
        ])
        .arg(current_tree)
        .arg(target_tree)
        .output()
        .await?;
    git_success(&output, "inspect paths changed by the approved tree")?;
    parse_raw_tree_changes(&output.stdout)
}

fn parse_raw_tree_changes(raw: &[u8]) -> Result<Vec<TreeChange>> {
    let mut records = raw.split(|byte| *byte == 0);
    let mut changes = Vec::new();
    while let Some(metadata) = records.next() {
        if metadata.is_empty() {
            continue;
        }
        let path = records
            .next()
            .context("Git returned a malformed raw tree diff without a path")?;
        let mut fields = metadata.split(|byte| byte.is_ascii_whitespace());
        let old_mode = fields
            .next()
            .and_then(|mode| mode.strip_prefix(b":"))
            .context("Git returned a malformed raw tree diff without an old mode")?;
        let new_mode = fields
            .next()
            .context("Git returned a malformed raw tree diff without a new mode")?;
        changes.push(TreeChange {
            path: path.to_vec(),
            is_addition: old_mode == b"000000" && new_mode != b"000000",
            changes_gitlink: old_mode == b"160000" || new_mode == b"160000",
        });
    }
    Ok(changes)
}

async fn reject_ignored_path_collisions(project_root: &Path, changes: &[TreeChange]) -> Result<()> {
    let additions: Vec<&[u8]> = changes
        .iter()
        .filter(|change| change.is_addition)
        .map(|change| change.path.as_slice())
        .collect();
    if additions.is_empty() {
        return Ok(());
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args([
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
        ])
        .output()
        .await?;
    git_success(
        &output,
        "inspect ignored canonical paths before integration",
    )?;
    let collision = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .any(|ignored| {
            let ignored = ignored.strip_suffix(b"/").unwrap_or(ignored);
            additions
                .iter()
                .any(|addition| paths_overlap(ignored, addition))
        });
    if collision {
        anyhow::bail!(
            "Canonical workspace contains ignored local content at a path changed by the approved submission"
        );
    }
    Ok(())
}

fn paths_overlap(left: &[u8], right: &[u8]) -> bool {
    left == right || path_is_parent(left, right) || path_is_parent(right, left)
}

fn path_is_parent(parent: &[u8], child: &[u8]) -> bool {
    child
        .strip_prefix(parent)
        .is_some_and(|remainder| remainder.starts_with(b"/"))
}

async fn reject_sparse_checkout_exclusions(
    project_root: &Path,
    changes: &[TreeChange],
) -> Result<()> {
    if changes.is_empty() || !sparse_checkout_enabled(project_root).await? {
        return Ok(());
    }
    let paths: HashSet<&[u8]> = changes
        .iter()
        .map(|change| change.path.as_slice())
        .collect();
    let input = nul_terminated_paths(paths.iter().copied());
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(project_root)
        .args(["sparse-checkout", "check-rules", "-z"]);
    let output = command_output_with_stdin(&mut command, &input).await?;
    git_success(&output, "check approved paths against the sparse checkout")?;
    let included: HashSet<&[u8]> = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .collect();
    if paths.iter().any(|path| !included.contains(path)) {
        anyhow::bail!("Approved submission changes paths outside the canonical sparse checkout");
    }
    Ok(())
}

async fn sparse_checkout_enabled(project_root: &Path) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["config", "--bool", "--get", "core.sparseCheckout"])
        .output()
        .await?;
    if output.status.success() {
        return Ok(output.stdout.starts_with(b"true"));
    }
    if output.status.code() == Some(1) {
        return Ok(false);
    }
    git_success(&output, "inspect canonical sparse checkout configuration")?;
    Ok(false)
}

fn nul_terminated_paths<'a>(paths: impl IntoIterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path);
        input.push(0);
    }
    input
}

async fn command_output_with_stdin(command: &mut Command, input: &[u8]) -> Result<Output> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .context("Failed to open Git stdin for canonical integration preflight")?;
    stdin.write_all(input).await?;
    drop(stdin);
    Ok(child.wait_with_output().await?)
}

async fn commit_tree(
    project_root: &Path,
    tree: &str,
    parent: Option<&str>,
    role: &str,
) -> Result<String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(project_root)
        .env("GIT_AUTHOR_NAME", "Ferrus")
        .env("GIT_AUTHOR_EMAIL", "ferrus@example.invalid")
        .env("GIT_COMMITTER_NAME", "Ferrus")
        .env("GIT_COMMITTER_EMAIL", "ferrus@example.invalid")
        .args(["commit-tree", tree]);
    if let Some(parent) = parent {
        command.args(["-p", parent]);
    }
    let output = command
        .args(["-m", &format!("Ferrus temporary {role} tree")])
        .output()
        .await?;
    git_stdout(output, &format!("wrap {role} tree in a temporary commit"))
}

async fn materialize_tree(
    project_root: &Path,
    current_tree: &str,
    target_tree: &str,
) -> Result<()> {
    let index = TemporaryIntegrationIndex::new();
    read_tree(project_root, index.path(), current_tree, false).await?;
    read_tree(project_root, index.path(), target_tree, true).await
}

async fn tree_without_paths(
    project_root: &Path,
    tree: &str,
    excluded_paths: &[Vec<u8>],
) -> Result<String> {
    if excluded_paths.is_empty() {
        return Ok(tree.to_string());
    }

    let index = TemporaryIntegrationIndex::new();
    read_tree(project_root, index.path(), tree, false).await?;
    let input = nul_terminated_paths(excluded_paths.iter().map(Vec::as_slice));
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(project_root)
        .env("GIT_INDEX_FILE", index.path())
        .args(["update-index", "--force-remove", "-z", "--stdin"]);
    let output = command_output_with_stdin(&mut command, &input).await?;
    git_success(
        &output,
        "exclude pre-integration ignored paths from rollback",
    )?;
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .env("GIT_INDEX_FILE", index.path())
        .arg("write-tree")
        .output()
        .await?;
    git_stdout(output, "write rollback tree without ignored paths")
}

static TEMPORARY_INTEGRATION_INDEX_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TemporaryIntegrationIndex {
    path: PathBuf,
}

impl TemporaryIntegrationIndex {
    fn new() -> Self {
        let sequence = TEMPORARY_INTEGRATION_INDEX_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            path: std::env::temp_dir().join(format!(
                "ferrus-integration-index-{}-{nanos:x}-{sequence:x}",
                std::process::id()
            )),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryIntegrationIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let mut lock = self.path.as_os_str().to_owned();
        lock.push(".lock");
        let _ = std::fs::remove_file(PathBuf::from(lock));
    }
}

async fn read_tree(
    project_root: &Path,
    index: &Path,
    tree: &str,
    update_worktree: bool,
) -> Result<()> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(project_root)
        .env("GIT_INDEX_FILE", index)
        .arg("read-tree");
    if update_worktree {
        command.args(["--reset", "-u"]);
    }
    let output = command.arg(tree).output().await?;
    git_success(&output, "materialize Git tree through temporary index")
}

fn git_stdout(output: Output, operation: &str) -> Result<String> {
    git_success(&output, operation)?;
    let stdout =
        String::from_utf8(output.stdout).context("Git returned non-UTF-8 object output")?;
    stdout
        .lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .with_context(|| format!("Git returned no result while attempting to {operation}"))
}

fn git_success(output: &Output, operation: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    anyhow::bail!(
        "Failed to {operation}: {}",
        if detail.is_empty() {
            output.status.to_string()
        } else {
            bounded_git_detail(detail)
        }
    )
}

fn bounded_git_detail(detail: &str) -> String {
    const MAX_BYTES: usize = 8 * 1024;
    if detail.len() <= MAX_BYTES {
        return detail.to_string();
    }
    let mut end = MAX_BYTES;
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n... output truncated ...", &detail[..end])
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

struct CanonicalApprovalLock {
    path: PathBuf,
    owner_path: PathBuf,
    owner_file: Option<std::fs::File>,
}

struct CanonicalApprovalLockGuard {
    _file: std::fs::File,
}

impl Drop for CanonicalApprovalLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        unregister_canonical_approval_owner(&self.owner_path);
        drop(self.owner_file.take());
        let _ = std::fs::remove_file(&self.owner_path);
    }
}

static CANONICAL_APPROVAL_OWNERS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

fn canonical_approval_owners() -> &'static Mutex<HashSet<PathBuf>> {
    CANONICAL_APPROVAL_OWNERS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn register_canonical_approval_owner(path: &Path) {
    canonical_approval_owners()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(path.to_path_buf());
}

fn unregister_canonical_approval_owner(path: &Path) {
    canonical_approval_owners()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(path);
}

fn process_holds_canonical_approval_owner(path: &Path) -> bool {
    canonical_approval_owners()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains(path)
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

    // Serialize creation and stale-lock removal on a stable guard file so
    // competing recoveries cannot unlink a newly acquired approval lock.
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

fn canonical_approval_lock_owner_path(lock_path: &Path) -> PathBuf {
    lock_path.with_file_name(".canonical-approval.lock.owner")
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

    // Publish a complete marker without replacing an existing owner's path.
    match std::fs::hard_link(&temp_path, lock_path) {
        Ok(()) => {
            let owner_path = canonical_approval_lock_owner_path(lock_path);
            let owner_file = match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&owner_path)
                .and_then(|file| {
                    file.lock_exclusive()?;
                    Ok(file)
                }) {
                Ok(file) => file,
                Err(err) => {
                    let _ = tokio::fs::remove_file(&temp_path).await;
                    let _ = tokio::fs::remove_file(lock_path).await;
                    let _ = tokio::fs::remove_file(&owner_path).await;
                    return Err(err).with_context(|| {
                        format!(
                            "Failed to hold canonical approval owner marker {}",
                            owner_path.display()
                        )
                    });
                }
            };
            register_canonical_approval_owner(&owner_path);
            let _ = tokio::fs::remove_file(&temp_path).await;
            Ok(Some(CanonicalApprovalLock {
                path: lock_path.to_path_buf(),
                owner_path,
                owner_file: Some(owner_file),
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

// The held owner-file lock is authoritative; marker PIDs can be reused after a crash.
async fn remove_stale_canonical_approval_lock(lock_path: &Path) -> Result<bool> {
    let owner_path = canonical_approval_lock_owner_path(lock_path);
    if process_holds_canonical_approval_owner(&owner_path) {
        return Ok(false);
    }
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&owner_path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return remove_canonical_approval_lock_files(lock_path, &owner_path).await;
        }
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "Failed to open canonical approval owner marker {}",
                    owner_path.display()
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
                    "Failed to inspect canonical approval owner marker {}",
                    owner_path.display()
                )
            });
        }
    }
    drop(file);

    remove_canonical_approval_lock_files(lock_path, &owner_path).await
}

async fn remove_canonical_approval_lock_files(lock_path: &Path, owner_path: &Path) -> Result<bool> {
    match tokio::fs::remove_file(lock_path).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => Err(err).with_context(|| {
            format!(
                "Failed to remove stale canonical approval lock {}",
                lock_path.display()
            )
        })?,
    }
    match tokio::fs::remove_file(owner_path).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => Err(err).with_context(|| {
            format!(
                "Failed to remove stale canonical approval owner marker {}",
                owner_path.display()
            )
        })?,
    }
    Ok(true)
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
