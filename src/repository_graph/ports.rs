//! Storage-independent extension points for later index and query phases.

use serde::{Deserialize, Serialize};

use super::domain::{
    BuildId, DiagnosticCode, Digest, GraphBuild, GraphDiagnostic, GraphEdge, GraphNode,
    GraphSnapshot, OverlayRevisionId, RepoPath, RepositoryRef, SnapshotId, SourceRevision,
    WorkspaceRef,
};
use super::{
    diagnostics::GraphLifecycleEvent,
    query::{
        ContentRequest, ContentResponse, ContextRequest, ContextResponse, NeighborhoodRequest,
        NeighborhoodResponse, QueryError, SearchRequest, SearchResponse, ShowRequest, ShowResponse,
        StatusRequest, StatusResponse,
    },
    store::{BuildFailure, PublicationOutcome, PublishRequest, PublishedView},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceFileMode {
    Regular,
    Executable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFileDescriptor {
    pub path: RepoPath,
    pub content_identity: super::domain::Digest,
    pub byte_len: u64,
    pub file_mode: SourceFileMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDiagnostic {
    pub code: DiagnosticCode,
    pub path: Option<RepoPath>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDiscoveryMetrics {
    pub candidates: u64,
    pub directories: u64,
    pub included: u64,
    pub skipped: u64,
    pub total_bytes: u64,
    pub suppressed_diagnostics: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceManifest {
    pub revision: SourceRevision,
    pub extractor_set_digest: Digest,
    pub files: Vec<SourceFileDescriptor>,
    pub diagnostics: Vec<SourceDiagnostic>,
    pub metrics: SourceDiscoveryMetrics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayChangeKind {
    Added,
    Modified,
    Deleted,
}

/// One path-level worktree delta after applying the repository graph source
/// policy. Renames are represented as a deletion plus an addition whose
/// `renamed_from` points at the deleted path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayFileChange {
    pub path: RepoPath,
    pub kind: OverlayChangeKind,
    pub renamed_from: Option<RepoPath>,
    /// Present only when the current path is indexable under the effective
    /// source, sensitive-path, binary, size, and file-kind policy.
    pub current_file: Option<SourceFileDescriptor>,
}

/// Changed-file manifest for a task worktree relative to its pinned Git tree.
/// It contains no source bodies or machine-local paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceOverlayManifest {
    pub workspace: WorkspaceRef,
    pub revision_id: OverlayRevisionId,
    pub manifest_digest: Digest,
    pub analysis_config_digest: Digest,
    pub source_policy_digest: Digest,
    pub extractor_set_digest: Digest,
    pub changes: Vec<OverlayFileChange>,
    pub diagnostics: Vec<SourceDiagnostic>,
    pub metrics: SourceDiscoveryMetrics,
}

#[derive(Clone, PartialEq, Eq)]
pub struct SourceContent {
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for SourceContent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SourceContent")
            .field("byte_len", &self.bytes.len())
            .finish()
    }
}

/// Immutable identifiers and hard budgets supplied to every file extractor.
///
/// Keeping this context independent from SQLite lets the same deterministic
/// extractor run locally today and inside a stateless worker later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractionContext {
    pub snapshot_id: SnapshotId,
    pub build_id: BuildId,
    pub repository: RepositoryRef,
    pub max_facts_per_file: u64,
    pub max_parser_duration_ms: u64,
    pub max_diagnostics: u64,
}

/// One immutable, content-verified file presented to an extractor.
#[derive(Clone, Copy)]
pub struct FileExtractionInput<'a> {
    pub context: &'a ExtractionContext,
    pub file: &'a SourceFileDescriptor,
    pub content: &'a [u8],
}

impl std::fmt::Debug for FileExtractionInput<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileExtractionInput")
            .field("context", self.context)
            .field("file", self.file)
            .field("content_bytes", &self.content.len())
            .finish()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphFragment {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub diagnostics: Vec<GraphDiagnostic>,
}

/// Privacy-safe extractor failure used by the object-safe coordinator boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractorFailure {
    pub extractor: super::domain::ExtractorIdentity,
    pub code: DiagnosticCode,
}

