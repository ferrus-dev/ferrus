//! Bounded read-only federation across repository and project-memory queries.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::{NonZeroU32, NonZeroU64},
    time::Instant,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::repository_graph::{
    config::QueryLimitsConfig,
    domain::QueryBudget,
    ports::GraphQuery,
    query::{
        ContextData, ContextRequest, ContextResponse, ContextSeed, ContextSelectionKind,
        ContextSelectionReason, ContextSnippet, EdgeDirection, PageRequest, QueryError,
        QueryErrorCode, QueryScope, SearchRequest, SearchResponse, StatusRequest, StatusResponse,
        TruncationReason,
    },
};

use super::{
    FEDERATION_WIRE_VERSION,
    diagnostics::{MemoryDiagnostic, MemoryDiagnosticCode, MemoryDiagnosticSeverity},
    domain::{
        FederationPageCursor, MemoryBuildId, MemoryRelationship, MemoryRelationshipTarget,
        MemoryResolutionState, MemoryRevisionId, MemorySourceCategory,
    },
    federation::{
        FederatedContextItem, FederatedContextRequest, FederatedContextResponse,
        FederatedContextSeed, FederatedPageInfo, FederatedSearchRequest, FederatedSearchResponse,
        FederatedSearchResult, FederatedTarget, MemoryDomainState, RepositoryContextTarget,
        RepositoryDomainState,
    },
    policy::MemoryPolicy,
    ports::{ContextService, MemoryLinkStore, MemoryQuery},
    query::{
        MemoryContextPolicy, MemoryContextRequest, MemoryContextResponse, MemoryContextSeed,
        MemoryFreshnessComparison, MemoryPageRequest, MemoryQueryBudget, MemoryQueryError,
        MemoryQueryScope, MemoryRevisionSelector, MemorySearchRequest, MemorySearchResponse,
        MemoryStatusRequest, MemoryStatusResponse, MemoryTruncation, MemoryTruncationReason,
    },
};

const MAX_FEDERATION_CANDIDATES: usize = 4_096;
const CURSOR_VERSION: u32 = 1;

fn candidate_cap_truncated(desired: usize, backend_has_more: bool) -> bool {
    desired == MAX_FEDERATION_CANDIDATES && backend_has_more
}

fn repository_truncation_reason(reason: TruncationReason) -> MemoryTruncationReason {
    match reason {
        TruncationReason::Results => MemoryTruncationReason::Results,
        TruncationReason::Bytes => MemoryTruncationReason::Bytes,
        TruncationReason::Depth => MemoryTruncationReason::Depth,
        TruncationReason::Duration => MemoryTruncationReason::Duration,
        TruncationReason::Capability => MemoryTruncationReason::Capability,
    }
}

fn truncation_priority(reason: MemoryTruncationReason) -> u8 {
    match reason {
        MemoryTruncationReason::Duration => 5,
        MemoryTruncationReason::Bytes => 4,
        MemoryTruncationReason::Results => 3,
        MemoryTruncationReason::Depth => 2,
        MemoryTruncationReason::Capability => 1,
    }
}

fn stronger_truncation(
    left: Option<MemoryTruncationReason>,
    right: Option<MemoryTruncationReason>,
) -> Option<MemoryTruncationReason> {
    match (left, right) {
        (Some(left), Some(right)) => {
            Some(if truncation_priority(left) >= truncation_priority(right) {
                left
            } else {
                right
            })
        }
        (left, right) => left.or(right),
    }
}

#[derive(Debug, Clone, Copy)]
struct EffectiveBudget {
    max_results: usize,
    max_bytes: u64,
    max_snippet_bytes: u64,
    max_depth: u32,
    max_duration_ms: u64,
    max_diagnostics: usize,
}

