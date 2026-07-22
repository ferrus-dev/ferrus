use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Output,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;

use super::{
    LocalRepositorySource, SourceContent, SourceDiscoveryContext, SourceError,
    git::{canonical_root, ensure_worktree_root, git_command, trim_ascii},
    sha256_digest,
};
use crate::repository_graph::{
    domain::{Digest, OverlayRevisionId, RepoPath, WorkspaceRef},
    ports::{OverlayChangeKind, OverlayFileChange, SourceFileDescriptor, WorkspaceOverlayManifest},
};

static TEMPORARY_INDEX_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitWorktreeChange {
    pub path: RepoPath,
    pub kind: OverlayChangeKind,
    pub renamed_from: Option<RepoPath>,
}

#[derive(Debug, Clone)]
struct BaselineEntry {
    object_id: String,
    mode: String,
}

/// Read-only path inventory for a Git worktree relative to an immutable tree.
///
/// The real Git index is never changed. Ignored untracked paths and Ferrus
/// runtime metadata are omitted consistently for graph and patch consumers.
#[derive(Debug, Clone)]
pub struct GitWorktreeInventory {
    root: PathBuf,
    baseline_revision: Digest,
    baseline_paths: Vec<RepoPath>,
    tracked_paths: Vec<RepoPath>,
    untracked_paths: Vec<RepoPath>,
    changes: Vec<GitWorktreeChange>,
}

