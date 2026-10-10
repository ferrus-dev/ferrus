//! Optional local sidecar ports, independent of orchestration identity.

use super::{
    ports::MemoryQuery,
    query::{MemoryQueryError, MemoryStatusRequest, MemoryStatusResponse},
    query_sqlite::SqliteMemoryQuery,
};
use crate::repository_graph::{ports::GraphQuery, query_sqlite::SqliteGraphQuery};

pub struct OptionalGraphQuery<'a> {
    query: Option<SqliteGraphQuery<'a>>,
}

impl<'a> OptionalGraphQuery<'a> {
    pub fn new(
        sidecar: Option<&'a crate::repository_graph::sqlite::Sidecar>,
        limits: crate::repository_graph::config::QueryLimitsConfig,
        freshness: Option<crate::repository_graph::query_sqlite::FreshnessComparison>,
    ) -> Self {
        Self {
            query: sidecar.map(|sidecar| SqliteGraphQuery::new(sidecar, limits, freshness)),
        }
    }
}

impl GraphQuery for OptionalGraphQuery<'_> {
    fn status(
        &self,
        request: &crate::repository_graph::query::StatusRequest,
    ) -> std::result::Result<
        crate::repository_graph::query::StatusResponse,
        crate::repository_graph::query::QueryError,
    > {
        self.query
            .as_ref()
            .ok_or_else(graph_unavailable)?
            .status(request)
    }
    fn search(
        &self,
        request: &crate::repository_graph::query::SearchRequest,
    ) -> std::result::Result<
        crate::repository_graph::query::SearchResponse,
        crate::repository_graph::query::QueryError,
    > {
        self.query
            .as_ref()
            .ok_or_else(graph_unavailable)?
            .search(request)
    }
    fn show(
        &self,
        request: &crate::repository_graph::query::ShowRequest,
    ) -> std::result::Result<
        crate::repository_graph::query::ShowResponse,
        crate::repository_graph::query::QueryError,
    > {
        self.query
            .as_ref()
            .ok_or_else(graph_unavailable)?
            .show(request)
    }
    fn neighborhood(
        &self,
        request: &crate::repository_graph::query::NeighborhoodRequest,
    ) -> std::result::Result<
        crate::repository_graph::query::NeighborhoodResponse,
        crate::repository_graph::query::QueryError,
    > {
        self.query
            .as_ref()
            .ok_or_else(graph_unavailable)?
            .neighborhood(request)
    }
    fn context(
        &self,
        request: &crate::repository_graph::query::ContextRequest,
    ) -> std::result::Result<
        crate::repository_graph::query::ContextResponse,
        crate::repository_graph::query::QueryError,
    > {
        self.query
            .as_ref()
            .ok_or_else(graph_unavailable)?
            .context(request)
    }
}

fn graph_unavailable() -> crate::repository_graph::query::QueryError {
    crate::repository_graph::query::QueryError {
        wire_version: crate::repository_graph::QUERY_WIRE_VERSION,
        code: crate::repository_graph::query::QueryErrorCode::NotBuilt,
        message: "repository graph is not built".to_string(),
        retryable: true,
        recommended_action: Some(crate::repository_graph::query::RetrievalAction::Index),
        details: Default::default(),
    }
}

pub struct OptionalMemoryBackend<'a> {
    pub query: Option<SqliteMemoryQuery<'a>>,
    pub sidecar: Option<&'a crate::project_memory::sqlite::MemorySidecar>,
}

