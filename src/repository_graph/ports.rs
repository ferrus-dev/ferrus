//! Storage-independent extension points for later index and query phases.

use super::domain::{
    BuildId, GraphDiagnostic, GraphEdge, GraphNode, GraphQueryRequest, GraphSnapshot, RepoPath,
    RepositoryRef, SnapshotId, SourceRevision,
};

#[derive(Debug, Clone)]
pub struct SourceFileDescriptor {
    pub path: RepoPath,
    pub content_identity: super::domain::Digest,
    pub byte_len: u64,
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
    fn revision(&self) -> Result<SourceRevision, Self::Error>;
    fn files(&self) -> Result<Vec<SourceFileDescriptor>, Self::Error>;
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

    fn begin_build(&mut self, revision: &SourceRevision) -> Result<BuildId, Self::Error>;
    fn snapshot(&self, id: &SnapshotId) -> Result<Option<GraphSnapshot>, Self::Error>;
}

pub trait GraphQuery {
    type Error;
    type Response;

    fn query(&self, request: &GraphQueryRequest) -> Result<Self::Response, Self::Error>;
}

pub trait SnapshotContent {
    type Error;

    fn read_verified(
        &self,
        snapshot_id: &SnapshotId,
        file: &SourceFileDescriptor,
    ) -> Result<Vec<u8>, Self::Error>;
}

pub trait EventSink {
    type Error;

    fn emit(&self, event: GraphLifecycleEvent<'_>) -> Result<(), Self::Error>;
}

#[derive(Debug, Clone, Copy)]
pub struct GraphLifecycleEvent<'a> {
    pub build_id: &'a BuildId,
    pub event_type: &'a str,
    pub diagnostic_count: usize,
}