impl EffectiveBudget {
    fn new(requested: &MemoryQueryBudget, limits: &QueryLimitsConfig) -> Self {
        Self {
            max_results: requested.max_results.get().min(limits.max_results) as usize,
            max_bytes: requested.max_bytes.get().min(limits.max_bytes),
            max_snippet_bytes: requested
                .max_snippet_bytes
                .get()
                .min(limits.max_snippet_bytes),
            max_depth: requested.max_depth.get().min(limits.max_depth),
            max_duration_ms: requested.max_duration_ms.get().min(limits.max_duration_ms),
            max_diagnostics: requested.max_diagnostics.get().min(limits.max_diagnostics) as usize,
        }
    }

    fn memory_budget(self, max_results: usize) -> MemoryQueryBudget {
        MemoryQueryBudget {
            max_results: NonZeroU32::new(u32::try_from(max_results.max(1)).unwrap_or(u32::MAX))
                .unwrap_or(NonZeroU32::MIN),
            max_bytes: NonZeroU64::new(self.max_bytes.max(1)).unwrap_or(NonZeroU64::MIN),
            max_snippet_bytes: NonZeroU64::new(self.max_snippet_bytes.max(1))
                .unwrap_or(NonZeroU64::MIN),
            max_depth: NonZeroU32::new(self.max_depth.max(1)).unwrap_or(NonZeroU32::MIN),
            max_duration_ms: NonZeroU64::new(self.max_duration_ms.max(1))
                .unwrap_or(NonZeroU64::MIN),
            max_diagnostics: NonZeroU32::new(
                u32::try_from(self.max_diagnostics.max(1)).unwrap_or(u32::MAX),
            )
            .unwrap_or(NonZeroU32::MIN),
        }
    }

    fn repository_budget(self, max_results: usize) -> QueryBudget {
        QueryBudget::new(
            NonZeroU32::new(u32::try_from(max_results.max(1)).unwrap_or(u32::MAX))
                .unwrap_or(NonZeroU32::MIN),
            NonZeroU64::new(self.max_bytes.max(1)).unwrap_or(NonZeroU64::MIN),
            NonZeroU32::new(self.max_depth.max(1)).unwrap_or(NonZeroU32::MIN),
            NonZeroU64::new(self.max_duration_ms.max(1)).unwrap_or(NonZeroU64::MIN),
            NonZeroU32::new(u32::try_from(self.max_diagnostics.max(1)).unwrap_or(u32::MAX))
                .unwrap_or(NonZeroU32::MIN),
        )
    }

    fn remaining(self, started: Instant) -> Result<Self, MemoryQueryError> {
        let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if elapsed >= self.max_duration_ms {
            return Err(MemoryQueryError::BudgetExceeded(
                MemoryTruncationReason::Duration,
            ));
        }

        Ok(Self {
            max_duration_ms: self.max_duration_ms - elapsed,
            ..self
        })
    }

    fn with_max_depth(self, max_depth: u32) -> Self {
        Self { max_depth, ..self }
    }
}

/// Stateless federation coordinator. Stores remain independently revisioned.
pub struct FederatedContextService<'a, R, M, L>
where
    R: GraphQuery + ?Sized,
    M: MemoryQuery + ?Sized,
    L: MemoryLinkStore + ?Sized,
{
    repository: &'a R,
    memory: &'a M,
    links: &'a L,
    limits: QueryLimitsConfig,
    memory_freshness: Option<MemoryFreshnessComparison>,
}

