//! Project-local adaptation for project-memory and federated retrieval.

use std::{
    num::{NonZeroU32, NonZeroU64},
    path::PathBuf,
};

use anyhow::{Context, Result as AnyResult};

use crate::{
    project,
    project_memory::{
        FEDERATION_WIRE_VERSION, MEMORY_QUERY_WIRE_VERSION,
        domain::{MemorySourceCategory, MemoryViewName, ProjectId, ProjectNamespace, ProjectRef},
        federation::{
            ContextDomain, FederatedContextRequest, FederatedContextResponse, FederatedScope,
            FederatedSearchRequest, FederatedSearchResponse, FederatedTarget,
            RepositoryContextTarget,
        },
        federation_service::FederatedContextService,
        policy::MemoryPolicy,
        ports::{ContextService, MemoryQuery, MemorySource},
        query::{
            MemoryAvailability, MemoryFreshness, MemoryFreshnessComparison,
            MemoryFreshnessEnvelope, MemoryQueryBudget, MemoryQueryError, MemoryQueryScope,
            MemoryRetrievalAction, MemoryRevisionSelector, MemorySourcePolicyStatus,
            MemoryStatusData, MemoryStatusRequest, MemoryStatusResponse,
        },
        query_sqlite::{SqliteMemoryQuery, default_budget as default_memory_budget},
        source::LocalMemorySource,
        sqlite::{MEMORY_SIDECAR_FILE_NAME, OpenMemoryQuerySidecarResult, open_for_query_at},
    },
    repository_graph::{
        domain::QueryBudget,
        ports::GraphQuery,
        query_sqlite::SqliteGraphQuery,
        sqlite::{
            OpenQuerySidecarResult, SIDECAR_FILE_NAME, open_for_query_at as open_graph_for_query_at,
        },
    },
    repository_graph_runtime::LocalGraphContext,
};

pub(crate) const PROJECT_MEMORY_VIEW: &str = "project";

/// Refreshes derived memory after the archive transaction has committed.
/// Failure is deliberately isolated from the successful archive lifecycle.
pub(crate) async fn refresh_after_archive_best_effort() {
    let started = std::time::Instant::now();
    match crate::project_memory::index::index_current_project(
        crate::project_memory::index::MemoryIndexOptions::default(),
    )
    .await
    {
        Ok(outcome) => tracing::info!(
            target: "ferrus::project_memory::index",
            revision_id = outcome.revision.id.as_str(),
            build_id = outcome.build_id.as_str(),
            duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            discovered_sources = outcome.metrics.discovered_sources,
            reused_sources = outcome.metrics.reused_sources,
            extracted_sources = outcome.metrics.extracted_sources,
            entities = outcome.metrics.entities,
            relationships = outcome.metrics.relationships,
            diagnostics = outcome.metrics.diagnostics,
            trigger = "spec_archive",
            "project memory refreshed after spec archive"
        ),
        Err(error) => tracing::warn!(
            error_category = memory_index_error_category(&error),
            trigger = "spec_archive",
            "project memory refresh failed after successful spec archive; archive state is unchanged"
        ),
    }
}

pub(crate) struct LocalProjectContext {
    pub(crate) graph: LocalGraphContext,
    pub(crate) project: ProjectRef,
    data_dir: PathBuf,
    exact_memory_source: Option<LocalMemorySource>,
    compare_local_freshness: bool,
}

impl LocalProjectContext {
    pub(crate) async fn load_for_cli(require_graph: bool) -> AnyResult<Self> {
        Self::load(require_graph, None, true, true).await
    }

    #[cfg(test)]
    pub(crate) async fn load_unscoped_read_only() -> AnyResult<Self> {
        Self::load(false, None, false, false).await
    }

    pub(crate) async fn load_for_agent(
        agent_id: &str,
        include_memory_content: bool,
    ) -> AnyResult<Self> {
        Self::load(false, Some(agent_id), include_memory_content, false).await
    }

    async fn load(
        require_graph: bool,
        agent_id: Option<&str>,
        include_memory_content: bool,
        compare_local_freshness: bool,
    ) -> AnyResult<Self> {
        let graph = match agent_id {
            Some(agent_id) => LocalGraphContext::load_for_agent(require_graph, agent_id).await?,
            None => LocalGraphContext::load(require_graph).await?,
        };
        let project_id = project::current_project_id().await?;
        let data_dir = project::current_project_data_dir().await?;
        let project = ProjectRef {
            namespace: ProjectNamespace::new("local:ferrus")?,
            project_id: ProjectId::new(project_id)?,
        };
        let exact_memory_source = if include_memory_content {
            Some(LocalMemorySource::discover_current().await?)
        } else {
            None
        };
        Ok(Self {
            graph,
            project,
            data_dir,
            exact_memory_source,
            compare_local_freshness,
        })
    }