impl GitWorktreeInventory {
    pub fn discover(
        root: impl AsRef<Path>,
        baseline_revision: Digest,
    ) -> Result<Self, SourceError> {
        validate_git_tree_digest(&baseline_revision)?;
        let root = canonical_root(root.as_ref())?;
        ensure_worktree_root(&root)?;
        verify_tree(&root, &baseline_revision)?;

        let baseline_entries = baseline_entries(&root, &baseline_revision)?;
        let tracked_paths = listed_paths(&root, &["ls-files", "-z"], "list tracked paths")?;
        let untracked_paths = listed_paths(
            &root,
            &["ls-files", "--others", "--exclude-standard", "-z"],
            "list untracked paths",
        )?;
        let changes = changed_paths(
            &root,
            &baseline_revision,
            &baseline_entries,
            &tracked_paths,
            &untracked_paths,
        )?;

        Ok(Self {
            root,
            baseline_revision,
            baseline_paths: baseline_entries.keys().cloned().collect(),
            tracked_paths,
            untracked_paths,
            changes,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn baseline_revision(&self) -> &Digest {
        &self.baseline_revision
    }

    pub fn baseline_paths(&self) -> &[RepoPath] {
        &self.baseline_paths
    }

    pub fn tracked_paths(&self) -> &[RepoPath] {
        &self.tracked_paths
    }

    pub fn untracked_paths(&self) -> &[RepoPath] {
        &self.untracked_paths
    }

    pub fn changes(&self) -> &[GitWorktreeChange] {
        &self.changes
    }
}

/// Policy-aware, content-addressed overlay inputs for one task worktree.
#[derive(Debug, Clone)]
pub struct TaskWorktreeOverlay {
    root: PathBuf,
    context: SourceDiscoveryContext,
    source: LocalRepositorySource,
    manifest: WorkspaceOverlayManifest,
}

impl TaskWorktreeOverlay {
    pub fn discover(
        root: impl AsRef<Path>,
        workspace: WorkspaceRef,
        context: SourceDiscoveryContext,
    ) -> Result<Self, SourceError> {
        if workspace.repository != *context.repository() {
            return Err(SourceError::RepositoryMismatch);
        }
        let inventory =
            GitWorktreeInventory::discover(root.as_ref(), workspace.baseline_revision.clone())?;
        let source = LocalRepositorySource::discover(inventory.root(), context.clone())?;
        if !matches!(source, LocalRepositorySource::Git(_)) {
            return Err(SourceError::NotGitRoot);
        }
        let current_files = source
            .manifest()
            .files
            .iter()
            .map(|file| (file.path.clone(), file.clone()))
            .collect::<BTreeMap<_, _>>();
        let changes = inventory
            .changes()
            .iter()
            .map(|change| OverlayFileChange {
                path: change.path.clone(),
                kind: change.kind,
                renamed_from: change.renamed_from.clone(),
                current_file: if change.kind == OverlayChangeKind::Deleted {
                    None
                } else {
                    current_files.get(&change.path).cloned()
                },
            })
            .collect::<Vec<_>>();
        let manifest_digest = overlay_manifest_digest(&workspace, &context, &changes);
        let revision_id = overlay_revision_id(&workspace, &manifest_digest);
        let source_manifest = source.manifest();
        let manifest = WorkspaceOverlayManifest {
            workspace,
            revision_id,
            manifest_digest,
            analysis_config_digest: context.analysis_config_digest.clone(),
            source_policy_digest: context.source_policy_digest.clone(),
            extractor_set_digest: context.extractor_set_digest.clone(),
            changes,
            diagnostics: source_manifest.diagnostics.clone(),
            metrics: source_manifest.metrics.clone(),
        };

        Ok(Self {
            root: inventory.root,
            context,
            source,
            manifest,
        })
    }

    pub fn manifest(&self) -> &WorkspaceOverlayManifest {
        &self.manifest
    }

    pub fn read_verified(&self, file: &SourceFileDescriptor) -> Result<SourceContent, SourceError> {
        let is_changed_input = self
            .manifest
            .changes
            .iter()
            .filter_map(|change| change.current_file.as_ref())
            .any(|changed| changed == file);
        if !is_changed_input {
            return Err(SourceError::FileNotInManifest);
        }
        self.source.read_verified(file)
    }

    /// Recomputes both Git delta and effective source policy. Any included
    /// content, mode, path, policy, analyzer, or extractor change invalidates
    /// the overlay revision.
    pub fn revalidate(&self) -> Result<bool, SourceError> {
        let current = Self::discover(
            &self.root,
            self.manifest.workspace.clone(),
            self.context.clone(),
        )?;
        Ok(current.manifest == self.manifest)
    }
}

#[derive(Serialize)]
struct CanonicalOverlayManifest<'a> {
    version: u32,
    repository: &'a crate::repository_graph::domain::RepositoryRef,
    baseline_revision: &'a Digest,
    analysis_config_digest: &'a Digest,
    source_policy_digest: &'a Digest,
    extractor_set_digest: &'a Digest,
    changes: &'a [OverlayFileChange],
}

fn overlay_manifest_digest(
    workspace: &WorkspaceRef,
    context: &SourceDiscoveryContext,
    changes: &[OverlayFileChange],
) -> Digest {
    let canonical = CanonicalOverlayManifest {
        version: 1,
        repository: &workspace.repository,
        baseline_revision: &workspace.baseline_revision,
        analysis_config_digest: &context.analysis_config_digest,
        source_policy_digest: &context.source_policy_digest,
        extractor_set_digest: &context.extractor_set_digest,
        changes,
    };
    sha256_digest(
        &serde_json::to_vec(&canonical)
            .expect("canonical overlay manifest serialization cannot fail"),
    )
}

#[derive(Serialize)]
struct CanonicalOverlayRevision<'a> {
    version: u32,
    task_view_id: &'a crate::repository_graph::domain::TaskViewId,
    manifest_digest: &'a Digest,
}

fn overlay_revision_id(workspace: &WorkspaceRef, manifest_digest: &Digest) -> OverlayRevisionId {
    let canonical = CanonicalOverlayRevision {
        version: 1,
        task_view_id: &workspace.task_view_id,
        manifest_digest,
    };
    let digest = sha256_digest(
        &serde_json::to_vec(&canonical)
            .expect("canonical overlay revision serialization cannot fail"),
    );
    OverlayRevisionId::new(format!("sha256:{}", digest.value()))
        .expect("derived overlay revision identity is never empty")
}

pub fn parse_git_tree_digest(value: &str) -> Result<Digest, SourceError> {
    let value = value.trim();
    let algorithm = match value.len() {
        40 => "git-tree-sha1",
        64 => "git-tree-sha256",
        _ => return Err(SourceError::InvalidGitTree),
    };
    Digest::new(algorithm, value).map_err(|_| SourceError::InvalidGitTree)
}

fn validate_git_tree_digest(digest: &Digest) -> Result<(), SourceError> {
    match (digest.algorithm(), digest.value().len()) {
        ("git-tree-sha1", 40) | ("git-tree-sha256", 64) => Ok(()),
        _ => Err(SourceError::InvalidGitTree),
    }
}

/// Captures the effective worktree as a Git tree without changing its real
/// index. This is shared by HQ baseline creation and graph baseline checks.
pub fn capture_worktree_tree(root: impl AsRef<Path>) -> Result<Digest, SourceError> {
    let root = canonical_root(root.as_ref())?;
    ensure_worktree_root(&root)?;
    let temporary_index = TemporaryGitIndex::new();
    let head = run_git_allow_failure(&root, &["rev-parse", "--verify", "HEAD^{tree}"])?;
    if head.status.success() {
        let value = std::str::from_utf8(trim_ascii(&head.stdout))
            .map_err(|_| SourceError::InvalidGitTree)?;
        run_git_with_index(
            &root,
            temporary_index.path(),
            &["read-tree", value],
            "seed temporary index",
        )?;
    } else {
        run_git_with_index(
            &root,
            temporary_index.path(),
            &["read-tree", "--empty"],
            "seed empty temporary index",
        )?;
    }
    run_git_with_index(
        &root,
        temporary_index.path(),
        &["add", "-A", "--", "."],
        "capture worktree paths",
    )?;
    run_git_with_index(
        &root,
        temporary_index.path(),
        &["rm", "--cached", "-r", "--ignore-unmatch", "--", ".ferrus"],
        "exclude Ferrus runtime paths",
    )?;
    let tree = run_git_with_index(
        &root,
        temporary_index.path(),
        &["write-tree"],
        "write captured worktree tree",
    )?;
    parse_git_tree_digest(
        std::str::from_utf8(trim_ascii(&tree.stdout)).map_err(|_| SourceError::InvalidGitTree)?,
    )
}

fn verify_tree(root: &Path, baseline: &Digest) -> Result<(), SourceError> {
    let tree = format!("{}^{{tree}}", baseline.value());
    run_git(root, &["cat-file", "-e", &tree], "verify baseline tree").map(|_| ())
}

fn baseline_entries(
    root: &Path,
    baseline: &Digest,
) -> Result<BTreeMap<RepoPath, BaselineEntry>, SourceError> {
    let output = run_git(
        root,
        &["ls-tree", "-r", "-z", "--full-tree", baseline.value()],
        "list baseline tree",
    )?;
    let mut entries = BTreeMap::new();
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or(SourceError::GitCommand {
                operation: "parse baseline tree",
            })?;
        let header = std::str::from_utf8(&record[..tab]).map_err(|_| SourceError::GitCommand {
            operation: "parse baseline tree",
        })?;
        let mut fields = header.split_ascii_whitespace();
        let mode = fields.next().ok_or(SourceError::GitCommand {
            operation: "parse baseline tree",
        })?;
        let _kind = fields.next().ok_or(SourceError::GitCommand {
            operation: "parse baseline tree",
        })?;
        let object_id = fields.next().ok_or(SourceError::GitCommand {
            operation: "parse baseline tree",
        })?;
        if fields.next().is_some() {
            return Err(SourceError::GitCommand {
                operation: "parse baseline tree",
            });
        }
        let path = parse_path(&record[tab + 1..], "parse baseline tree path")?;
        if !is_runtime_path(&path) {
            entries.insert(
                path,
                BaselineEntry {
                    object_id: object_id.to_string(),
                    mode: mode.to_string(),
                },
            );
        }
    }
    Ok(entries)
}

