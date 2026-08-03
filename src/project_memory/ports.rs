//! Storage-independent project-memory extension points.

use std::fmt;

use chrono::{DateTime, Utc};

use super::{
    diagnostics::{MemoryDiagnostic, MemoryDiagnosticCode, MemoryLifecycleEvent},
    domain::{
        AuthorizedSourceDescriptor, AuthorizedSourceManifest, MemoryBuild, MemoryBuildId,
        MemoryCommit, MemoryExtractorIdentity, MemoryFragment, MemoryFragmentCacheKey,
        MemoryPublicationOutcome, MemoryPublishRequest, MemoryRevision, MemoryRevisionId,
        MemorySourceCategory, MemoryViewName, ProjectRef, PublishedMemoryRevision,
    },
    federation::{
        FederatedContextRequest, FederatedContextResponse, FederatedSearchRequest,
        FederatedSearchResponse,
    },
    query::{
        MemoryContentRequest, MemoryContentResponse, MemoryContextRequest, MemoryContextResponse,
        MemoryQueryError, MemorySearchRequest, MemorySearchResponse, MemoryStatusRequest,
        MemoryStatusResponse,
    },
};

#[derive(Clone, PartialEq, Eq)]
pub struct MemorySourceContent {
    pub bytes: Vec<u8>,
}

impl fmt::Debug for MemorySourceContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemorySourceContent")
            .field("byte_len", &self.bytes.len())
            .finish()
    }
}

/// Enumerates only policy-authorized sources and verifies every content read.
pub trait MemorySource {
    type Error;

    fn manifest(&self) -> Result<AuthorizedSourceManifest, Self::Error>;
    fn read_verified(
        &self,
        source: &AuthorizedSourceDescriptor,
    ) -> Result<MemorySourceContent, Self::Error>;
    fn revalidate(&self, manifest: &AuthorizedSourceManifest) -> Result<(), Self::Error>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryExtractionContext {
    pub project: ProjectRef,
    pub revision_id: MemoryRevisionId,
    pub build_id: MemoryBuildId,
    pub indexed_at: DateTime<Utc>,
    pub max_entities_per_source: u64,
    pub max_relationships_per_source: u64,
    pub max_parser_duration_ms: u64,
    pub max_diagnostics: u64,
}

#[derive(Clone, Copy)]
pub struct MemoryExtractionInput<'a> {
    pub context: &'a MemoryExtractionContext,
    pub source: &'a AuthorizedSourceDescriptor,
    pub content: &'a [u8],
}

impl fmt::Debug for MemoryExtractionInput<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemoryExtractionInput")
            .field("context", self.context)
            .field("source", self.source)
            .field("content_bytes", &self.content.len())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryExtractionFailure {
    pub extractor: MemoryExtractorIdentity,
    pub code: MemoryDiagnosticCode,
}

pub trait MemoryExtractor: Send + Sync {
    fn identity(&self) -> MemoryExtractorIdentity;
    fn supports(&self, category: MemorySourceCategory) -> bool;
    fn extract(
        &self,
        input: MemoryExtractionInput<'_>,
    ) -> Result<MemoryFragment, MemoryExtractionFailure>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryBuildFailure {
    pub code: MemoryDiagnosticCode,
}

pub trait MemoryStore {
    type Error;

    fn start_build(&mut self, build: &MemoryBuild) -> Result<(), Self::Error>;
    fn fail_build(
        &mut self,
        build_id: &MemoryBuildId,
        failure: &MemoryBuildFailure,
    ) -> Result<(), Self::Error>;
    fn complete_build(&mut self, commit: &MemoryCommit) -> Result<(), Self::Error>;
    fn publish(
        &mut self,
        request: &MemoryPublishRequest,
    ) -> Result<MemoryPublicationOutcome, Self::Error>;
    fn supersede_build(&mut self, build_id: &MemoryBuildId) -> Result<(), Self::Error>;
    fn build(&self, build_id: &MemoryBuildId) -> Result<Option<MemoryBuild>, Self::Error>;
    fn revision(
        &self,
        revision_id: &MemoryRevisionId,
    ) -> Result<Option<MemoryRevision>, Self::Error>;
    fn published_view(
        &self,
        project: &ProjectRef,
        view_name: &MemoryViewName,
    ) -> Result<Option<PublishedMemoryRevision>, Self::Error>;
    fn load_cached_fragment(
        &self,
        key: &MemoryFragmentCacheKey,
    ) -> Result<Option<MemoryFragment>, Self::Error>;
    fn diagnostics_for_build(
        &self,
        build_id: &MemoryBuildId,
    ) -> Result<Vec<MemoryDiagnostic>, Self::Error>;
}

pub trait MemoryQuery {
    fn status(
        &self,
        request: MemoryStatusRequest,
    ) -> Result<MemoryStatusResponse, MemoryQueryError>;
    fn search(
        &self,
        request: MemorySearchRequest,
    ) -> Result<MemorySearchResponse, MemoryQueryError>;
    fn context(
        &self,
        request: MemoryContextRequest,
    ) -> Result<MemoryContextResponse, MemoryQueryError>;
}

/// Backend-neutral project-memory capability.
pub trait ProjectMemory: MemoryQuery {}

impl<T> ProjectMemory for T where T: MemoryQuery + ?Sized {}

pub trait MemoryContent {
    fn content(
        &self,
        request: MemoryContentRequest,
    ) -> Result<MemoryContentResponse, MemoryQueryError>;
}

pub trait ContextService {
    fn search(
        &self,
        request: FederatedSearchRequest,
    ) -> Result<FederatedSearchResponse, MemoryQueryError>;
    fn context(
        &self,
        request: FederatedContextRequest,
    ) -> Result<FederatedContextResponse, MemoryQueryError>;
}

pub trait MemoryEventSink {
    type Error;

    fn emit(&self, event: MemoryLifecycleEvent<'_>) -> Result<(), Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extraction_debug_output_redacts_source_content() {
        let content = MemorySourceContent {
            bytes: b"approved but private body".to_vec(),
        };
        let debug = format!("{content:?}");
        assert!(debug.contains("byte_len"));
        assert!(!debug.contains("approved"));
        assert!(!debug.contains("private"));
    }
}
