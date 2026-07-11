//! Storage-independent extension points for later index and query phases.

use super::domain::{
    BuildId, GraphBuild, GraphDiagnostic, GraphEdge, GraphNode, GraphSnapshot, RepoPath,
    RepositoryRef, SnapshotId, SourceRevision,
};
use super::{
    query::{
        ContentRequest, ContentResponse, ContextRequest, ContextResponse, NeighborhoodRequest,
        NeighborhoodResponse, QueryError, SearchRequest, SearchResponse, StatusRequest,
        StatusResponse,
    },
    store::{BuildFailure, PublicationOutcome, PublishRequest, PublishedView},
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

#[derive(Debug, Clone, Copy)]
pub struct GraphLifecycleEvent<'a> {
    pub build_id: &'a BuildId,
    pub event_type: &'a str,
    pub diagnostic_count: usize,
}