fn listed_paths(
    root: &Path,
    arguments: &[&str],
    operation: &'static str,
) -> Result<Vec<RepoPath>, SourceError> {
    let output = run_git(root, arguments, operation)?;
    let mut paths = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| parse_path(path, operation))
        .collect::<Result<Vec<_>, _>>()?;
    paths.retain(|path| !is_runtime_path(path));
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn changed_paths(
    root: &Path,
    baseline: &Digest,
    baseline_entries: &BTreeMap<RepoPath, BaselineEntry>,
    tracked_paths: &[RepoPath],
    untracked_paths: &[RepoPath],
) -> Result<Vec<GitWorktreeChange>, SourceError> {
    let output = run_git(
        root,
        &[
            "diff",
            "--name-status",
            "-z",
            "--no-ext-diff",
            "--no-textconv",
            "--find-renames",
            "--ignore-submodules=all",
            baseline.value(),
            "--",
        ],
        "compare worktree to baseline",
    )?;
    let records = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .collect::<Vec<_>>();
    let mut changes = BTreeMap::<RepoPath, GitWorktreeChange>::new();
    let mut index = 0;
    while index < records.len() {
        let status = records[index];
        index += 1;
        let code = *status.first().ok_or(SourceError::GitCommand {
            operation: "parse worktree changes",
        })?;
        match code {
            b'R' => {
                let old = next_change_path(&records, &mut index)?;
                let new = next_change_path(&records, &mut index)?;
                insert_change(&mut changes, old.clone(), OverlayChangeKind::Deleted, None);
                insert_change(&mut changes, new, OverlayChangeKind::Added, Some(old));
            }
            b'C' => {
                let _old = next_change_path(&records, &mut index)?;
                let new = next_change_path(&records, &mut index)?;
                insert_change(&mut changes, new, OverlayChangeKind::Added, None);
            }
            b'A' => {
                let path = next_change_path(&records, &mut index)?;
                insert_change(&mut changes, path, OverlayChangeKind::Added, None);
            }
            b'D' => {
                let path = next_change_path(&records, &mut index)?;
                insert_change(&mut changes, path, OverlayChangeKind::Deleted, None);
            }
            b'M' | b'T' | b'U' => {
                let path = next_change_path(&records, &mut index)?;
                insert_change(&mut changes, path, OverlayChangeKind::Modified, None);
            }
            _ => {
                return Err(SourceError::GitCommand {
                    operation: "parse worktree change status",
                });
            }
        }
    }

    let tracked = tracked_paths.iter().cloned().collect::<BTreeSet<_>>();
    let untracked = untracked_paths.iter().cloned().collect::<BTreeSet<_>>();
    for (path, entry) in baseline_entries {
        if untracked.contains(path) {
            changes.remove(path);
            if !untracked_matches_baseline(root, path, entry)? {
                insert_change(
                    &mut changes,
                    path.clone(),
                    OverlayChangeKind::Modified,
                    None,
                );
            }
        } else if !tracked.contains(path) {
            insert_change(&mut changes, path.clone(), OverlayChangeKind::Deleted, None);
        }
    }
    for path in tracked.iter().chain(untracked.iter()) {
        if !baseline_entries.contains_key(path) {
            changes
                .entry(path.clone())
                .or_insert_with(|| GitWorktreeChange {
                    path: path.clone(),
                    kind: OverlayChangeKind::Added,
                    renamed_from: None,
                });
        }
    }

    Ok(changes.into_values().collect())
}

