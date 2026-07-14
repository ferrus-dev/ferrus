//! Storage-independent extension points for later index and query phases.

use serde::{Deserialize, Serialize};

use super::domain::{
    BuildId, DiagnosticCode, Digest, GraphBuild, GraphDiagnostic, GraphEdge, GraphNode,
    GraphSnapshot, RepoPath, RepositoryRef, SnapshotId, SourceRevision,
};
use super::{
    diagnostics::GraphLifecycleEvent,
    query::{
        ContentRequest, ContentResponse, ContextRequest, ContextResponse, NeighborhoodRequest,
        NeighborhoodResponse, QueryError, SearchRequest, SearchResponse, StatusRequest,
        StatusResponse,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceContent {
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct GraphFragment {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub diagnostics: Vec<GraphDiagnostic>,
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

pub trait Extractor {
    type Error;

    fn identity(&self) -> super::domain::ExtractorIdentity;
    fn extract(&self, file: &SourceFileDescriptor) -> Result<GraphFragment, Self::Error>;
}

pub trait CrossFileResolver {
    type Error;

    fn resolve(&self, fragment: GraphFragment) -> Result<GraphFragment, Self::Error>;
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

pub trait GraphQuery {
    fn status(&self, request: &StatusRequest) -> Result<StatusResponse, QueryError>;
    fn search(&self, request: &SearchRequest) -> Result<SearchResponse, QueryError>;
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
