//! Local sidecar scope is supplied by the host, never resolved through ferrus.db.

use super::{
    super::context_request::{Request, Response},
    storage::Storage,
};
use crate::{project_memory as memory, repository_graph as graph};
use anyhow::{Context as _, Result, ensure};
use graph::{domain::*, ports::GraphQuery, query::*};
use memory::{
    federation::*,
    ports::{ContextService, MemoryQuery},
};
use std::{
    collections::BTreeSet,
    num::{NonZeroU32, NonZeroU64},
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub(super) struct LocalContext {
    root: PathBuf,
    sidecar: PathBuf,
    invalidation: PathBuf,
    dirty: bool,
    repository: RepositoryRef,
    pub graph_enabled: bool,
    memory: Option<(PathBuf, memory::domain::ProjectRef)>,
}

impl LocalContext {
    pub fn new(
        root: &Path,
        store: &Storage,
        graph_enabled: bool,
        sidecar: Option<PathBuf>,
        namespace: Option<String>,
        project: Option<String>,
    ) -> Result<Self> {
        let memory = match sidecar {
            Some(path) => {
                ensure!(path.is_absolute(), "Memory sidecar path must be absolute");
                Some((
                    path,
                    memory::domain::ProjectRef {
                        namespace: memory::domain::ProjectNamespace::new(
                            namespace.context("Missing memory namespace")?,
                        )?,
                        project_id: memory::domain::ProjectId::new(
                            project.context("Missing memory project ID")?,
                        )?,
                    },
                ))
            }
            None => None,
        };
        Ok(Self {
            root: root.into(),
            sidecar: store.path.join("repo-graph.db"),
            invalidation: store.path.join("graph-invalidated"),
            dirty: false,
            repository: RepositoryRef {
                namespace: RepositoryNamespace::new("local:nano-standalone")?,
                repository_id: RepositoryId::new(&store.workspace_id)?,
            },
            graph_enabled,
            memory,
        })
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        if !enabled {
            self.graph_enabled = false;
            self.memory = None;
        }
    }

    pub fn has_memory(&self) -> bool {
        self.memory.is_some()
    }

    fn config(&self) -> graph::config::RepositoryGraphConfig {
        graph::config::RepositoryGraphConfig {
            enabled: true,
            ..Default::default()
        }
    }

    fn source(&self) -> Result<graph::source::LocalRepositorySource> {
        let config = self.config();
        let identities = graph::index::active_extractor_identities(&config)?;
        let scope = graph::source::SourceDiscoveryContext::from_config(
            self.repository.clone(),
            &config,
            &identities,
        )?;
        Ok(graph::source::LocalRepositorySource::discover(
            &self.root, scope,
        )?)
    }

    pub fn index(&self) -> Result<graph::index::IndexOutcome> {
        let source = self.source()?;
        let graph::sqlite::OpenSidecarResult::Ready(mut sidecar) =
            graph::sqlite::open_for_build_at(&self.sidecar)?
        else {
            anyhow::bail!("Standalone graph sidecar requires rebuild");
        };
        let outcome = graph::index::IndexCoordinator::new(&mut sidecar).index(
            &source,
            &self.config(),
            graph::index::IndexRequest {
                build_id: BuildId::new(format!("nano-index-{}", super::storage::fresh_id()?))?,
                view_name: PublishedViewName::new("standalone")?,
                force_full: false,
            },
        )?;
        match std::fs::remove_file(&self.invalidation) {
            Ok(()) => super::super::private::sync_directory(self.invalidation.parent().unwrap())?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        Ok(outcome)
    }

    pub fn invalidate(&mut self) {
        self.dirty = true;
        // Derived-state failure must not turn a workspace edit into a failure.
        // Every publication still reports unknown freshness for external writers.
        let result = (|| -> Result<()> {
            if !self.sidecar.try_exists()? || self.invalidation.try_exists()? {
                return Ok(());
            }
            let file = super::super::private::file(&self.invalidation, true)?;
            file.sync_all()?;
            super::super::private::sync_directory(self.invalidation.parent().unwrap())?;
            Ok(())
        })();
        if let Err(error) = result {
            tracing::warn!(
                ?error,
                "Standalone graph invalidation could not be persisted"
            );
        }
    }

    fn graph(&self) -> Result<Option<graph::sqlite::Sidecar>> {
        if !self.graph_enabled {
            return Ok(None);
        }
        match graph::sqlite::open_for_query_at(&self.sidecar)? {
            graph::sqlite::OpenQuerySidecarResult::Ready(sidecar) => Ok(Some(sidecar)),
            graph::sqlite::OpenQuerySidecarResult::Absent => Ok(None),
            _ => anyhow::bail!("Standalone graph sidecar requires migration or rebuild"),
        }
    }

    fn scope(&self, budget: QueryBudget) -> Result<QueryScope> {
        Ok(QueryScope::current(
            self.repository.clone(),
            SnapshotSelector::Published(PublishedViewName::new("standalone")?),
            budget,
        ))
    }

    pub async fn retrieve(&self, name: &str, input: Request) -> Result<serde_json::Value> {
        let local = self.clone();
        let name = name.to_owned();
        tokio::task::spawn_blocking(move || local.retrieve_local(&name, input)).await?
    }

    pub async fn revisions(&self) -> Result<serde_json::Value> {
        let local = self.clone();
        tokio::task::spawn_blocking(move || {
            // Inspect the same publications used by retrieval, without indexing
            // or claiming workspace freshness. Optional sidecar failures remove
            // only that domain's identity; known dirty graph evidence stays out.
            let graph_request = Request::parse("repository_graph_status", serde_json::json!({}))?;
            let memory_request = Request::parse("project_memory_status", serde_json::json!({}))?;
            let snapshot = local
                .retrieve_local("repository_graph_status", graph_request)
                .ok()
                .filter(|status| status["result"]["freshness"]["freshness"] != "stale")
                .map(|status| status["result"]["snapshot_id"].clone());
            let memory = local
                .retrieve_local("project_memory_status", memory_request)
                .ok()
                .map(|status| status["result"]["revision_id"].clone());
            Ok(serde_json::json!({"snapshot_id":snapshot,"memory_revision_id":memory,"task_view":null}))
        })
        .await?
    }

    fn retrieve_local(&self, name: &str, input: Request) -> Result<serde_json::Value> {
        let config = self.config();
        let includes_graph = name.starts_with("repository_")
            || matches!(
                input.domain,
                Some(ContextDomain::Repository | ContextDomain::All)
            );
        let graph = if includes_graph { self.graph()? } else { None };
        // Match latency-bounded native retrieval: external writers make
        // freshness unknown. Do not rescan the repository on every model query.
        let graph_query = memory::local_query::OptionalGraphQuery::new(
            graph.as_ref(),
            config.query_limits.clone(),
            None,
        );
        let dirty = self.dirty || self.invalidation.try_exists()?;
        let mut scope = self.scope(input.graph_budget(&config.query_limits)?)?;
        if name.starts_with("repository_") {
            let mut status = match graph_query.status(&StatusRequest {
                scope: scope.clone(),
            }) {
                Ok(status) => status,
                Err(error) if error.code == QueryErrorCode::NotBuilt => self.not_built_status()?,
                Err(error) => return Err(error.into()),
            };
            if dirty && status.snapshot_id.is_some() {
                status.freshness.freshness = Freshness::Stale;
                status
                    .freshness
                    .reason_codes
                    .push("standalone.workspace_changed".into());
            }
            if name == "repository_graph_status" {
                return Ok(serde_json::to_value(Response::RepositoryStatus(status))?);
            }
            ensure!(
                !dirty,
                "Standalone graph is stale; use --index-graph or repository_fallback"
            );
            scope.snapshot = SnapshotSelector::Snapshot(
                status
                    .snapshot_id
                    .context("Standalone graph has no publication")?,
            );
            let page = PageRequest {
                cursor: input.cursor.map(PageCursor::new).transpose()?,
            };
            let response = if name == "repository_search" {
                Response::RepositorySearch(
                    graph_query.search(&SearchRequest {
                        scope,
                        text: input.query.context("Missing query")?,
                        node_kinds: input.kinds,
                        paths: input
                            .paths
                            .into_iter()
                            .map(RepoPath::new)
                            .collect::<Result<_, _>>()?,
                        page,
                    }),
                )
            } else {
                let request = ContextRequest {
                    scope,
                    seeds: input
                        .seeds
                        .iter()
                        .map(|seed| match seed.native()? {
                            FederatedContextSeed::Repository(seed) => Ok(seed),
                            _ => anyhow::bail!("Repository seed required"),
                        })
                        .collect::<Result<_>>()?,
                    policy: ContextPolicy {
                        direction: input.direction,
                        edge_kinds: vec![],
                        include_unresolved: input.include_unresolved,
                        include_external: false,
                    },
                    page,
                };
                let mut response = graph_query.context(&request)?;
                if input.include_snippets {
                    self.snippets(
                        graph.as_ref().context("Graph unavailable")?,
                        &mut response,
                        input.max_snippet_bytes.unwrap_or(4096).min(8192),
                        request.scope.budget.max_diagnostics.get() as usize,
                    )?;
                }
                Response::RepositoryContext(Ok(response))
            };
            return Ok(serde_json::to_value(response)?);
        }
        let (path, project) = self
            .memory
            .as_ref()
            .context("No explicit memory sidecar configured")?;
        let sidecar = match memory::sqlite::open_for_query_at(path)? {
            memory::sqlite::OpenMemoryQuerySidecarResult::Ready(sidecar) => sidecar,
            _ => anyhow::bail!("Memory sidecar unavailable"),
        };
        let limits = graph::config::QueryLimitsConfig::default();
        let budget = memory::query::MemoryQueryBudget {
            max_results: NonZeroU32::new(input.max_results.unwrap_or(32).min(64)).unwrap(),
            max_bytes: NonZeroU64::new(input.max_bytes.unwrap_or(24 * 1024).min(24 * 1024))
                .unwrap(),
            max_snippet_bytes: NonZeroU64::new(input.max_snippet_bytes.unwrap_or(4096).min(8192))
                .unwrap(),
            max_depth: NonZeroU32::new(input.max_depth.unwrap_or(2).min(8)).unwrap(),
            max_duration_ms: NonZeroU64::new(input.max_duration_ms.unwrap_or(1000).min(1000))
                .unwrap(),
            max_diagnostics: NonZeroU32::new(input.max_diagnostics.unwrap_or(16).min(16)).unwrap(),
        };
        let query = memory::query_sqlite::SqliteMemoryQuery::new(&sidecar, limits.clone());
        let memory_scope = memory::query::MemoryQueryScope::current(
            project.clone(),
            memory::query::MemoryRevisionSelector::Published(memory::domain::MemoryViewName::new(
                "project",
            )?),
            budget,
        );
        let status = query.status(memory::query::MemoryStatusRequest {
            scope: memory_scope,
        })?;
        if name == "project_memory_status" {
            return Ok(serde_json::to_value(Response::MemoryStatus(status))?);
        }
        ensure!(
            !input.include_snippets,
            "Standalone memory snippets require a verified content adapter; use structural memory context"
        );
        let memory = memory::query::MemoryRevisionSelector::Revision(
            status.revision_id.context("Memory has no publication")?,
        );
        let target = match input.domain.context("Explicit context domain required")? {
            ContextDomain::Memory => FederatedTarget::Memory { memory },
            domain => {
                let status = graph_query.status(&StatusRequest {
                    scope: scope.clone(),
                })?;
                ensure!(!dirty, "Standalone graph is stale; use repository_fallback");
                let repository = RepositoryContextTarget {
                    repository: self.repository.clone(),
                    snapshot: SnapshotSelector::Snapshot(
                        status.snapshot_id.context("No graph publication")?,
                    ),
                };
                if domain == ContextDomain::Repository {
                    FederatedTarget::Repository { repository }
                } else {
                    FederatedTarget::All { repository, memory }
                }
            }
        };
        let backend = memory::local_query::OptionalMemoryBackend {
            query: Some(query),
            sidecar: Some(&sidecar),
        };
        let service = memory::federation_service::FederatedContextService::new(
            &graph_query,
            &backend,
            &backend,
            limits,
            None,
        );
        let scope = FederatedScope::current(project.clone(), target, budget);
        let (repository_kinds, memory_kinds) = input.search_kind_filters()?;
        let cursor = input
            .cursor
            .map(memory::domain::FederationPageCursor::new)
            .transpose()?;
        let response = if name == "project_context_search" {
            Response::ProjectSearch(
                service
                    .search(FederatedSearchRequest {
                        scope,
                        text: memory::domain::MemoryQueryText::new(
                            input.query.context("Missing query")?,
                        )?,
                        repository_kinds,
                        repository_paths: input
                            .paths
                            .into_iter()
                            .map(RepoPath::new)
                            .collect::<Result<_, _>>()?,
                        memory_kinds,
                        memory_sources: vec![],
                        cursor,
                    })
                    .map_err(|e| e.to_string()),
            )
        } else {
            Response::ProjectContext(
                service
                    .context(FederatedContextRequest {
                        scope,
                        seeds: input
                            .seeds
                            .iter()
                            .map(|seed| seed.native())
                            .collect::<Result<_>>()?,
                        repository_policy: ContextPolicy {
                            direction: input.direction,
                            edge_kinds: vec![],
                            include_unresolved: input.include_unresolved,
                            include_external: false,
                        },
                        memory_policy: memory::query::MemoryContextPolicy {
                            direction: input.direction,
                            relationship_kinds: vec![],
                            include_unresolved: input.include_unresolved,
                            include_stale: input.include_stale,
                            include_snippets: false,
                        },
                        cursor,
                    })
                    .map_err(|e| e.to_string()),
            )
        };
        Ok(serde_json::to_value(response)?)
    }

    fn not_built_status(&self) -> Result<StatusResponse> {
        Ok(StatusResponse {
            wire_version: graph::QUERY_WIRE_VERSION,
            repository: self.repository.clone(),
            snapshot_id: None,
            source_revision: None,
            task_view: None,
            freshness: FreshnessEnvelope {
                freshness: Freshness::NotApplicable,
                compared_manifest: None,
                reason_codes: vec!["standalone.not_built".into()],
            },
            diagnostics: DiagnosticsEnvelope::default(),
            page: PageInfo {
                next_cursor: None,
                truncation: None,
            },
            data: StatusData {
                availability: Availability::NotBuilt,
                build_state: None,
                build_id: None,
                published_view: Some(PublishedViewName::new("standalone")?),
                graph_model_version: None,
                statistics: None,
                recommended_action: Some(RetrievalAction::Index),
                task_view_status: None,
                fallback: Some(RetrievalFallback::DirectSourceInspection),
            },
        })
    }

    fn snippets(
        &self,
        sidecar: &graph::sqlite::Sidecar,
        response: &mut ContextResponse,
        mut remaining: u64,
        max_diagnostics: usize,
    ) -> Result<()> {
        use graph::ports::SnapshotContent;
        let paths: BTreeSet<_> = response
            .data
            .items
            .iter()
            .map(|item| item.path.clone())
            .collect();
        let files =
            graph::query_sqlite::snapshot_file_descriptors(sidecar, &response.snapshot_id, &paths)?;
        let content = graph::source::LocalSnapshotContent::new(
            &self.root,
            self.repository.clone(),
            response.snapshot_id.clone(),
            &self.config().source,
            files,
            NonZeroU64::new(8192).unwrap(),
        )?;
        let mut seen = BTreeSet::new();
        let mut truncated = false;
        for item in &response.data.items {
            if !seen.insert(serde_json::to_string(&(&item.path, &item.span))?) {
                continue;
            }
            let Some(max_bytes) = NonZeroU64::new(remaining) else {
                truncated = true;
                break;
            };
            let snippet = content.read_verified(&ContentRequest {
                wire_version: graph::QUERY_WIRE_VERSION,
                repository: self.repository.clone(),
                snapshot_id: response.snapshot_id.clone(),
                path: item.path.clone(),
                expected_content_identity: item.content_identity.clone(),
                span: item.span.clone(),
                max_bytes,
            })?;
            let text = String::from_utf8(snippet.bytes)?;
            remaining = remaining.saturating_sub(text.len() as u64);
            truncated |= snippet.truncated;
            response.data.snippets.push(ContextSnippet {
                path: item.path.clone(),
                span: item.span.clone(),
                verified_content_identity: snippet.verified_content_identity,
                text,
                truncated: snippet.truncated,
            });
        }
        if truncated {
            response.diagnostics.summary.warning += 1;
            if response.diagnostics.items.len() < max_diagnostics {
                response.diagnostics.items.push(QueryDiagnostic {
                    severity: DiagnosticSeverity::Warning,
                    code: DiagnosticCode::new("content.snippets_truncated")?,
                    location: None,
                });
            } else {
                response.diagnostics.truncated = true;
            }
        }
        Ok(())
    }
}