fn next_change_path(records: &[&[u8]], index: &mut usize) -> Result<RepoPath, SourceError> {
    let record = records.get(*index).ok_or(SourceError::GitCommand {
        operation: "parse worktree changes",
    })?;
    *index += 1;
    parse_path(record, "parse worktree change path")
}

fn insert_change(
    changes: &mut BTreeMap<RepoPath, GitWorktreeChange>,
    path: RepoPath,
    kind: OverlayChangeKind,
    renamed_from: Option<RepoPath>,
) {
    if !is_runtime_path(&path) {
        changes.insert(
            path.clone(),
            GitWorktreeChange {
                path,
                kind,
                renamed_from,
            },
        );
    }
}

fn untracked_matches_baseline(
    root: &Path,
    path: &RepoPath,
    baseline: &BaselineEntry,
) -> Result<bool, SourceError> {
    let output = run_git(
        root,
        &["hash-object", "--no-filters", "--", path.as_str()],
        "hash baseline-seeded worktree path",
    )?;
    let object_id =
        std::str::from_utf8(trim_ascii(&output.stdout)).map_err(|_| SourceError::GitCommand {
            operation: "parse worktree blob identity",
        })?;
    let mode = observed_git_mode(root.join(path.as_str()))?;
    Ok(object_id == baseline.object_id && mode == baseline.mode)
}