impl<'a, R, M, L> FederatedContextService<'a, R, M, L>
where
    R: GraphQuery + ?Sized,
    M: MemoryQuery + ?Sized,
    L: MemoryLinkStore + ?Sized,
{
    pub fn new(
        repository: &'a R,
        memory: &'a M,
        links: &'a L,
        limits: QueryLimitsConfig,
        memory_freshness: Option<MemoryFreshnessComparison>,
    ) -> Self {
        Self {
            repository,
            memory,
            links,
            limits,
            memory_freshness,
        }
    }

    fn validate_scope(&self, wire_version: u32) -> Result<(), MemoryQueryError> {
        if wire_version == FEDERATION_WIRE_VERSION {
            Ok(())
        } else {
            Err(backend_error(
                const {
                    crate::project_memory::diagnostics::MemoryDiagnosticCode::from_static(
                        "federation.wireversion",
                    )
                },
            ))
        }
    }

    fn repository_search(
        &self,
        project_request: &FederatedSearchRequest,
        target: &RepositoryContextTarget,
        budget: EffectiveBudget,
        desired: usize,
    ) -> Result<(RepositoryDomainState, Vec<FederatedSearchResult>, bool), MemoryQueryError> {
        let started = Instant::now();
        let mut request = SearchRequest {
            scope: QueryScope::current(
                target.repository.clone(),
                target.snapshot.clone(),
                budget.repository_budget(budget.max_results),
            ),
            text: project_request.text.as_str().to_string(),
            node_kinds: project_request
                .repository_kinds
                .iter()
                .map(|kind| kind.as_str().to_string())
                .collect(),
            paths: project_request.repository_paths.clone(),
            page: PageRequest { cursor: None },
        };
        let mut state = None;
        let mut results = Vec::new();
        let mut backend_has_more = false;
        loop {
            let remaining = match budget.remaining(started) {
                Ok(remaining) => remaining,
                Err(_) if state.is_some() => break,
                Err(error) => return Err(error),
            };
            request.scope.budget = remaining.repository_budget(budget.max_results);
            let response = self.repository.search(&request).map_err(graph_error)?;
            let next_cursor = response.page.next_cursor.clone();
            backend_has_more = next_cursor.is_some();
            let returned = response.data.hits.len();
            state = Some(repository_state_from_search(&response));
            results.extend(
                response
                    .data
                    .hits
                    .into_iter()
                    .map(FederatedSearchResult::Repository),
            );
            if results.len() >= desired || next_cursor.is_none() || returned == 0 {
                break;
            }

            if budget.remaining(started).is_err() {
                break;
            }

            request.page.cursor = next_cursor;
        }

        Ok((
            state.ok_or_else(|| {
                backend_error(
                    const {
                        crate::project_memory::diagnostics::MemoryDiagnosticCode::from_static(
                            "federation.repositoryquery",
                        )
                    },
                )
            })?,
            results,
            backend_has_more,
        ))
    }

    fn memory_search(
        &self,
        project_request: &FederatedSearchRequest,
        selector: &MemoryRevisionSelector,
        budget: EffectiveBudget,
        desired: usize,
    ) -> Result<(MemoryDomainState, Vec<FederatedSearchResult>, bool), MemoryQueryError> {
        let started = Instant::now();
        let mut scope = MemoryQueryScope::current(
            project_request.scope.project.clone(),
            selector.clone(),
            budget.memory_budget(budget.max_results),
        );
        scope.freshness_comparison = self.memory_freshness.clone();
        let mut request = MemorySearchRequest {
            scope,
            text: project_request.text.clone(),
            entity_kinds: project_request.memory_kinds.clone(),
            source_categories: project_request.memory_sources.clone(),
            page: MemoryPageRequest::default(),
        };
        let mut state = None;
        let mut results = Vec::new();
        let mut backend_has_more = false;
        loop {
            let remaining = match budget.remaining(started) {
                Ok(remaining) => remaining,
                Err(_) if state.is_some() => break,
                Err(error) => return Err(error),
            };
            request.scope.budget = remaining.memory_budget(budget.max_results);
            let response = self.memory.search(request.clone())?;
            let next_cursor = response.page.next_cursor.clone();
            backend_has_more = next_cursor.is_some();
            let returned = response.hits.len();
            state = Some(memory_state_from_search(&response));
            results.extend(response.hits.into_iter().map(FederatedSearchResult::Memory));
            if results.len() >= desired || next_cursor.is_none() || returned == 0 {
                break;
            }

            if budget.remaining(started).is_err() {
                break;
            }

            request.page.cursor = next_cursor;
        }

        Ok((
            state.ok_or_else(|| {
                backend_error(
                    const {
                        crate::project_memory::diagnostics::MemoryDiagnosticCode::from_static(
                            "federation.memoryquery",
                        )
                    },
                )
            })?,
            results,
            backend_has_more,
        ))
    }

    fn repository_status(
        &self,
        target: &RepositoryContextTarget,
        budget: EffectiveBudget,
    ) -> Result<StatusResponse, MemoryQueryError> {
        self.repository
            .status(&StatusRequest {
                scope: QueryScope::current(
                    target.repository.clone(),
                    target.snapshot.clone(),
                    budget.repository_budget(budget.max_results),
                ),
            })
            .map_err(graph_error)
    }

    fn memory_status(
        &self,
        project: &super::domain::ProjectRef,
        selector: &MemoryRevisionSelector,
        budget: EffectiveBudget,
    ) -> Result<MemoryStatusResponse, MemoryQueryError> {
        let mut scope = MemoryQueryScope::current(
            project.clone(),
            selector.clone(),
            budget.memory_budget(budget.max_results),
        );
        scope.freshness_comparison = self.memory_freshness.clone();
        self.memory.status(MemoryStatusRequest { scope })
    }

    fn repository_context(
        &self,
        target: &RepositoryContextTarget,
        request: &FederatedContextRequest,
        seeds: Vec<ContextSeed>,
        budget: EffectiveBudget,
        desired: usize,
    ) -> Result<Option<ContextResponse>, MemoryQueryError> {
        if seeds.is_empty() {
            return Ok(None);
        }

        let started = Instant::now();
        let mut request = ContextRequest {
            scope: QueryScope::current(
                target.repository.clone(),
                target.snapshot.clone(),
                budget.repository_budget(budget.max_results),
            ),
            seeds: dedup(seeds)?,
            policy: request.repository_policy.clone(),
            page: PageRequest { cursor: None },
        };
        let mut aggregate = None;
        let mut items = Vec::new();
        let mut snippets = Vec::new();
        loop {
            let remaining = match budget.remaining(started) {
                Ok(remaining) => remaining,
                Err(_) if aggregate.is_some() => break,
                Err(error) => return Err(error),
            };
            request.scope.budget = remaining.repository_budget(budget.max_results);
            let response = self.repository.context(&request).map_err(graph_error)?;
            let next_cursor = response.page.next_cursor.clone();
            let returned = response.data.items.len();
            items.extend(response.data.items.iter().cloned());
            snippets.extend(response.data.snippets.iter().cloned());
            aggregate = Some(response);
            if items.len() >= desired || next_cursor.is_none() || returned == 0 {
                break;
            }

            if budget.remaining(started).is_err() {
                break;
            }

            request.page.cursor = next_cursor;
        }

        let mut response = aggregate.ok_or_else(|| {
            backend_error(
                const {
                    crate::project_memory::diagnostics::MemoryDiagnosticCode::from_static(
                        "federation.repositoryquery",
                    )
                },
            )
        })?;
        response.data = ContextData { items, snippets };
        if budget.max_depth == 0 {
            response.data.items.retain(|item| {
                item.selection_reasons
                    .iter()
                    .any(|reason| reason.kind == ContextSelectionKind::ExactSeed)
            });
            let seed_paths = response
                .data
                .items
                .iter()
                .map(|item| item.path.clone())
                .collect::<BTreeSet<_>>();
            response
                .data
                .snippets
                .retain(|snippet| seed_paths.contains(&snippet.path));
            if let Some(truncation) = &mut response.page.truncation {
                truncation.explored_depth = 0;
            }
        }

        Ok(Some(response))
    }

    fn memory_context(
        &self,
        project: &super::domain::ProjectRef,
        selector: &MemoryRevisionSelector,
        request: &FederatedContextRequest,
        seeds: Vec<MemoryContextSeed>,
        budget: EffectiveBudget,
        desired: usize,
    ) -> Result<Option<MemoryContextResponse>, MemoryQueryError> {
        if seeds.is_empty() {
            return Ok(None);
        }

        let mut scope = MemoryQueryScope::current(
            project.clone(),
            selector.clone(),
            budget.memory_budget(budget.max_results),
        );
        scope.freshness_comparison = self.memory_freshness.clone();
        let started = Instant::now();
        let mut request = MemoryContextRequest {
            scope,
            seeds: dedup(seeds)?,
            policy: request.memory_policy.clone(),
            page: MemoryPageRequest::default(),
        };
        let mut aggregate = None;
        let mut items = Vec::new();
        let mut relationships = BTreeMap::new();
        loop {
            let remaining = match budget.remaining(started) {
                Ok(remaining) => remaining,
                Err(_) if aggregate.is_some() => break,
                Err(error) => return Err(error),
            };
            request.scope.budget = remaining.memory_budget(budget.max_results);
            let response = self.memory.context(request.clone())?;
            let next_cursor = response.page.next_cursor.clone();
            let returned = response.items.len();
            items.extend(response.items.iter().cloned());
            relationships.extend(
                response
                    .relationships
                    .iter()
                    .cloned()
                    .map(|relationship| (relationship.id.clone(), relationship)),
            );
            aggregate = Some(response);
            if items.len() >= desired || next_cursor.is_none() || returned == 0 {
                break;
            }

            if budget.remaining(started).is_err() {
                break;
            }

            request.page.cursor = next_cursor;
        }

        let mut response = aggregate.ok_or_else(|| {
            backend_error(
                const {
                    crate::project_memory::diagnostics::MemoryDiagnosticCode::from_static(
                        "federation.memoryquery",
                    )
                },
            )
        })?;
        response.items = items;
        response.relationships = relationships.into_values().collect();
        if budget.max_depth == 0 {
            response.items.retain(|item| {
                item.selection_reasons
                    .iter()
                    .any(|reason| reason.as_str() == "context.seed")
            });
            response.relationships.clear();
            if let Some(truncation) = &mut response.page.truncation {
                truncation.explored_depth = 0;
            }
        }

        Ok(Some(response))
    }

    fn exact_links(
        &self,
        revision_id: &MemoryRevisionId,
        build_id: Option<&MemoryBuildId>,
        target: &RepositoryContextTarget,
        snapshot_id: Option<&crate::repository_graph::domain::SnapshotId>,
        max_duration_ms: u64,
        max_diagnostics: usize,
    ) -> Result<(Vec<MemoryRelationship>, Vec<MemoryDiagnostic>, bool, bool), MemoryQueryError>
    {
        let Some(link_set) = self
            .links
            .repository_link_set_for_snapshot(revision_id, &target.repository, snapshot_id)
            .map_err(|_| {
                backend_error(
                    const {
                        crate::project_memory::diagnostics::MemoryDiagnosticCode::from_static(
                            "federation.linksunavailable",
                        )
                    },
                )
            })?
        else {
            let diagnostics = build_id
                .map(|build_id| MemoryDiagnostic {
                    build_id: build_id.clone(),
                    revision_id: revision_id.clone(),
                    severity: MemoryDiagnosticSeverity::Warning,
                    code: diagnostic_code(
                        const {
                            crate::project_memory::diagnostics::MemoryDiagnosticCode::from_static(
                                "federation.linksunavailable",
                            )
                        },
                    ),
                    source_category: None,
                    entity_id: None,
                    relationship_id: None,
                    metrics: Default::default(),
                })
                .into_iter()
                .collect();
            return Ok((vec![], diagnostics, false, false));
        };
        let bounded = self
            .links
            .bounded_repository_links(
                &link_set.id,
                u32::try_from(MAX_FEDERATION_CANDIDATES).unwrap_or(u32::MAX),
                u32::try_from(max_diagnostics).unwrap_or(u32::MAX),
                max_duration_ms,
            )
            .map_err(|_| {
                backend_error(
                    const {
                        crate::project_memory::diagnostics::MemoryDiagnosticCode::from_static(
                            "federation.linksunavailable",
                        )
                    },
                )
            })?;
        let mut diagnostics = bounded.diagnostics;
        diagnostics.truncate(max_diagnostics);
        Ok((
            bounded.relationships,
            diagnostics,
            bounded.truncated,
            bounded.duration_exceeded,
        ))
    }
}

mod support;
use support::*;

mod context;

#[cfg(test)]
#[path = "federation_service_tests.rs"]
mod tests;