    pub(crate) fn default_budget(&self) -> AnyResult<MemoryQueryBudget> {
        default_memory_budget(&self.graph.config.query_limits).map_err(Into::into)
    }

    pub(crate) fn requested_budget(
        &self,
        max_results: Option<u32>,
        max_bytes: Option<u64>,
        max_snippet_bytes: Option<u64>,
        max_depth: Option<u32>,
        max_duration_ms: Option<u64>,
        max_diagnostics: Option<u32>,
    ) -> AnyResult<MemoryQueryBudget> {
        let defaults = self.default_budget()?;
        Ok(MemoryQueryBudget {
            max_results: NonZeroU32::new(max_results.unwrap_or(defaults.max_results.get()))
                .context("max_results must be greater than zero")?,
            max_bytes: NonZeroU64::new(max_bytes.unwrap_or(defaults.max_bytes.get()))
                .context("max_bytes must be greater than zero")?,
            max_snippet_bytes: NonZeroU64::new(
                max_snippet_bytes.unwrap_or(defaults.max_snippet_bytes.get()),
            )
            .context("max_snippet_bytes must be greater than zero")?,
            max_depth: NonZeroU32::new(max_depth.unwrap_or(defaults.max_depth.get()))
                .context("max_depth must be greater than zero")?,
            max_duration_ms: NonZeroU64::new(
                max_duration_ms.unwrap_or(defaults.max_duration_ms.get()),
            )
            .context("max_duration_ms must be greater than zero")?,
            max_diagnostics: NonZeroU32::new(
                max_diagnostics.unwrap_or(defaults.max_diagnostics.get()),
            )
            .context("max_diagnostics must be greater than zero")?,
        })
    }

    pub(crate) fn scope(
        &self,
        domain: ContextDomain,
        budget: MemoryQueryBudget,
    ) -> AnyResult<FederatedScope> {
        if matches!(domain, ContextDomain::Repository | ContextDomain::All)
            && !self.graph.config.enabled
        {
            anyhow::bail!(
                "repository graph is disabled; enable it before repository or combined retrieval"
            );
        }
        let memory = MemoryRevisionSelector::Published(
            MemoryViewName::new(PROJECT_MEMORY_VIEW).expect("static memory view is valid"),
        );
        let repository_scope = || -> AnyResult<RepositoryContextTarget> {
            let scope = self.graph.scope(repository_budget(&budget))?;
            Ok(RepositoryContextTarget {
                repository: scope.repository,
                snapshot: scope.snapshot,
            })
        };
        let target = match domain {
            ContextDomain::Repository => FederatedTarget::Repository {
                repository: repository_scope()?,
            },
            ContextDomain::Memory => FederatedTarget::Memory { memory },
            ContextDomain::All => FederatedTarget::All {
                repository: repository_scope()?,
                memory,
            },
        };
        Ok(FederatedScope {
            wire_version: FEDERATION_WIRE_VERSION,
            project: self.project.clone(),
            target,
            budget,
        })
    }

    pub(crate) fn memory_status(
        &self,
        budget: MemoryQueryBudget,
    ) -> AnyResult<MemoryStatusResponse> {
        let sidecar = match open_for_query_at(&self.data_dir.join(MEMORY_SIDECAR_FILE_NAME)) {
            Ok(OpenMemoryQuerySidecarResult::Ready(sidecar)) => sidecar,
            Ok(OpenMemoryQuerySidecarResult::Absent) => {
                return Ok(unavailable_status(
                    self.project.clone(),
                    MemoryAvailability::NotBuilt,
                    MemoryRetrievalAction::Build,
                ));
            }
            Ok(
                OpenMemoryQuerySidecarResult::NeedsMigration { .. }
                | OpenMemoryQuerySidecarResult::RequiresRebuild,
            )
            | Err(_) => {
                return Ok(unavailable_status(
                    self.project.clone(),
                    MemoryAvailability::Incompatible,
                    MemoryRetrievalAction::Rebuild,
                ));
            }
        };
        let query = self.memory_query(&sidecar);
        let mut scope = MemoryQueryScope::current(
            self.project.clone(),
            MemoryRevisionSelector::Published(
                MemoryViewName::new(PROJECT_MEMORY_VIEW).expect("static memory view is valid"),
            ),
            budget,
        );
        scope.freshness_comparison = self.memory_freshness_comparison()?;
        Ok(query.status(MemoryStatusRequest { scope })?)
    }