fn observed_git_mode(path: PathBuf) -> Result<&'static str, SourceError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| SourceError::Io {
        operation: "inspect worktree path mode",
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Ok("120000");
    }
    if !metadata.is_file() {
        return Ok("160000");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 != 0 {
            return Ok("100755");
        }
    }
    Ok("100644")
}

fn parse_path(raw: &[u8], operation: &'static str) -> Result<RepoPath, SourceError> {
    let value = std::str::from_utf8(raw).map_err(|_| SourceError::GitCommand { operation })?;
    RepoPath::new(value).map_err(|_| SourceError::GitCommand { operation })
}

fn is_runtime_path(path: &RepoPath) -> bool {
    path.as_str().split('/').any(|component| {
        component.eq_ignore_ascii_case(".git") || component.eq_ignore_ascii_case(".ferrus")
    })
}

fn run_git(
    root: &Path,
    arguments: &[&str],
    operation: &'static str,
) -> Result<Output, SourceError> {
    let output = run_git_allow_failure(root, arguments)?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(SourceError::GitCommand { operation })
    }
}

fn run_git_allow_failure(root: &Path, arguments: &[&str]) -> Result<Output, SourceError> {
    git_command(root, arguments)
        .output()
        .map_err(|_| SourceError::GitCommand {
            operation: "run Git worktree command",
        })
}

fn run_git_with_index(
    root: &Path,
    index: &Path,
    arguments: &[&str],
    operation: &'static str,
) -> Result<Output, SourceError> {
    let output = git_command(root, arguments)
        .env("GIT_INDEX_FILE", index)
        .output()
        .map_err(|_| SourceError::GitCommand { operation })?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(SourceError::GitCommand { operation })
    }
}

struct TemporaryGitIndex {
    path: PathBuf,
}

