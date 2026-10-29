//! Git tree integration, materialization, and rollback helpers.

use super::*;

pub(super) async fn apply_approved_patch(
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
            if let Err(report_error) = write_integration_error(context, &reason).await {
                return Err(error).context(format!(
                    "{reason}\n\nFailed to save integration diagnostics: {report_error}"
                ));
            }

            project::record_task_integration_failed_best_effort(
                &context.task_id,
                context.run_id.as_deref(),
                &reason,
            )
            .await;
            return Err(error).context(format!(
                "{reason}\n\nIntegration error was saved to {}/INTEGRATION_ERROR.md. \
             Reject this review with the conflict details so an Executor can address it.",
                context.run_dir
            ));
        }
    };
    Ok(Some(snapshot))
}

pub(super) async fn task_submission_trees(
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

pub(super) async fn integrate_patch_three_way(
    project_root: &Path,
    snapshot: &CanonicalWorkspaceSnapshot,
    baseline: &str,
    submitted: &str,
) -> Result<()> {
    let merged = merge_trees(project_root, baseline, &snapshot.tree, submitted).await?;
    validate_tree_materialization(project_root, &snapshot.tree, &merged).await?;
    if let Err(error) = materialize_tree(project_root, &snapshot.tree, &merged).await {
        if let Err(restore_error) = snapshot.restore(project_root).await {
            return Err(rollback_failed(error, restore_error));
        }

        return Err(error);
    }

    Ok(())
}

pub(super) async fn task_baseline_tree(
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

pub(super) async fn resolve_tree(project_root: &Path, revision: &str) -> Result<String> {
    let output = approval_git_command()
        .arg("-C")
        .arg(project_root)
        .args(["rev-parse", "--verify"])
        .arg(revision)
        .output()
        .await?;
    git_stdout(output, "resolve Git tree")
}

pub(super) async fn verify_tree(project_root: &Path, tree: &str) -> Result<()> {
    let object = format!("{tree}^{{tree}}");
    let output = approval_git_command()
        .arg("-C")
        .arg(project_root)
        .args(["cat-file", "-e"])
        .arg(object)
        .output()
        .await?;
    git_success(&output, "verify frozen submitted tree")
}

pub(super) async fn submitted_tree_from_patch(
    context: &RuntimeTaskContext,
    project_root: &Path,
    baseline: &str,
) -> Result<String> {
    let index = TemporaryIntegrationIndex::new();
    read_tree(project_root, index.path(), baseline, false).await?;
    let patch_path = store::resolve_project_path(Path::new(&context.run_dir).join("PATCH.diff"));
    let output = approval_git_command()
        .arg("-C")
        .arg(project_root)
        .env("GIT_INDEX_FILE", index.path())
        .args(["apply", "--cached", "--whitespace=nowarn"])
        .arg(patch_path)
        .output()
        .await?;
    git_success(&output, "apply submitted patch to its baseline")?;
    let output = approval_git_command()
        .arg("-C")
        .arg(project_root)
        .env("GIT_INDEX_FILE", index.path())
        .arg("write-tree")
        .output()
        .await?;
    git_stdout(output, "write submitted tree")
}

pub(super) async fn merge_trees(
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
    let output = approval_git_command()
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
pub(super) struct TreeChange {
    pub(super) path: Vec<u8>,
    pub(super) is_addition: bool,
    pub(super) changes_gitlink: bool,
}

pub(super) async fn validate_tree_materialization(
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

pub(super) async fn tree_changes(
    project_root: &Path,
    current_tree: &str,
    target_tree: &str,
) -> Result<Vec<TreeChange>> {
    let output = approval_git_command()
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

pub(super) fn parse_raw_tree_changes(raw: &[u8]) -> Result<Vec<TreeChange>> {
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

pub(super) async fn reject_ignored_path_collisions(
    project_root: &Path,
    changes: &[TreeChange],
) -> Result<()> {
    let additions: Vec<&[u8]> = changes
        .iter()
        .filter(|change| change.is_addition)
        .map(|change| change.path.as_slice())
        .collect();
    if additions.is_empty() {
        return Ok(());
    }

    let output = approval_git_command()
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

pub(super) fn paths_overlap(left: &[u8], right: &[u8]) -> bool {
    left == right || path_is_parent(left, right) || path_is_parent(right, left)
}

pub(super) fn path_is_parent(parent: &[u8], child: &[u8]) -> bool {
    child
        .strip_prefix(parent)
        .is_some_and(|remainder| remainder.starts_with(b"/"))
}

pub(super) async fn reject_sparse_checkout_exclusions(
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
    let mut command = approval_git_command();
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

pub(super) async fn sparse_checkout_enabled(project_root: &Path) -> Result<bool> {
    let output = approval_git_command()
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

pub(super) fn nul_terminated_paths<'a>(paths: impl IntoIterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path);
        input.push(0);
    }

    input
}

pub(super) async fn command_output_with_stdin(
    command: &mut Command,
    input: &[u8],
) -> Result<Output> {
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

pub(super) async fn commit_tree(
    project_root: &Path,
    tree: &str,
    parent: Option<&str>,
    role: &str,
) -> Result<String> {
    let mut command = approval_git_command();
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

pub(super) async fn materialize_tree(
    project_root: &Path,
    current_tree: &str,
    target_tree: &str,
) -> Result<()> {
    let index = TemporaryIntegrationIndex::new();
    read_tree(project_root, index.path(), current_tree, false).await?;
    read_tree(project_root, index.path(), target_tree, true).await
}

pub(super) async fn tree_without_paths(
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
    let mut command = approval_git_command();
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
    let output = approval_git_command()
        .arg("-C")
        .arg(project_root)
        .env("GIT_INDEX_FILE", index.path())
        .arg("write-tree")
        .output()
        .await?;
    git_stdout(output, "write rollback tree without ignored paths")
}

pub(super) static TEMPORARY_INTEGRATION_INDEX_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(super) struct TemporaryIntegrationIndex {
    pub(super) path: PathBuf,
}

impl TemporaryIntegrationIndex {
    pub(super) fn new() -> Self {
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

    pub(super) fn path(&self) -> &Path {
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

pub(super) async fn read_tree(
    project_root: &Path,
    index: &Path,
    tree: &str,
    update_worktree: bool,
) -> Result<()> {
    let mut command = approval_git_command();
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

pub(super) fn git_stdout(output: Output, operation: &str) -> Result<String> {
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

pub(super) fn git_success(output: &Output, operation: &str) -> Result<()> {
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

pub(super) fn bounded_git_detail(detail: &str) -> String {
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