    pub(crate) fn search(
        &self,
        request: FederatedSearchRequest,
    ) -> Result<FederatedSearchResponse, MemoryQueryError> {
        let graph_sidecar = self.open_graph_query().map_err(runtime_error)?;
        let memory_sidecar = self.open_memory_query().map_err(runtime_error)?;
        if matches!(
            request.scope.target,
            FederatedTarget::Repository { .. } | FederatedTarget::All { .. }
        ) && graph_sidecar.is_none()
        {
            return Err(MemoryQueryError::Unavailable);
        }
        if matches!(
            request.scope.target,
            FederatedTarget::Memory { .. } | FederatedTarget::All { .. }
        ) && memory_sidecar.is_none()
        {
            return Err(MemoryQueryError::Unavailable);
        }
        let graph_query = OptionalGraphQuery::new(
            graph_sidecar.as_deref(),
            self.graph.config.query_limits.clone(),
            self.graph_freshness_comparison(),
        );
        let memory_query = memory_sidecar
            .as_deref()
            .map(|sidecar| self.memory_query(sidecar));
        let backend = OptionalMemoryBackend {
            query: memory_query,
            sidecar: memory_sidecar.as_deref(),
        };
        let service = FederatedContextService::new(
            &graph_query,
            &backend,
            &backend,
            self.graph.config.query_limits.clone(),
            self.memory_freshness_comparison().map_err(runtime_error)?,
        );
        let mut response = service.search(request)?;
        if let Some(repository) = response.repository.as_mut() {
            repository.task_view = self.graph.task_view_envelope();
        }
        Ok(response)
    }

    pub(crate) fn context(
        &self,
        request: FederatedContextRequest,
    ) -> Result<FederatedContextResponse, MemoryQueryError> {
        let graph_sidecar = self.open_graph_query().map_err(runtime_error)?;
        let memory_sidecar = self.open_memory_query().map_err(runtime_error)?;
        if matches!(
            request.scope.target,
            FederatedTarget::Repository { .. } | FederatedTarget::All { .. }
        ) && graph_sidecar.is_none()
        {
            return Err(MemoryQueryError::Unavailable);
        }
        if matches!(
            request.scope.target,
            FederatedTarget::Memory { .. } | FederatedTarget::All { .. }
        ) && memory_sidecar.is_none()
        {
            return Err(MemoryQueryError::Unavailable);
        }
        let graph_query = OptionalGraphQuery::new(
            graph_sidecar.as_deref(),
            self.graph.config.query_limits.clone(),
            self.graph_freshness_comparison(),
        );
        let memory_query = memory_sidecar
            .as_deref()
            .map(|sidecar| self.memory_query(sidecar));
        let backend = OptionalMemoryBackend {
            query: memory_query,
            sidecar: memory_sidecar.as_deref(),
        };
        let service = FederatedContextService::new(
            &graph_query,
            &backend,
            &backend,
            self.graph.config.query_limits.clone(),
            self.memory_freshness_comparison().map_err(runtime_error)?,
        );
        let mut response = service.context(request)?;
        if let Some(repository) = response.repository.as_mut() {
            repository.task_view = self.graph.task_view_envelope();
        }
        Ok(response)
    }

