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
            canonical_source_manifest_digest(&files, &context.source_policy_digest)?;
        let source_manifest_digest = composed_manifest_digest(
            &effective_manifest_digest,
            &overlay.manifest().manifest_digest,
        )?;
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
            )?,
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
) -> Result<Digest, SourceError> {
    Ok(sha256_digest(&serde_json::to_vec(
        &ComposedManifestIdentity {
            version: 1,
            effective_manifest_digest,
            overlay_manifest_digest,
        },
    )?))
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
        let manifest_digest = overlay_manifest_digest(&workspace, &context, &changes)?;
        let revision_id = overlay_revision_id(&workspace, &manifest_digest)?;
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
) -> Result<Digest, SourceError> {
    let canonical = CanonicalOverlayManifest {
        version: 1,
        repository: &workspace.repository,
        baseline_revision: &workspace.baseline_revision,
        analysis_config_digest: &context.analysis_config_digest,
        source_policy_digest: &context.source_policy_digest,
        extractor_set_digest: &context.extractor_set_digest,
        changes,
    };
    Ok(sha256_digest(&serde_json::to_vec(&canonical)?))
}

#[derive(Serialize)]
struct CanonicalOverlayRevision<'a> {
    version: u32,
    task_view_id: &'a crate::repository_graph::domain::TaskViewId,
    manifest_digest: &'a Digest,
}

fn overlay_revision_id(
    workspace: &WorkspaceRef,
    manifest_digest: &Digest,
) -> Result<OverlayRevisionId, SourceError> {
    let canonical = CanonicalOverlayRevision {
        version: 1,
        task_view_id: &workspace.task_view_id,
        manifest_digest,
    };
    let digest = sha256_digest(&serde_json::to_vec(&canonical)?);
    Ok(OverlayRevisionId::from_content_digest(digest.value()))
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

mod git;
pub use git::*;

#[cfg(test)]
#[path = "worktree_tests.rs"]
mod tests;