impl TemporaryGitIndex {
    fn new() -> Self {
        let sequence = TEMPORARY_INDEX_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            path: std::env::temp_dir().join(format!(
                "ferrus-git-index-{}-{nanos:x}-{sequence:x}",
                std::process::id()
            )),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryGitIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let mut lock = self.path.as_os_str().to_owned();
        lock.push(".lock");
        let _ = fs::remove_file(PathBuf::from(lock));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository_graph::{
        config::RepositoryGraphConfig,
        domain::{RepositoryId, RepositoryNamespace, RepositoryRef, TaskViewId},
    };
    use std::process::Command;

    fn git(root: &Path, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            arguments,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn initialize(root: &Path) {
        git(root, &["init"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "user.name", "Ferrus Test"]);
        git(root, &["config", "commit.gpgsign", "false"]);
    }

    fn repository() -> RepositoryRef {
        RepositoryRef {
            namespace: RepositoryNamespace::new("local:test").unwrap(),
            repository_id: RepositoryId::new("root").unwrap(),
        }
    }

    fn workspace(task: &str, baseline_revision: Digest) -> WorkspaceRef {
        WorkspaceRef {
            repository: repository(),
            task_view_id: TaskViewId::new(task).unwrap(),
            baseline_revision,
        }
    }

    #[test]
    fn inventory_reports_add_change_delete_and_rename_without_touching_the_index() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        initialize(root);
        fs::write(root.join(".gitignore"), "ignored.log\n.ferrus/\n").unwrap();
        fs::write(root.join("modified.rs"), "pub struct Before;\n").unwrap();
        fs::write(root.join("deleted.rs"), "pub struct Deleted;\n").unwrap();
        fs::write(root.join("renamed_old.rs"), "pub struct Renamed;\n").unwrap();
        fs::write(root.join("unchanged.rs"), "pub struct Unchanged;\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "baseline"]);
        let baseline = parse_git_tree_digest(&git(root, &["rev-parse", "HEAD^{tree}"])).unwrap();

        fs::write(root.join("modified.rs"), "pub struct After;\n").unwrap();
        fs::remove_file(root.join("deleted.rs")).unwrap();
        git(root, &["mv", "renamed_old.rs", "renamed_new.rs"]);
        fs::write(root.join("added.rs"), "pub struct Added;\n").unwrap();
        fs::write(root.join("ignored.log"), "ignored\n").unwrap();
        fs::create_dir(root.join(".ferrus")).unwrap();
        fs::write(root.join(".ferrus/runtime"), "runtime\n").unwrap();
        let index_before = git(root, &["ls-files", "--stage"]);

        let inventory = GitWorktreeInventory::discover(root, baseline).unwrap();
        let changes = inventory
            .changes()
            .iter()
            .map(|change| {
                (
                    change.path.as_str(),
                    change.kind,
                    change.renamed_from.as_ref().map(RepoPath::as_str),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            changes,
            vec![
                ("added.rs", OverlayChangeKind::Added, None),
                ("deleted.rs", OverlayChangeKind::Deleted, None),
                ("modified.rs", OverlayChangeKind::Modified, None),
                (
                    "renamed_new.rs",
                    OverlayChangeKind::Added,
                    Some("renamed_old.rs")
                ),
                ("renamed_old.rs", OverlayChangeKind::Deleted, None),
            ]
        );
        assert_eq!(git(root, &["ls-files", "--stage"]), index_before);
        assert!(
            !inventory
                .untracked_paths()
                .iter()
                .any(|path| path.as_str() == "ignored.log")
        );
    }

    #[test]
    fn captured_tree_preserves_the_real_index_and_is_an_empty_overlay() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        initialize(root);
        fs::write(root.join("tracked.rs"), "pub struct Base;\n").unwrap();
        git(root, &["add", "tracked.rs"]);
        git(root, &["commit", "-m", "baseline"]);
        fs::write(root.join("tracked.rs"), "pub struct Seeded;\n").unwrap();
        fs::write(root.join("seeded.rs"), "pub struct SeededFile;\n").unwrap();
        let index_before = git(root, &["ls-files", "--stage"]);

        let captured = capture_worktree_tree(root).unwrap();
        let inventory = GitWorktreeInventory::discover(root, captured).unwrap();

        assert!(inventory.changes().is_empty());
        assert_eq!(git(root, &["ls-files", "--stage"]), index_before);
    }

    #[test]
    fn overlay_manifest_applies_policy_and_invalidates_on_indexable_content_changes() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        initialize(root);
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub struct Before;\n").unwrap();
        fs::write(root.join("secret.txt"), "before-secret\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "baseline"]);
        let baseline = parse_git_tree_digest(&git(root, &["rev-parse", "HEAD^{tree}"])).unwrap();
        fs::write(root.join("src/lib.rs"), "pub struct After;\n").unwrap();
        fs::write(root.join("secret.txt"), "after-secret\n").unwrap();

        let mut config = RepositoryGraphConfig::default();
        config.source.sensitive = ["secret.txt".to_string()].into_iter().collect();
        let context = SourceDiscoveryContext::from_config(repository(), &config, &[]).unwrap();
        let overlay = TaskWorktreeOverlay::discover(
            root,
            workspace("task-a", baseline.clone()),
            context.clone(),
        )
        .unwrap();
        let indexed = overlay
            .manifest()
            .changes
            .iter()
            .find(|change| change.path.as_str() == "src/lib.rs")
            .unwrap();
        let sensitive = overlay
            .manifest()
            .changes
            .iter()
            .find(|change| change.path.as_str() == "secret.txt")
            .unwrap();
        let indexed_file = indexed.current_file.clone().unwrap();
        assert_eq!(
            overlay.read_verified(&indexed_file).unwrap().bytes,
            b"pub struct After;\n"
        );
        assert!(sensitive.current_file.is_none());
        assert!(overlay.revalidate().unwrap());

        let other_task =
            TaskWorktreeOverlay::discover(root, workspace("task-b", baseline), context).unwrap();
        assert_eq!(
            overlay.manifest().manifest_digest,
            other_task.manifest().manifest_digest
        );
        assert_ne!(
            overlay.manifest().revision_id,
            other_task.manifest().revision_id
        );

        fs::write(root.join("src/lib.rs"), "pub struct ChangedAgain;\n").unwrap();
        assert!(matches!(
            overlay.read_verified(&indexed_file),
            Err(SourceError::ContentChanged)
        ));
        assert!(!overlay.revalidate().unwrap());
    }
}