impl std::fmt::Display for ExtractorFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "repository graph extractor {} failed with {}",
            self.extractor.id.as_str(),
            self.code.as_str()
        )
    }
}

impl std::error::Error for ExtractorFailure {}

/// Object-safe view over heterogeneous extractor implementations.
pub trait DynExtractor: Send + Sync {
    fn identity(&self) -> super::domain::ExtractorIdentity;
    fn supports(&self, file: &SourceFileDescriptor) -> bool;
    fn extract(&self, input: FileExtractionInput<'_>) -> Result<GraphFragment, ExtractorFailure>;
}

impl<T> DynExtractor for T
where
    T: Extractor,
{
    fn identity(&self) -> super::domain::ExtractorIdentity {
        Extractor::identity(self)
    }

    fn supports(&self, file: &SourceFileDescriptor) -> bool {
        Extractor::supports(self, file)
    }

    fn extract(&self, input: FileExtractionInput<'_>) -> Result<GraphFragment, ExtractorFailure> {
        Extractor::extract(self, input).map_err(|_| ExtractorFailure {
            extractor: Extractor::identity(self),
            code: DiagnosticCode::new("extractor.failed")
                .expect("static extractor failure code is canonical"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FragmentCacheKey {
    pub repository: RepositoryRef,
    pub path: RepoPath,
    pub content_identity: Digest,
    pub byte_len: u64,
    pub file_mode: SourceFileMode,
    pub analysis_config_digest: Digest,
    pub extractor: super::domain::ExtractorIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedFragment {
    pub key: FragmentCacheKey,
    pub fragment: GraphFragment,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexBuildMetrics {
    pub discovered_files: u64,
    pub reused_files: u64,
    pub parsed_files: u64,
    pub skipped_files: u64,
    pub failed_files: u64,
    pub processed_bytes: u64,
    pub nodes: u64,
    pub edges: u64,
    pub diagnostics: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone)]
pub struct IndexCommit {
    pub snapshot: GraphSnapshot,
    pub files: Vec<SourceFileDescriptor>,
    pub graph: GraphFragment,
    pub cache_writes: Vec<CachedFragment>,
    pub metrics: IndexBuildMetrics,
}

/// Hard limits for one deterministic cross-file resolution pass.
///
/// The resolver may retain the already-bounded extractor facts regardless of
/// this budget. `max_relationships` limits only relationships that it resolves
/// or adds, while the other limits bound diagnostics and wall-clock work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolutionBudget {
    pub max_relationships: u64,
    pub max_duration_ms: u64,
    pub max_diagnostics: u64,
}

/// One complete building-snapshot fragment resolved against exactly one
/// immutable source manifest.
pub struct CrossFileResolutionInput<'a> {
    pub context: &'a ExtractionContext,
    pub manifest: &'a SourceManifest,
    pub fragment: GraphFragment,
    pub budget: ResolutionBudget,
}

impl std::fmt::Debug for CrossFileResolutionInput<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CrossFileResolutionInput")
            .field("context", self.context)
            .field("manifest_revision", &self.manifest.revision.id)
            .field("nodes", &self.fragment.nodes.len())
            .field("edges", &self.fragment.edges.len())
            .field("diagnostics", &self.fragment.diagnostics.len())
            .field("budget", &self.budget)
            .finish()
    }
}

pub trait RepositorySource {
    type Error;

    fn repository(&self) -> &RepositoryRef;
    fn manifest(&self) -> &SourceManifest;
    fn read_verified(&self, file: &SourceFileDescriptor) -> Result<SourceContent, Self::Error>;
    /// Re-discovers the mutable source and compares all source-identity inputs.
    fn revalidate(&self) -> Result<bool, Self::Error>;

    fn revision(&self) -> SourceRevision {
        self.manifest().revision.clone()
    }

    fn files(&self) -> Vec<SourceFileDescriptor> {
        self.manifest().files.clone()
    }
}

pub trait Extractor: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    fn identity(&self) -> super::domain::ExtractorIdentity;
    fn supports(&self, file: &SourceFileDescriptor) -> bool;
    fn extract(&self, input: FileExtractionInput<'_>) -> Result<GraphFragment, Self::Error>;
}

pub trait CrossFileResolver: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    fn identity(&self) -> super::domain::ExtractorIdentity;
    fn resolve(&self, input: CrossFileResolutionInput<'_>) -> Result<GraphFragment, Self::Error>;
}

pub trait GraphStore {
    type Error;

    fn start_build(&mut self, build: &GraphBuild) -> Result<(), Self::Error>;
    fn fail_build(&mut self, failure: &BuildFailure) -> Result<GraphBuild, Self::Error>;
    fn complete_build(&mut self, snapshot: &GraphSnapshot) -> Result<GraphSnapshot, Self::Error>;
    fn publish(&mut self, request: &PublishRequest) -> Result<PublicationOutcome, Self::Error>;
    fn supersede_build(&mut self, build_id: &BuildId) -> Result<GraphBuild, Self::Error>;
    fn build(&self, id: &BuildId) -> Result<Option<GraphBuild>, Self::Error>;
    fn snapshot(&self, id: &SnapshotId) -> Result<Option<GraphSnapshot>, Self::Error>;
    fn published_view(
        &self,
        repository: &RepositoryRef,
        name: &super::domain::PublishedViewName,
    ) -> Result<Option<PublishedView>, Self::Error>;
}

/// Persistence operations needed by the indexing coordinator. The DTOs remain
/// backend-neutral so a remote implementation can preserve the same workflow.
pub trait IndexStore: GraphStore {
    fn load_cached_fragment(
        &mut self,
        key: &FragmentCacheKey,
    ) -> Result<Option<GraphFragment>, Self::Error>;
    fn complete_index(&mut self, commit: &IndexCommit) -> Result<GraphSnapshot, Self::Error>;
    fn record_build_metrics(
        &mut self,
        build_id: &BuildId,
        metrics: &IndexBuildMetrics,
    ) -> Result<(), Self::Error>;
}

pub trait GraphQuery {
    fn status(&self, request: &StatusRequest) -> Result<StatusResponse, QueryError>;
    fn search(&self, request: &SearchRequest) -> Result<SearchResponse, QueryError>;
    fn show(&self, request: &ShowRequest) -> Result<ShowResponse, QueryError>;
    fn neighborhood(
        &self,
        request: &NeighborhoodRequest,
    ) -> Result<NeighborhoodResponse, QueryError>;
    fn context(&self, request: &ContextRequest) -> Result<ContextResponse, QueryError>;
}

pub trait SnapshotContent {
    fn read_verified(&self, request: &ContentRequest) -> Result<ContentResponse, QueryError>;
}

pub trait EventSink {
    type Error;

    fn emit(&self, event: GraphLifecycleEvent<'_>) -> Result<(), Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository_graph::domain::{BuildId, RepositoryId, RepositoryNamespace, SnapshotId};

    #[test]
    fn extraction_input_debug_output_redacts_verified_content() {
        let context = ExtractionContext {
            snapshot_id: SnapshotId::new("snapshot").unwrap(),
            build_id: BuildId::new("build").unwrap(),
            repository: RepositoryRef {
                namespace: RepositoryNamespace::new("local").unwrap(),
                repository_id: RepositoryId::new("root").unwrap(),
            },
            max_facts_per_file: 10,
            max_parser_duration_ms: 10,
            max_diagnostics: 10,
        };
        let file = SourceFileDescriptor {
            path: RepoPath::new("src/lib.rs").unwrap(),
            content_identity: Digest::new("sha256", "00").unwrap(),
            byte_len: 16,
            file_mode: SourceFileMode::Regular,
        };
        let input = FileExtractionInput {
            context: &context,
            file: &file,
            content: b"TOP_SECRET_VALUE",
        };

        let debug = format!("{input:?}");
        assert!(!debug.contains("TOP_SECRET_VALUE"));
        assert!(debug.contains("content_bytes: 16"));

        let content = SourceContent {
            bytes: b"TOP_SECRET_VALUE".to_vec(),
        };
        let debug = format!("{content:?}");
        assert!(!debug.contains("TOP_SECRET_VALUE"));
        assert!(debug.contains("byte_len: 16"));
    }
}
