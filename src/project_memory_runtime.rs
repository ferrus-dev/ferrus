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
        config::QueryLimitsConfig,
        domain::QueryBudget,
        sqlite::{
            OpenQuerySidecarResult, SIDECAR_FILE_NAME, open_for_query_at as open_graph_for_query_at,
        },
    },
    repository_graph_runtime::LocalGraphContext,
};

pub(crate) const PROJECT_MEMORY_VIEW: &str = "project";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArchiveMemoryRefreshOutcome {
    Refreshed,
    Failed,
}

/// Refreshes derived memory after the archive transaction has committed.
/// Failure is deliberately isolated from the successful archive lifecycle.
pub(crate) async fn refresh_after_archive_best_effort() -> ArchiveMemoryRefreshOutcome {
    let started = std::time::Instant::now();
    match crate::project_memory::index::index_current_project(
        crate::project_memory::index::MemoryIndexOptions::default(),
    )
    .await
    {
        Ok(outcome) => {
            tracing::info!(
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
            );
            ArchiveMemoryRefreshOutcome::Refreshed
        }
        Err(error) => {
            tracing::warn!(
                error_category = memory_index_error_category(&error),
                trigger = "spec_archive",
                "project memory refresh failed after successful spec archive; archive state is unchanged"
            );
            ArchiveMemoryRefreshOutcome::Failed
        }
    }
}

