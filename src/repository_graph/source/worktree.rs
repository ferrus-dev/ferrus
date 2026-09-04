//! Compose pinned Git baselines with task worktree changes and capture immutable source trees.

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
    DiagnosticCollector, LocalRepositorySource, SourceContent, SourceDiscoveryContext, SourceError,
    canonical_source_manifest_digest,
    git::{canonical_root, ensure_worktree_root, git_command, trim_ascii},
    is_binary, revision_id, sha256_digest,
};
use crate::repository_graph::{
    domain::{Digest, OverlayRevisionId, RepoPath, SourceKind, SourceRevision, WorkspaceRef},
    ports::{
        OverlayChangeKind, OverlayFileChange, RepositorySource, SourceDiscoveryMetrics,
        SourceFileDescriptor, SourceFileMode, SourceManifest, WorkspaceOverlayManifest,
    },
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
    baseline_objects: BTreeMap<RepoPath, String>,
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
            baseline_objects: baseline_entries
                .iter()
                .map(|(path, entry)| (path.clone(), entry.object_id.clone()))
                .collect(),
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

    fn baseline_object(&self, path: &RepoPath) -> Option<&str> {
        self.baseline_objects.get(path).map(String::as_str)
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

/// Effective task source composed from immutable baseline descriptors and the
/// policy-filtered worktree delta. Unchanged bytes are read from the pinned Git
/// tree, while changed bytes cross the worktree's hash-verifying boundary.
/// This lets the indexer reuse cached baseline fragments and parse only changed
/// paths while still running cross-file resolution over the complete view.
#[derive(Debug, Clone)]
pub struct TaskOverlaySource {
    inventory: GitWorktreeInventory,
    overlay: TaskWorktreeOverlay,
    manifest: SourceManifest,
    baseline_files: BTreeMap<RepoPath, SourceFileDescriptor>,
    baseline_manifest_rebuilt: bool,
}

impl TaskOverlaySource {
    pub fn discover(
        root: impl AsRef<Path>,
        workspace: WorkspaceRef,
        context: SourceDiscoveryContext,
        baseline_analysis_config_digest: Digest,
        baseline_files: Vec<SourceFileDescriptor>,
    ) -> Result<Self, SourceError> {
        let inventory =
            GitWorktreeInventory::discover(root.as_ref(), workspace.baseline_revision.clone())?;
        let overlay = TaskWorktreeOverlay::discover(root, workspace.clone(), context.clone())?;
        // Source policy is part of the analysis configuration identity. Once
        // that identity changes, the stored baseline descriptor set may omit
        // newly included paths or retain newly sensitive ones, so derive it
        // again from the immutable tree under the current policy.
        let baseline_manifest_rebuilt =
            baseline_analysis_config_digest != *context.analysis_config_digest();
        let baseline_files = if baseline_manifest_rebuilt {
            discover_tree_manifest(
                inventory.root(),
                &context,
                workspace.baseline_revision.clone(),
            )?
            .files
        } else {
            baseline_files
        };
        let baseline_files = baseline_files
            .into_iter()
            .map(|file| (file.path.clone(), file))
            .collect::<BTreeMap<_, _>>();
        if baseline_files
            .keys()
            .any(|path| inventory.baseline_object(path).is_none())
        {
            return Err(SourceError::FileNotInManifest);
        }

        let mut effective_files = baseline_files.clone();
        for change in &overlay.manifest().changes {
            match &change.current_file {
                Some(file) => {
                    effective_files.insert(change.path.clone(), file.clone());
                }
                None => {
                    effective_files.remove(&change.path);
                }
            }
        }
        let files = effective_files.into_values().collect::<Vec<_>>();
        let included = files.len() as u64;
        let effective_manifest_digest =
            canonical_source_manifest_digest(&files, &context.source_policy_digest);
        let source_manifest_digest = composed_manifest_digest(
            &effective_manifest_digest,
            &overlay.manifest().manifest_digest,
        );
        let dirty = !overlay.manifest().changes.is_empty();
        let includes_untracked = overlay
            .manifest()
            .changes
            .iter()
            .any(|change| change.kind == OverlayChangeKind::Added);
        let source_kind = SourceKind::WorkspaceOverlay;
        let base_revision = Some(workspace.baseline_revision.clone());
        let source_revision = SourceRevision {
            id: revision_id(
                &workspace.repository,
                source_kind,
                base_revision.as_ref(),
                &source_manifest_digest,
                &context.analysis_config_digest,
                dirty,
                includes_untracked,
            ),
            repository: workspace.repository,
            source_kind,
            base_revision,
            manifest_digest: source_manifest_digest,
            analysis_config_digest: context.analysis_config_digest.clone(),
            dirty,
            includes_untracked,
        };
        let total_bytes = files
            .iter()
            .fold(0_u64, |total, file| total.saturating_add(file.byte_len));
        let manifest = SourceManifest {
            revision: source_revision,
            extractor_set_digest: context.extractor_set_digest.clone(),
            files,
            diagnostics: overlay.manifest().diagnostics.clone(),
            metrics: SourceDiscoveryMetrics {
                included,
                total_bytes,
                ..overlay.manifest().metrics.clone()
            },
        };

        Ok(Self {
            inventory,
            overlay,
            manifest,
            baseline_files,
            baseline_manifest_rebuilt,
        })
    }

    pub fn overlay_manifest(&self) -> &WorkspaceOverlayManifest {
        self.overlay.manifest()
    }

    pub fn requires_index(&self) -> bool {
        self.baseline_manifest_rebuilt || !self.overlay.manifest().changes.is_empty()
    }

    fn read_baseline_verified(
        &self,
        file: &SourceFileDescriptor,
    ) -> Result<SourceContent, SourceError> {
        if self.baseline_files.get(&file.path) != Some(file) {
            return Err(SourceError::FileNotInManifest);
        }
        let object_id = self
            .inventory
            .baseline_object(&file.path)
            .ok_or(SourceError::FileNotInManifest)?;
        let output = run_git(
            &self.inventory.root,
            &["cat-file", "blob", object_id],
            "read pinned baseline blob",
        )?;
        if output.stdout.len() as u64 != file.byte_len
            || sha256_digest(&output.stdout) != file.content_identity
        {
            return Err(SourceError::ContentChanged);
        }
        Ok(SourceContent {
            bytes: output.stdout,
        })
    }
}

#[derive(Serialize)]
struct ComposedManifestIdentity<'a> {
    version: u32,
    effective_manifest_digest: &'a Digest,
    overlay_manifest_digest: &'a Digest,
}

fn composed_manifest_digest(
    effective_manifest_digest: &Digest,
    overlay_manifest_digest: &Digest,
) -> Digest {
    sha256_digest(
        &serde_json::to_vec(&ComposedManifestIdentity {
            version: 1,
            effective_manifest_digest,
            overlay_manifest_digest,
        })
        .expect("canonical composed manifest serialization cannot fail"),
    )
}

impl RepositorySource for TaskOverlaySource {
    type Error = SourceError;

    fn repository(&self) -> &crate::repository_graph::domain::RepositoryRef {
        &self.manifest.revision.repository
    }

    fn manifest(&self) -> &SourceManifest {
        &self.manifest
    }

    fn read_verified(&self, file: &SourceFileDescriptor) -> Result<SourceContent, Self::Error> {
        if self
            .overlay
            .manifest()
            .changes
            .iter()
            .filter_map(|change| change.current_file.as_ref())
            .any(|changed| changed == file)
        {
            self.overlay.read_verified(file)
        } else {
            self.read_baseline_verified(file)
        }
    }

    fn revalidate(&self) -> Result<bool, Self::Error> {
        self.overlay.revalidate()
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

/// Keeps a submitted tree and all of its blobs reachable while the task is in
/// review. The ref is task-scoped because there can only be one active frozen
/// submission per task, and the reviewing run has a different run identity
/// from the Executor that created the tree.
pub fn pin_submitted_tree(
    root: impl AsRef<Path>,
    task_id: &str,
    tree: &Digest,
) -> Result<(), SourceError> {
    validate_git_tree_digest(tree)?;
    let root = canonical_root(root.as_ref())?;
    ensure_worktree_root(&root)?;
    let reference = submitted_tree_ref(task_id);
    run_git(
        &root,
        &["update-ref", &reference, tree.value()],
        "pin submitted tree",
    )?;
    Ok(())
}

pub fn release_submitted_tree_pin(
    root: impl AsRef<Path>,
    task_id: &str,
) -> Result<(), SourceError> {
    let root = canonical_root(root.as_ref())?;
    ensure_worktree_root(&root)?;
    let reference = submitted_tree_ref(task_id);
    run_git(
        &root,
        &["update-ref", "-d", &reference],
        "release submitted tree pin",
    )?;
    Ok(())
}

fn submitted_tree_ref(task_id: &str) -> String {
    let digest = sha256_digest(task_id.as_bytes());
    format!("refs/ferrus/reviews/{}", digest.value())
}

fn verify_tree(root: &Path, baseline: &Digest) -> Result<(), SourceError> {
    let tree = format!("{}^{{tree}}", baseline.value());
    run_git(root, &["cat-file", "-e", &tree], "verify baseline tree").map(|_| ())
}

pub(super) fn verify_tree_available(root: &Path, tree: &Digest) -> Result<(), SourceError> {
    validate_git_tree_digest(tree)?;
    verify_tree(root, tree)
}

pub(super) fn discover_tree_manifest(
    root: &Path,
    context: &SourceDiscoveryContext,
    tree: Digest,
) -> Result<SourceManifest, SourceError> {
    validate_git_tree_digest(&tree)?;
    verify_tree(root, &tree)?;
    let entries = baseline_entries(root, &tree)?;
    if entries.len() as u64 > context.limits.max_files {
        return Err(SourceError::FileLimitExceeded {
            limit: context.limits.max_files,
        });
    }

    let mut diagnostics = DiagnosticCollector::new(context.limits.max_diagnostics);
    let mut metrics = SourceDiscoveryMetrics {
        candidates: entries.len() as u64,
        ..SourceDiscoveryMetrics::default()
    };
    let mut directories = BTreeSet::new();
    let mut files = Vec::new();

    for (path, entry) in entries {
        record_tree_parent_directories(&path, &mut directories, context.limits.max_directories)?;
        if let Some(code) = context.policy.exclusion_for_file(&path) {
            diagnostics.push(code, Some(path));
            metrics.skipped = metrics.skipped.saturating_add(1);
            continue;
        }
        let mode = match entry.mode.as_str() {
            "100644" => SourceFileMode::Regular,
            "100755" => SourceFileMode::Executable,
            "120000" => {
                diagnostics.push("symlink_skipped", Some(path));
                metrics.skipped = metrics.skipped.saturating_add(1);
                continue;
            }
            "160000" => {
                diagnostics.push("gitlink_skipped", Some(path));
                metrics.skipped = metrics.skipped.saturating_add(1);
                continue;
            }
            _ => {
                diagnostics.push("special_file_skipped", Some(path));
                metrics.skipped = metrics.skipped.saturating_add(1);
                continue;
            }
        };
        let size = tree_blob_size(root, &entry.object_id)?;
        if size > context.limits.max_file_bytes {
            diagnostics.push("file_too_large", Some(path));
            metrics.skipped = metrics.skipped.saturating_add(1);
            continue;
        }
        if metrics.total_bytes.saturating_add(size) > context.limits.max_total_bytes {
            return Err(SourceError::TotalBytesLimitExceeded {
                limit: context.limits.max_total_bytes,
            });
        }
        let output = run_git(
            root,
            &["cat-file", "blob", &entry.object_id],
            "read pinned baseline blob",
        )?;
        if output.stdout.len() as u64 != size {
            return Err(SourceError::ContentChanged);
        }
        metrics.total_bytes = metrics.total_bytes.saturating_add(size);
        if is_binary(&output.stdout) {
            diagnostics.push("binary_file_skipped", Some(path));
            metrics.skipped = metrics.skipped.saturating_add(1);
            continue;
        }
        files.push(SourceFileDescriptor {
            path,
            content_identity: sha256_digest(&output.stdout),
            byte_len: size,
            file_mode: mode,
        });
        metrics.included = metrics.included.saturating_add(1);
    }

    metrics.directories = directories.len() as u64 + 1;
    metrics.suppressed_diagnostics = diagnostics.suppressed;
    let source_manifest_digest =
        canonical_source_manifest_digest(&files, &context.source_policy_digest);
    let revision = SourceRevision {
        id: revision_id(
            &context.repository,
            SourceKind::TaskBaseline,
            Some(&tree),
            &source_manifest_digest,
            &context.analysis_config_digest,
            false,
            false,
        ),
        repository: context.repository.clone(),
        source_kind: SourceKind::TaskBaseline,
        base_revision: Some(tree),
        manifest_digest: source_manifest_digest,
        analysis_config_digest: context.analysis_config_digest.clone(),
        dirty: false,
        includes_untracked: false,
    };
    Ok(SourceManifest {
        revision,
        extractor_set_digest: context.extractor_set_digest.clone(),
        files,
        diagnostics: diagnostics.diagnostics,
        metrics,
    })
}

fn tree_blob_size(root: &Path, object_id: &str) -> Result<u64, SourceError> {
    let output = run_git(
        root,
        &["cat-file", "-s", object_id],
        "inspect pinned baseline blob",
    )?;
    std::str::from_utf8(trim_ascii(&output.stdout))
        .ok()
        .and_then(|size| size.parse().ok())
        .ok_or(SourceError::GitCommand {
            operation: "parse pinned baseline blob size",
        })
}

fn record_tree_parent_directories(
    path: &RepoPath,
    directories: &mut BTreeSet<String>,
    max_directories: u64,
) -> Result<(), SourceError> {
    let mut components = path.as_str().split('/').peekable();
    let mut parent = String::new();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        if !parent.is_empty() {
            parent.push('/');
        }
        parent.push_str(component);
        directories.insert(parent.clone());
        if directories.len() as u64 >= max_directories {
            return Err(SourceError::DirectoryLimitExceeded {
                limit: max_directories,
            });
        }
    }
    Ok(())
}

pub(super) fn read_tree_descriptor_verified(
    root: &Path,
    tree: &Digest,
    file: &SourceFileDescriptor,
) -> Result<SourceContent, SourceError> {
    validate_git_tree_digest(tree)?;
    let root = canonical_root(root)?;
    ensure_worktree_root(&root)?;
    verify_tree(&root, tree)?;
    let entries = baseline_entries(&root, tree)?;
    let object_id = entries
        .get(&file.path)
        .map(|entry| entry.object_id.as_str())
        .ok_or(SourceError::FileNotInManifest)?;
    let output = run_git(
        &root,
        &["cat-file", "blob", object_id],
        "read frozen submitted blob",
    )?;
    if output.stdout.len() as u64 != file.byte_len
        || sha256_digest(&output.stdout) != file.content_identity
    {
        return Err(SourceError::ContentChanged);
    }
    Ok(SourceContent {
        bytes: output.stdout,
    })
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
    let filter_path = format!("--path={}", path.as_str());
    let output = run_git(
        root,
        &["hash-object", &filter_path, "--", path.as_str()],
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
#[path = "worktree_tests.rs"]
mod tests;