    fn memory_query<'a>(
        &'a self,
        sidecar: &'a crate::project_memory::sqlite::MemorySidecar,
    ) -> SqliteMemoryQuery<'a> {
        let query = SqliteMemoryQuery::new(sidecar, self.graph.config.query_limits.clone());
        match self.exact_memory_source.as_ref() {
            Some(source) => query.with_content(source),
            None => query,
        }
    }

    fn memory_freshness_comparison(&self) -> AnyResult<Option<MemoryFreshnessComparison>> {
        if !self.compare_local_freshness {
            return Ok(None);
        }
        self.exact_memory_source
            .as_ref()
            .map(|source| {
                source
                    .manifest()
                    .map(|manifest| MemoryFreshnessComparison::from_manifest(&manifest))
            })
            .transpose()
    }

    fn graph_freshness_comparison(
        &self,
    ) -> Option<crate::repository_graph::query_sqlite::FreshnessComparison> {
        if self.compare_local_freshness {
            self.graph.freshness_comparison().ok().flatten()
        } else {
            None
        }
    }

    fn open_memory_query(
        &self,
    ) -> AnyResult<Option<Box<crate::project_memory::sqlite::MemorySidecar>>> {
        match open_for_query_at(&self.data_dir.join(MEMORY_SIDECAR_FILE_NAME))? {
            OpenMemoryQuerySidecarResult::Ready(sidecar) => Ok(Some(sidecar)),
            OpenMemoryQuerySidecarResult::Absent => Ok(None),
            OpenMemoryQuerySidecarResult::NeedsMigration {
                found_schema_version,
            } => anyhow::bail!(
                "project-memory schema {found_schema_version} requires an explicit index migration or rebuild"
            ),
            OpenMemoryQuerySidecarResult::RequiresRebuild => {
                anyhow::bail!("project-memory sidecar is incompatible and requires rebuild")
            }
        }
    }

    fn open_graph_query(&self) -> AnyResult<Option<Box<crate::repository_graph::sqlite::Sidecar>>> {
        match open_graph_for_query_at(&self.data_dir.join(SIDECAR_FILE_NAME))? {
            OpenQuerySidecarResult::Ready(sidecar) => Ok(Some(Box::new(sidecar))),
            OpenQuerySidecarResult::Absent => Ok(None),
            OpenQuerySidecarResult::NeedsMigration {
                found_schema_version,
            } => anyhow::bail!("repository graph schema {found_schema_version} requires migration"),
            OpenQuerySidecarResult::RequiresRebuild(reason) => anyhow::bail!(
                "repository graph schema {} is incompatible with {}: {}",
                reason.found_schema_version,
                reason.supported_schema_version,
                reason.reason
            ),
        }
    }
}

fn repository_budget(budget: &MemoryQueryBudget) -> QueryBudget {
    QueryBudget::new(
        budget.max_results,
        budget.max_bytes,
        budget.max_depth,
        budget.max_duration_ms,
        budget.max_diagnostics,
    )
}

fn unavailable_status(
    project: ProjectRef,
    availability: MemoryAvailability,
    action: MemoryRetrievalAction,
) -> MemoryStatusResponse {
    let policy = MemoryPolicy::default();
    MemoryStatusResponse {
        wire_version: MEMORY_QUERY_WIRE_VERSION,
        project,
        revision_id: None,
        freshness: MemoryFreshnessEnvelope {
            freshness: MemoryFreshness::Unknown,
            compared_source_set_digest: None,
            reason_codes: vec![],
        },
        diagnostics: vec![],
        data: MemoryStatusData {
            availability,
            build_state: None,
            build_id: None,
            memory_model_version: None,
            statistics: None,
            recommended_action: Some(action),
            source_policy: MemorySourceCategory::ALL
                .into_iter()
                .filter_map(|category| {
                    policy
                        .category(category)
                        .copied()
                        .map(|policy| MemorySourcePolicyStatus { category, policy })
                })
                .collect(),
        },
    }
}

fn memory_index_error_category(
    error: &crate::project_memory::index::MemoryIndexError,
) -> &'static str {
    match error {
        crate::project_memory::index::MemoryIndexError::Source(_) => "source",
        crate::project_memory::index::MemoryIndexError::Store(_) => "store",
        crate::project_memory::index::MemoryIndexError::Identity(_) => "identity",
        crate::project_memory::index::MemoryIndexError::Links(_) => "links",
        crate::project_memory::index::MemoryIndexError::FactCollision => "fact_collision",
    }
}

fn runtime_error(_error: anyhow::Error) -> MemoryQueryError {
    tracing::warn!(
        error_category = "backend_unavailable",
        "project context backend is unavailable"
    );
    MemoryQueryError::Unavailable
}

struct OptionalGraphQuery<'a> {
    query: Option<SqliteGraphQuery<'a>>,
}

impl<'a> OptionalGraphQuery<'a> {
    fn new(
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

struct OptionalMemoryBackend<'a> {
    query: Option<SqliteMemoryQuery<'a>>,
    sidecar: Option<&'a crate::project_memory::sqlite::MemorySidecar>,
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
