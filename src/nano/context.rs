//! Native, read-only graph and memory retrieval under an explicit session binding.

use super::{
    binding::Binding,
    tools::*,
    workspace::{self, Workspace},
};
#[cfg(all(test, feature = "nano-mcp"))]
use crate::repository_graph::config::QueryLimitsConfig;
use crate::{
    project_memory::{
        domain::{FederationPageCursor, MemoryQueryText},
        federation::{self, ContextDomain, FederatedContextSeed},
        query::MemoryContextPolicy,
    },
    repository_graph::{
        domain::{PageCursor, RepoPath},
        query::{self, ContextPolicy, PageRequest},
    },
    repository_graph_runtime::LocalGraphContext,
};
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::num::NonZeroU64;

pub(crate) use super::context_request::*;

pub(crate) struct Context {
    session: Binding,
    cache: std::sync::Mutex<super::working_set::QueryCache>,
    pub(super) cache_enabled: bool,
}

impl Context {
    pub(crate) fn new(session: impl Into<Binding>) -> Self {
        Self {
            session: session.into(),
            cache: Default::default(),
            cache_enabled: true,
        }
    }

    pub(super) fn invalidate(&self) {
        self.cache.lock().unwrap().clear();
    }

    pub(super) async fn revisions(&self) -> Result<Value> {
        let runtime = self.session.status().await?;
        let graph = self.session.graph().await;
        let snapshot = match graph {
            Ok(graph) => graph.status().await.ok().and_then(|s| {
                if runtime.is_none()
                    && s.freshness.freshness != crate::repository_graph::domain::Freshness::Fresh
                {
                    None
                } else {
                    s.snapshot_id
                }
            }),
            Err(_) => None,
        };
        // Optional sidecars must not prevent ordinary workspace tools from running.
        let memory = async {
            let local = self
                .session
                .project_context(ContextDomain::Memory, false)
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
            json!({"snapshot_id":snapshot,"memory_revision_id":memory,"task_view":runtime.as_ref().map(|runtime| super::binding::view_identity(&runtime.repository_view))}),
        )
    }

    async fn graph(&self) -> Result<LocalGraphContext> {
        self.session.graph().await
    }

    #[cfg(feature = "nano-mcp")]
    pub(crate) async fn normalized_graph_arguments(
        &self,
        name: &str,
        value: Value,
    ) -> Result<Value> {
        let request = Request::parse(name, value.clone())?;
        if name == "repository_graph_status" {
            return Ok(value);
        }
        let graph = self.graph().await?;
        request.with_graph_budget(value, &graph.config.query_limits)
    }

    pub(crate) async fn retrieve(&self, name: &str, input: Request) -> Result<Response> {
        // Revalidate before constructing a view, including memory-only requests.
        self.session.status().await?;

        if name.starts_with("repository_") {
            let graph = self.session.graph().await?;

            if name == "repository_graph_status" {
                return Ok(Response::RepositoryStatus(graph.status().await?));
            }

            let mut scope = graph.scope(input.graph_budget(&graph.config.query_limits)?)?;
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
                    .map(super::binding::view_identity),
                &graph.run_id,
                format!("{:?}", graph.config),
                self.session.project_id(),
                self.session.task_id(),
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
        let local = self
            .session
            .project_context(domain, input.include_snippets)
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

#[cfg(all(test, feature = "nano-mcp"))]
mod graph_peer_tests {
    use super::*;

    #[test]
    fn graph_peer_arguments_use_native_defaults_and_caps() {
        let limits = QueryLimitsConfig::default();
        let search = json!({"query":"needle"});
        let normalized = Request::parse("repository_search", search.clone())
            .unwrap()
            .with_graph_budget(search, &limits)
            .unwrap();
        assert_eq!(normalized["max_results"], 64);
        assert_eq!(normalized["max_bytes"], 24 * 1024);
        assert_eq!(normalized["max_duration_ms"], 1000);
        assert_eq!(normalized["max_diagnostics"], 16);

        let context = json!({
            "seeds":[{"type":"path","value":"src/lib.rs"}],
            "include_snippets":true,
            "max_results":1000,
            "max_bytes":100_000,
            "max_depth":100,
            "max_duration_ms":10_000,
            "max_diagnostics":100,
            "max_snippet_bytes":100_000
        });
        let normalized = Request::parse("repository_context", context.clone())
            .unwrap()
            .with_graph_budget(context, &limits)
            .unwrap();
        assert_eq!(normalized["max_results"], 64);
        assert_eq!(normalized["max_bytes"], 24 * 1024);
        assert_eq!(normalized["max_depth"], 8);
        assert_eq!(normalized["max_duration_ms"], 1000);
        assert_eq!(normalized["max_diagnostics"], 16);
        assert_eq!(normalized["max_snippet_bytes"], 8192);
    }

    #[test]
    fn graph_peer_arguments_preserve_stricter_limits() {
        let limits = QueryLimitsConfig::default();
        let context = json!({
            "seeds":[{"type":"path","value":"src/lib.rs"}],
            "include_snippets":true,
            "max_results":2,
            "max_bytes":4096,
            "max_depth":1,
            "max_duration_ms":100,
            "max_diagnostics":1,
            "max_snippet_bytes":512
        });
        let normalized = Request::parse("repository_context", context.clone())
            .unwrap()
            .with_graph_budget(context.clone(), &limits)
            .unwrap();
        assert_eq!(normalized, context);

        let defaults =
            json!({"seeds":[{"type":"path","value":"src/lib.rs"}],"include_snippets":true});
        let normalized = Request::parse("repository_context", defaults.clone())
            .unwrap()
            .with_graph_budget(defaults, &limits)
            .unwrap();
        assert_eq!(normalized["max_depth"], limits.max_depth);
        assert_eq!(normalized["max_snippet_bytes"], 4096);
    }
}