pub(crate) struct LocalProjectContext {
    graph: Option<LocalGraphContext>,
    query_limits: QueryLimitsConfig,
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
        require_graph: bool,
    ) -> AnyResult<Self> {
        Self::load(require_graph, Some(agent_id), include_memory_content, false).await
    }

    async fn load(
        require_graph: bool,
        agent_id: Option<&str>,
        include_memory_content: bool,
        compare_local_freshness: bool,
    ) -> AnyResult<Self> {
        let graph = if require_graph {
            Some(match agent_id {
                Some(agent_id) => LocalGraphContext::load_for_agent(true, agent_id).await?,
                None => LocalGraphContext::load(true).await?,
            })
        } else {
            None
        };
        let query_limits = match graph.as_ref() {
            Some(graph) => graph.config.query_limits.clone(),
            None => load_memory_query_limits().await?,
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
            query_limits,
            project,
            data_dir,
            exact_memory_source,
            compare_local_freshness,
        })
    }

    /// Explicit project/run adaptation for native managed retrieval.
    pub(crate) async fn load_for_runtime(
        root: &std::path::Path,
        project_id: &str,
        data_dir: &std::path::Path,
        runtime: &project::RuntimeTaskContext,
        domain: ContextDomain,
        snippets: bool,
    ) -> AnyResult<Self> {
        Self::load_for_binding(root, project_id, data_dir, Some(runtime), domain, snippets).await
    }

    pub(crate) async fn load_for_binding(
        root: &std::path::Path,
        project_id: &str,
        data_dir: &std::path::Path,
        runtime: Option<&project::RuntimeTaskContext>,
        domain: ContextDomain,
        snippets: bool,
    ) -> AnyResult<Self> {
        let contents = tokio::fs::read_to_string(root.join("ferrus.toml")).await?;
        let graph = if domain != ContextDomain::Memory {
            Some(match runtime {
                Some(runtime) => {
                    LocalGraphContext::load_for_runtime(root, project_id, data_dir, runtime).await?
                }
                None => LocalGraphContext::load_canonical(root, project_id, data_dir).await?,
            })
        } else {
            None
        };
        let query_limits = QueryLimitsConfig::from_ferrus_toml(&contents)?;
        let project = ProjectRef {
            namespace: ProjectNamespace::new("local:ferrus")?,
            project_id: ProjectId::new(project_id)?,
        };
        let data_dir = data_dir.to_path_buf();
        let exact_memory_source = if snippets && domain != ContextDomain::Repository {
            let config: toml::Value = toml::from_str(&contents)?;
            let spec = config
                .get("spec")
                .and_then(|v| v.get("directory"))
                .and_then(toml::Value::as_str)
                .unwrap_or("docs/specs");
            let spec = crate::repository_graph::domain::RepoPath::new(spec)?;
            let (root, data, project) = (root.to_path_buf(), data_dir.clone(), project.clone());
            Some(
                tokio::task::spawn_blocking(move || {
                    LocalMemorySource::discover_at(
                        root,
                        data,
                        project,
                        spec,
                        MemoryPolicy::default(),
                    )
                })
                .await??,
            )
        } else {
            None
        };
        Ok(Self {
            graph,
            query_limits,
            project,
            data_dir,
            exact_memory_source,
            compare_local_freshness: false,
        })
    }

    pub(crate) fn default_budget(&self) -> AnyResult<MemoryQueryBudget> {
        default_memory_budget(&self.query_limits).map_err(Into::into)
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
        let graph = self.graph.as_ref();
        if matches!(domain, ContextDomain::Repository | ContextDomain::All)
            && graph.is_none_or(|graph| !graph.config.enabled)
        {
            anyhow::bail!(
                "repository graph is disabled; enable it before repository or combined retrieval"
            );
        }
        let memory = MemoryRevisionSelector::Published(
            MemoryViewName::new(PROJECT_MEMORY_VIEW).expect("static memory view is valid"),
        );
        let repository_scope = || -> AnyResult<RepositoryContextTarget> {
            let graph = graph.context("repository graph context was not loaded")?;
            let scope = graph.scope(repository_budget(&budget))?;
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

    /// Resolve both publications against the already captured runtime binding.
    pub(crate) async fn pinned_scope(
        &self,
        domain: ContextDomain,
        budget: MemoryQueryBudget,
    ) -> AnyResult<FederatedScope> {
        let mut scope = self.scope(domain, budget)?;
        match &mut scope.target {
            FederatedTarget::Repository { repository }
            | FederatedTarget::All { repository, .. } => {
                let graph = self.graph.as_ref().context("Missing repository binding")?;
                let snapshot = graph
                    .status()
                    .await?
                    .snapshot_id
                    .context("No repository snapshot available for context assembly")?;
                repository.snapshot =
                    crate::repository_graph::query::SnapshotSelector::Snapshot(snapshot);
            }
            _ => (),
        }
        match &mut scope.target {
            FederatedTarget::Memory { memory } | FederatedTarget::All { memory, .. } => {
                let revision = self
                    .memory_status(budget)?
                    .revision_id
                    .context("No memory revision available for context assembly")?;
                *memory = MemoryRevisionSelector::Revision(revision);
            }
            _ => (),
        }
        Ok(scope)
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
        let includes_repository = matches!(
            request.scope.target,
            FederatedTarget::Repository { .. } | FederatedTarget::All { .. }
        );
        let includes_memory = matches!(
            request.scope.target,
            FederatedTarget::Memory { .. } | FederatedTarget::All { .. }
        );
        let graph_sidecar = if includes_repository {
            self.open_graph_query().map_err(runtime_error)?
        } else {
            None
        };
        let memory_sidecar = if includes_memory {
            self.open_memory_query().map_err(runtime_error)?
        } else {
            None
        };
        if includes_repository && graph_sidecar.is_none() {
            return Err(MemoryQueryError::Unavailable);
        }
        if includes_memory && memory_sidecar.is_none() {
            return Err(MemoryQueryError::Unavailable);
        }
        let graph_query = OptionalGraphQuery::new(
            graph_sidecar.as_deref(),
            self.query_limits.clone(),
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
            self.query_limits.clone(),
            if includes_memory {
                self.memory_freshness_comparison().map_err(runtime_error)?
            } else {
                None
            },
        );
        let mut response = service.search(request)?;
        if let Some(repository) = response.repository.as_mut() {
            repository.task_view = self
                .graph
                .as_ref()
                .and_then(LocalGraphContext::task_view_envelope);
        }
        Ok(response)
    }

    pub(crate) fn context(
        &self,
        request: FederatedContextRequest,
    ) -> Result<FederatedContextResponse, MemoryQueryError> {
        let includes_repository = matches!(
            request.scope.target,
            FederatedTarget::Repository { .. } | FederatedTarget::All { .. }
        );
        let includes_memory = matches!(
            request.scope.target,
            FederatedTarget::Memory { .. } | FederatedTarget::All { .. }
        );
        let graph_sidecar = if includes_repository {
            self.open_graph_query().map_err(runtime_error)?
        } else {
            None
        };
        let memory_sidecar = if includes_memory {
            self.open_memory_query().map_err(runtime_error)?
        } else {
            None
        };
        if includes_repository && graph_sidecar.is_none() {
            return Err(MemoryQueryError::Unavailable);
        }
        if includes_memory && memory_sidecar.is_none() {
            return Err(MemoryQueryError::Unavailable);
        }
        let graph_query = OptionalGraphQuery::new(
            graph_sidecar.as_deref(),
            self.query_limits.clone(),
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
            self.query_limits.clone(),
            if includes_memory {
                self.memory_freshness_comparison().map_err(runtime_error)?
            } else {
                None
            },
        );
        let mut response = service.context(request)?;
        if let Some(repository) = response.repository.as_mut() {
            repository.task_view = self
                .graph
                .as_ref()
                .and_then(LocalGraphContext::task_view_envelope);
        }
        Ok(response)
    }

    fn memory_query<'a>(
        &'a self,
        sidecar: &'a crate::project_memory::sqlite::MemorySidecar,
    ) -> SqliteMemoryQuery<'a> {
        let query = SqliteMemoryQuery::new(sidecar, self.query_limits.clone());
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
            self.graph
                .as_ref()
                .and_then(|graph| graph.freshness_comparison().ok().flatten())
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

async fn load_memory_query_limits() -> AnyResult<QueryLimitsConfig> {
    let root = project::canonical_project_root().await?;
    let contents = tokio::fs::read_to_string(root.join("ferrus.toml"))
        .await
        .context("ferrus.toml not found -- run ferrus init first")?;
    QueryLimitsConfig::from_ferrus_toml(&contents)
        .context("Invalid [repository_graph.query_limits] configuration")
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
            retention: None,
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

use crate::project_memory::local_query::{OptionalGraphQuery, OptionalMemoryBackend};

#[cfg(test)]
#[path = "project_memory_runtime_tests.rs"]
mod tests;
