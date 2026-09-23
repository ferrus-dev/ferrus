//! Native, read-only graph and memory retrieval under the exact managed binding.

use super::{
    ferrus::FerrusSession,
    tools::*,
    workspace::{self, Workspace},
};
use crate::{
    project_memory::{
        domain::{FederationPageCursor, MemoryQueryText},
        federation::{self, ContextDomain, FederatedContextSeed},
        query::MemoryContextPolicy,
    },
    project_memory_runtime::LocalProjectContext,
    repository_graph::{
        domain::{PageCursor, QueryBudget, RepoPath},
        query::{self, ContextPolicy, ContextSeed, PageRequest},
    },
    repository_graph_runtime::LocalGraphContext,
};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::num::{NonZeroU32, NonZeroU64};

pub(crate) const NAMES: &[&str] = &[
    "repository_graph_status",
    "repository_search",
    "repository_context",
    "project_memory_status",
    "project_context_search",
    "project_context",
    "repository_fallback",
];

pub(super) fn fields(name: &str) -> &'static [&'static str] {
    match name {
        "repository_graph_status" => &[],
        "project_memory_status" => &[
            "max_results",
            "max_bytes",
            "max_duration_ms",
            "max_diagnostics",
        ],
        "repository_search" => &[
            "query",
            "paths",
            "kinds",
            "cursor",
            "max_results",
            "max_bytes",
            "max_duration_ms",
            "max_diagnostics",
        ],
        "project_context_search" => &[
            "domain",
            "query",
            "paths",
            "kinds",
            "cursor",
            "max_results",
            "max_bytes",
            "max_duration_ms",
            "max_diagnostics",
        ],
        "repository_context" => &[
            "seeds",
            "cursor",
            "max_results",
            "max_bytes",
            "max_depth",
            "max_duration_ms",
            "max_diagnostics",
            "max_snippet_bytes",
            "include_snippets",
            "include_unresolved",
            "direction",
        ],
        "project_context" => &[
            "domain",
            "seeds",
            "cursor",
            "max_results",
            "max_bytes",
            "max_depth",
            "max_duration_ms",
            "max_diagnostics",
            "max_snippet_bytes",
            "include_snippets",
            "include_unresolved",
            "include_stale",
            "direction",
        ],
        _ => &[],
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    domain: Option<ContextDomain>,
    query: Option<String>,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    kinds: Vec<String>,
    #[serde(default)]
    seeds: Vec<Seed>,
    cursor: Option<String>,
    max_results: Option<u32>,
    max_bytes: Option<u64>,
    max_depth: Option<u32>,
    max_duration_ms: Option<u64>,
    max_diagnostics: Option<u32>,
    max_snippet_bytes: Option<u64>,
    #[serde(default)]
    include_snippets: bool,
    #[serde(default)]
    include_unresolved: bool,
    #[serde(default)]
    include_stale: bool,
    #[serde(default)]
    direction: query::EdgeDirection,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum Seed {
    Node(String),
    Symbol(String),
    Path(String),
    MemoryEntity(String),
    Milestone(String),
    Task(String),
    Run(String),
}

impl Seed {
    fn value(&self) -> &str {
        match self {
            Self::Node(s)
            | Self::Symbol(s)
            | Self::Path(s)
            | Self::MemoryEntity(s)
            | Self::Milestone(s)
            | Self::Task(s)
            | Self::Run(s) => s,
        }
    }

    fn native(&self) -> Result<FederatedContextSeed> {
        use crate::{
            project_memory::domain::{MemoryEntityId, MemoryRecordId},
            repository_graph::domain::{NodeId, SemanticKey},
        };

        Ok(match self {
            Self::Node(v) => FederatedContextSeed::Repository(ContextSeed::Node(NodeId::new(v)?)),
            Self::Symbol(v) => {
                FederatedContextSeed::Repository(ContextSeed::Symbol(SemanticKey::new(v)?))
            }
            Self::Path(v) => FederatedContextSeed::Repository(ContextSeed::Path(RepoPath::new(v)?)),
            Self::MemoryEntity(v) => FederatedContextSeed::MemoryEntity(MemoryEntityId::new(v)?),
            Self::Milestone(v) => FederatedContextSeed::Milestone(MemoryRecordId::new(v)?),
            Self::Task(v) => FederatedContextSeed::Task(MemoryRecordId::new(v)?),
            Self::Run(v) => FederatedContextSeed::Run(MemoryRecordId::new(v)?),
        })
    }
}

impl Request {
    pub(crate) fn parse(name: &str, value: Value) -> Result<Self> {
        let object = value
            .as_object()
            .context("Expected context request object")?;

        ensure!(
            object
                .keys()
                .all(|key| fields(name).contains(&key.as_str())),
            "Unsupported context field"
        );

        let r: Self = serde_json::from_value(value)?;
        ensure!(NAMES.contains(&name), "Unknown context tool");

        if name.starts_with("project_context") {
            r.domain.context("Explicit domain is required")?;
        } else {
            ensure!(r.domain.is_none(), "This tool has a fixed domain");
        }

        if name.ends_with("search") {
            let query = r.query.as_deref().context("Query is required")?;
            ensure!(
                !query.trim().is_empty() && query.len() <= 512,
                "Invalid query"
            );
        }

        if name == "repository_context" || name == "project_context" {
            ensure!(
                !r.seeds.is_empty() && r.seeds.len() <= 32,
                "Expected 1..=32 seeds"
            );
        }

        ensure!(
            r.paths.len() <= 32 && r.kinds.len() <= 32 && r.seeds.len() <= 32,
            "Too many filters"
        );

        for value in r
            .paths
            .iter()
            .chain(&r.kinds)
            .map(String::as_str)
            .chain(r.seeds.iter().map(Seed::value))
        {
            ensure!(
                !value.trim().is_empty() && value.len() <= 512,
                "Invalid filter or seed"
            );
        }

        for path in &r.paths {
            RepoPath::new(path)?;
        }

        for seed in &r.seeds {
            let native = seed.native()?;
            match r.domain.unwrap_or(ContextDomain::Repository) {
                ContextDomain::Repository => ensure!(
                    matches!(native, FederatedContextSeed::Repository(_)),
                    "Repository seeds required"
                ),
                ContextDomain::Memory => ensure!(
                    !matches!(
                        native,
                        FederatedContextSeed::Repository(ContextSeed::Node(_))
                    ),
                    "Memory cannot resolve node IDs"
                ),
                ContextDomain::All => (),
            }
        }

        ensure!(
            r.cursor
                .as_ref()
                .is_none_or(|s| !s.is_empty() && s.len() <= 16384),
            "Invalid cursor"
        );

        for n in [
            r.max_results.map(u64::from),
            r.max_bytes,
            r.max_depth.map(u64::from),
            r.max_duration_ms,
            r.max_diagnostics.map(u64::from),
            r.max_snippet_bytes,
        ]
        .into_iter()
        .flatten()
        {
            ensure!(n > 0, "Query limits must be positive");
        }

        Ok(r)
    }

    fn graph_budget(&self, graph: &LocalGraphContext) -> Result<QueryBudget> {
        let default =
            crate::repository_graph::query_sqlite::default_budget(&graph.config.query_limits)?;

        Ok(QueryBudget::new(
            NonZeroU32::new(
                self.max_results
                    .unwrap_or(default.max_results.get())
                    .min(64),
            )
            .unwrap(),
            NonZeroU64::new(
                self.max_bytes
                    .unwrap_or(default.max_bytes.get())
                    .min(24 * 1024),
            )
            .unwrap(),
            NonZeroU32::new(self.max_depth.unwrap_or(default.max_depth.get()).min(8)).unwrap(),
            NonZeroU64::new(
                self.max_duration_ms
                    .unwrap_or(default.max_duration_ms.get())
                    .min(1000),
            )
            .unwrap(),
            NonZeroU32::new(
                self.max_diagnostics
                    .unwrap_or(default.max_diagnostics.get())
                    .min(16),
            )
            .unwrap(),
        ))
    }
}

/// Keep typed domain responses up to the final model-tool encoding boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "result", rename_all = "snake_case")]
pub(crate) enum Response {
    RepositoryStatus(query::StatusResponse),
    RepositorySearch(std::result::Result<query::SearchResponse, query::QueryError>),
    RepositoryContext(std::result::Result<query::ContextResponse, query::QueryError>),
    MemoryStatus(crate::project_memory::query::MemoryStatusResponse),
    ProjectSearch(std::result::Result<federation::FederatedSearchResponse, String>),
    ProjectContext(std::result::Result<federation::FederatedContextResponse, String>),
}

pub(crate) struct Context {
    session: FerrusSession,
    cache: std::sync::Mutex<super::working_set::QueryCache>,
    pub(super) cache_enabled: bool,
}

impl Context {
    pub(crate) fn new(session: FerrusSession) -> Self {
        Self {
            session,
            cache: Default::default(),
            cache_enabled: true,
        }
    }

    pub(super) fn invalidate(&self) {
        self.cache.lock().unwrap().clear();
    }

    pub(super) async fn revisions(&self) -> Result<Value> {
        let runtime = self.session.status().await?;
        let graph = LocalGraphContext::load_for_runtime(
            self.session.project_root(),
            self.session.project_id(),
            self.session.data_dir(),
            &runtime,
        )
        .await;
        let snapshot = match graph {
            Ok(graph) => graph.status().await.ok().and_then(|s| s.snapshot_id),
            Err(_) => None,
        };
        // Optional sidecars must not prevent ordinary workspace tools from running.
        let memory = async {
            let local = LocalProjectContext::load_for_runtime(
                self.session.project_root(),
                self.session.project_id(),
                self.session.data_dir(),
                &runtime,
                ContextDomain::Memory,
                false,
            )
            .await?;
            let budget = local.requested_budget(
                Some(1),
                Some(4096),
                Some(1),
                Some(1),
                Some(1000),
                Some(1),
            )?;
            local.memory_status(budget).map(|s| s.revision_id)
        }
        .await
        .unwrap_or(None);
        Ok(
            json!({"snapshot_id":snapshot,"memory_revision_id":memory,"task_view":super::working_set::view_identity(&runtime.repository_view)}),
        )
    }

    async fn graph(&self) -> Result<LocalGraphContext> {
        let runtime = self.session.status().await?;
        LocalGraphContext::load_for_runtime(
            self.session.project_root(),
            self.session.project_id(),
            self.session.data_dir(),
            &runtime,
        )
        .await
    }

    pub(crate) async fn retrieve(&self, name: &str, input: Request) -> Result<Response> {
        // Revalidate before constructing a view, including memory-only requests.
        let runtime = self.session.status().await?;

        if name.starts_with("repository_") {
            let graph = LocalGraphContext::load_for_runtime(
                self.session.project_root(),
                self.session.project_id(),
                self.session.data_dir(),
                &runtime,
            )
            .await?;

            if name == "repository_graph_status" {
                return Ok(Response::RepositoryStatus(graph.status().await?));
            }

            let mut scope = graph.scope(input.graph_budget(&graph)?)?;
            // Resolve a mutable publication exactly once for this assembly.
            let status = graph.status().await?;
            if let Some(snapshot) = &status.snapshot_id {
                scope.snapshot = query::SnapshotSelector::Snapshot(snapshot.clone());
            }
            let cacheable =
                self.cache_enabled && !input.include_snippets && status.snapshot_id.is_some();
            let key = super::working_set::identity(&(
                name,
                &input,
                &scope,
                &status,
                graph
                    .repository_view
                    .as_ref()
                    .map(super::working_set::view_identity),
                &graph.run_id,
                format!("{:?}", graph.config),
                self.session.project_id(),
                &self.session.scope.task_id,
                self.session.workspace(),
            ));
            if cacheable && let Some(value) = self.cache.lock().unwrap().get(&key) {
                return Ok(serde_json::from_value(value)?);
            }
            let page = PageRequest {
                cursor: input.cursor.map(PageCursor::new).transpose()?,
            };

            let response = if name == "repository_search" {
                Response::RepositorySearch(
                    graph
                        .search(&query::SearchRequest {
                            scope,
                            text: input.query.context("Missing query")?.trim().into(),
                            node_kinds: input.kinds,
                            paths: input
                                .paths
                                .into_iter()
                                .map(RepoPath::new)
                                .collect::<Result<_, _>>()?,
                            page,
                        })
                        .await?,
                )
            } else {
                let seeds = input
                    .seeds
                    .iter()
                    .map(|s| match s.native()? {
                        FederatedContextSeed::Repository(seed) => Ok(seed),
                        _ => anyhow::bail!("Expected repository seed"),
                    })
                    .collect::<Result<_>>()?;

                let request = query::ContextRequest {
                    scope,
                    seeds,
                    page,
                    policy: ContextPolicy {
                        direction: input.direction,
                        edge_kinds: vec![],
                        include_unresolved: input.include_unresolved,
                        include_external: false,
                    },
                };

                Response::RepositoryContext(if input.include_snippets {
                    graph
                        .context_with_snippets(
                            &request,
                            NonZeroU64::new(input.max_snippet_bytes.unwrap_or(4096).min(8192))
                                .unwrap(),
                        )
                        .await?
                } else {
                    graph.context(&request).await?
                })
            };
            if cacheable
                && matches!(
                    &response,
                    Response::RepositorySearch(Ok(_)) | Response::RepositoryContext(Ok(_))
                )
            {
                self.cache
                    .lock()
                    .unwrap()
                    .insert(key, serde_json::to_value(&response)?);
            }
            return Ok(response);
        }

        let domain = input.domain.unwrap_or(ContextDomain::Memory);
        let local = LocalProjectContext::load_for_runtime(
            self.session.project_root(),
            self.session.project_id(),
            self.session.data_dir(),
            &runtime,
            domain,
            input.include_snippets,
        )
        .await?;

        let budget = local.requested_budget(
            Some(input.max_results.unwrap_or(32).min(64)),
            Some(input.max_bytes.unwrap_or(24 * 1024).min(24 * 1024)),
            Some(input.max_snippet_bytes.unwrap_or(4096).min(8192)),
            Some(input.max_depth.unwrap_or(2).min(8)),
            Some(input.max_duration_ms.unwrap_or(1000).min(1000)),
            Some(input.max_diagnostics.unwrap_or(16).min(16)),
        )?;

        if name == "project_memory_status" {
            return Ok(Response::MemoryStatus(local.memory_status(budget)?));
        }

        let scope = local.pinned_scope(domain, budget).await?;
        let cursor = input.cursor.map(FederationPageCursor::new).transpose()?;

        if name == "project_context_search" {
            Ok(Response::ProjectSearch(
                local
                    .search(federation::FederatedSearchRequest {
                        scope,
                        text: MemoryQueryText::new(input.query.context("Missing query")?.trim())?,
                        repository_kinds: input
                            .kinds
                            .into_iter()
                            .map(crate::project_memory::domain::MemoryStatusToken::new)
                            .collect::<Result<_, _>>()?,
                        repository_paths: input
                            .paths
                            .into_iter()
                            .map(RepoPath::new)
                            .collect::<Result<_, _>>()?,
                        memory_kinds: vec![],
                        memory_sources: vec![],
                        cursor,
                    })
                    .map_err(|e| e.to_string()),
            ))
        } else {
            Ok(Response::ProjectContext(
                local
                    .context(federation::FederatedContextRequest {
                        scope,
                        seeds: input
                            .seeds
                            .iter()
                            .map(Seed::native)
                            .collect::<Result<_>>()?,
                        repository_policy: ContextPolicy {
                            direction: input.direction,
                            edge_kinds: vec![],
                            include_unresolved: input.include_unresolved,
                            include_external: false,
                        },
                        memory_policy: MemoryContextPolicy {
                            direction: input.direction,
                            relationship_kinds: vec![],
                            include_unresolved: input.include_unresolved,
                            include_stale: input.include_stale,
                            include_snippets: input.include_snippets,
                        },
                        cursor,
                    })
                    .map_err(|e| e.to_string()),
            ))
        }
    }

    /// Explicit fallback selection cannot replace a failed runtime binding or graph routing.
    pub(crate) async fn fallback(
        &self,
        request: Fallback,
        cancellation: &Cancellation,
    ) -> Result<Value> {
        let graph = self.graph().await?;
        let status = graph.status().await?;
        let workspace = Workspace::new(self.session.workspace(), workspace::Limits::default())?;

        let (reason, evidence) = match request {
            Fallback::Read { reason, input } => (
                reason,
                serde_json::to_value(
                    workspace
                        .read_file(input)
                        .map_err(|e| anyhow::anyhow!("Workspace read: {:?}", e.code))?,
                )?,
            ),
            Fallback::Search { reason, input } => (
                reason,
                serde_json::to_value(
                    workspace
                        .search_text(input, cancellation)
                        .await
                        .map_err(|e| anyhow::anyhow!("Workspace search: {:?}", e.code))?,
                )?,
            ),
        };

        Ok(
            json!({"kind":"workspace_fallback", "requested_reason":reason, "graph_status":status, "evidence":evidence}),
        )
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FallbackReason {
    Missing,
    Disabled,
    Stale,
    Ambiguous,
    Unsupported,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Fallback {
    Read {
        reason: FallbackReason,
        input: workspace::ReadRequest,
    },
    Search {
        reason: FallbackReason,
        input: workspace::SearchRequest,
    },
}