impl MemoryQuery for OptionalMemoryBackend<'_> {
    fn status(
        &self,
        request: MemoryStatusRequest,
    ) -> std::result::Result<MemoryStatusResponse, MemoryQueryError> {
        self.query
            .as_ref()
            .ok_or(MemoryQueryError::Unavailable)?
            .status(request)
    }
    fn search(
        &self,
        request: crate::project_memory::query::MemorySearchRequest,
    ) -> std::result::Result<crate::project_memory::query::MemorySearchResponse, MemoryQueryError>
    {
        self.query
            .as_ref()
            .ok_or(MemoryQueryError::Unavailable)?
            .search(request)
    }
    fn context(
        &self,
        request: crate::project_memory::query::MemoryContextRequest,
    ) -> std::result::Result<crate::project_memory::query::MemoryContextResponse, MemoryQueryError>
    {
        self.query
            .as_ref()
            .ok_or(MemoryQueryError::Unavailable)?
            .context(request)
    }
}

impl crate::project_memory::ports::MemoryLinkStore for OptionalMemoryBackend<'_> {
    type Error = crate::project_memory::sqlite::MemoryStoreError;

    fn repository_link_set(
        &self,
        id: &crate::project_memory::domain::MemoryRepositoryLinkSetId,
    ) -> Result<Option<crate::project_memory::domain::MemoryRepositoryLinkSet>, Self::Error> {
        self.sidecar
            .ok_or(crate::project_memory::sqlite::MemoryStoreError::RequiresRebuild)?
            .repository_link_set(id)
    }
    fn repository_link_set_for_snapshot(
        &self,
        revision: &crate::project_memory::domain::MemoryRevisionId,
        repository: &crate::repository_graph::domain::RepositoryRef,
        snapshot: Option<&crate::repository_graph::domain::SnapshotId>,
    ) -> Result<Option<crate::project_memory::domain::MemoryRepositoryLinkSet>, Self::Error> {
        self.sidecar
            .ok_or(crate::project_memory::sqlite::MemoryStoreError::RequiresRebuild)?
            .repository_link_set_for_snapshot(revision, repository, snapshot)
    }
    fn latest_repository_link_set(
        &self,
        revision: &crate::project_memory::domain::MemoryRevisionId,
        repository: &crate::repository_graph::domain::RepositoryRef,
    ) -> Result<Option<crate::project_memory::domain::MemoryRepositoryLinkSet>, Self::Error> {
        self.sidecar
            .ok_or(crate::project_memory::sqlite::MemoryStoreError::RequiresRebuild)?
            .latest_repository_link_set(revision, repository)
    }
    fn latest_compatible_repository_link_set(
        &self,
        revision: &crate::project_memory::domain::MemoryRevision,
        repository: &crate::repository_graph::domain::RepositoryRef,
    ) -> Result<Option<crate::project_memory::domain::MemoryRepositoryLinkSet>, Self::Error> {
        self.sidecar
            .ok_or(crate::project_memory::sqlite::MemoryStoreError::RequiresRebuild)?
            .latest_compatible_repository_link_set(revision, repository)
    }
    fn repository_links(
        &self,
        id: &crate::project_memory::domain::MemoryRepositoryLinkSetId,
    ) -> Result<Vec<crate::project_memory::domain::MemoryRelationship>, Self::Error> {
        self.sidecar
            .ok_or(crate::project_memory::sqlite::MemoryStoreError::RequiresRebuild)?
            .repository_links(id)
    }
    fn repository_link_diagnostics(
        &self,
        id: &crate::project_memory::domain::MemoryRepositoryLinkSetId,
    ) -> Result<Vec<crate::project_memory::diagnostics::MemoryDiagnostic>, Self::Error> {
        self.sidecar
            .ok_or(crate::project_memory::sqlite::MemoryStoreError::RequiresRebuild)?
            .repository_link_diagnostics(id)
    }
    fn bounded_repository_links(
        &self,
        id: &crate::project_memory::domain::MemoryRepositoryLinkSetId,
        max_relationships: u32,
        max_diagnostics: u32,
        max_duration_ms: u64,
    ) -> Result<crate::project_memory::ports::BoundedMemoryLinks, Self::Error> {
        self.sidecar
            .ok_or(crate::project_memory::sqlite::MemoryStoreError::RequiresRebuild)?
            .bounded_repository_links(id, max_relationships, max_diagnostics, max_duration_ms)
    }
}
