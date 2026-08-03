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
        ContextSelectionReason, ContextSnippet, PageRequest, QueryError, QueryErrorCode,
        QueryScope, SearchRequest, SearchResponse, StatusRequest, StatusResponse,
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
        MemoryContextRequest, MemoryContextResponse, MemoryContextSeed, MemoryFreshnessComparison,
        MemoryPageRequest, MemoryQueryBudget, MemoryQueryError, MemoryQueryScope,
        MemoryRevisionSelector, MemorySearchRequest, MemorySearchResponse, MemoryStatusRequest,
        MemoryStatusResponse, MemoryTruncation, MemoryTruncationReason,
    },
};

const MAX_FEDERATION_CANDIDATES: usize = 4_096;
const CURSOR_VERSION: u32 = 1;

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
                .expect("federation result budget is non-zero"),
            max_bytes: NonZeroU64::new(self.max_bytes.max(1)).expect("byte budget is non-zero"),
            max_snippet_bytes: NonZeroU64::new(self.max_snippet_bytes.max(1))
                .expect("snippet budget is non-zero"),
            max_depth: NonZeroU32::new(self.max_depth.max(1)).expect("depth budget is non-zero"),
            max_duration_ms: NonZeroU64::new(self.max_duration_ms.max(1))
                .expect("duration budget is non-zero"),
            max_diagnostics: NonZeroU32::new(
                u32::try_from(self.max_diagnostics.max(1)).unwrap_or(u32::MAX),
            )
            .expect("diagnostic budget is non-zero"),
        }
    }

    fn repository_budget(self, max_results: usize) -> QueryBudget {
        QueryBudget::new(
            NonZeroU32::new(u32::try_from(max_results.max(1)).unwrap_or(u32::MAX))
                .expect("federation result budget is non-zero"),
            NonZeroU64::new(self.max_bytes.max(1)).expect("byte budget is non-zero"),
            NonZeroU32::new(self.max_depth.max(1)).expect("depth budget is non-zero"),
            NonZeroU64::new(self.max_duration_ms.max(1)).expect("duration budget is non-zero"),
            NonZeroU32::new(u32::try_from(self.max_diagnostics.max(1)).unwrap_or(u32::MAX))
                .expect("diagnostic budget is non-zero"),
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
            Err(backend_error("federation.wireversion"))
        }
    }

    fn repository_search(
        &self,
        project_request: &FederatedSearchRequest,
        target: &RepositoryContextTarget,
        budget: EffectiveBudget,
        desired: usize,
    ) -> Result<(RepositoryDomainState, Vec<FederatedSearchResult>), MemoryQueryError> {
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
        loop {
            let remaining = match budget.remaining(started) {
                Ok(remaining) => remaining,
                Err(_) if state.is_some() => break,
                Err(error) => return Err(error),
            };
            request.scope.budget = remaining.repository_budget(budget.max_results);
            let response = self.repository.search(&request).map_err(graph_error)?;
            let next_cursor = response.page.next_cursor.clone();
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
            state.ok_or_else(|| backend_error("federation.repositoryquery"))?,
            results,
        ))
    }

    fn memory_search(
        &self,
        project_request: &FederatedSearchRequest,
        selector: &MemoryRevisionSelector,
        budget: EffectiveBudget,
        desired: usize,
    ) -> Result<(MemoryDomainState, Vec<FederatedSearchResult>), MemoryQueryError> {
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
        loop {
            let remaining = match budget.remaining(started) {
                Ok(remaining) => remaining,
                Err(_) if state.is_some() => break,
                Err(error) => return Err(error),
            };
            request.scope.budget = remaining.memory_budget(budget.max_results);
            let response = self.memory.search(request.clone())?;
            let next_cursor = response.page.next_cursor.clone();
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
            state.ok_or_else(|| backend_error("federation.memoryquery"))?,
            results,
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
        let mut response = aggregate.ok_or_else(|| backend_error("federation.repositoryquery"))?;
        response.data = ContextData { items, snippets };
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
        let mut response = aggregate.ok_or_else(|| backend_error("federation.memoryquery"))?;
        response.items = items;
        response.relationships = relationships.into_values().collect();
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
            .map_err(|_| backend_error("federation.linksunavailable"))?
        else {
            let diagnostics = build_id
                .map(|build_id| MemoryDiagnostic {
                    build_id: build_id.clone(),
                    revision_id: revision_id.clone(),
                    severity: MemoryDiagnosticSeverity::Warning,
                    code: diagnostic_code("federation.linksunavailable"),
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
            .map_err(|_| backend_error("federation.linksunavailable"))?;
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

impl<R, M, L> ContextService for FederatedContextService<'_, R, M, L>
where
    R: GraphQuery + ?Sized,
    M: MemoryQuery + ?Sized,
    L: MemoryLinkStore + ?Sized,
{
    fn search(
        &self,
        request: FederatedSearchRequest,
    ) -> Result<FederatedSearchResponse, MemoryQueryError> {
        self.validate_scope(request.scope.wire_version)?;
        let started = Instant::now();
        let budget = EffectiveBudget::new(&request.scope.budget, &self.limits);
        let fingerprint = request_fingerprint("search", &SearchCursorShape::from(&request))?;
        let pending_cursor = decode_cursor(request.cursor.as_ref(), "search", &fingerprint)?;
        let offset = usize::try_from(pending_cursor.as_ref().map_or(0, |cursor| cursor.offset))
            .map_err(|_| MemoryQueryError::StaleCursor)?;
        let desired = offset
            .saturating_add(budget.max_results)
            .saturating_add(1)
            .min(MAX_FEDERATION_CANDIDATES);
        let mut repository = None;
        let mut memory = None;
        let mut results = Vec::new();
        match &request.scope.target {
            FederatedTarget::Repository { repository: target } => {
                let (state, domain_results) =
                    self.repository_search(&request, target, budget.remaining(started)?, desired)?;
                repository = Some(state);
                results.extend(domain_results);
            }
            FederatedTarget::Memory { memory: selector } => {
                let (state, domain_results) =
                    self.memory_search(&request, selector, budget.remaining(started)?, desired)?;
                memory = Some(state);
                results.extend(domain_results);
            }
            FederatedTarget::All {
                repository: target,
                memory: selector,
            } => {
                let (repository_state, domain_results) =
                    self.repository_search(&request, target, budget.remaining(started)?, desired)?;
                repository = Some(repository_state);
                results.extend(domain_results);
                if started.elapsed().as_millis() >= u128::from(budget.max_duration_ms) {
                    return Err(MemoryQueryError::BudgetExceeded(
                        MemoryTruncationReason::Duration,
                    ));
                }
                let (memory_state, domain_results) =
                    self.memory_search(&request, selector, budget.remaining(started)?, desired)?;
                memory = Some(memory_state);
                results.extend(domain_results);
            }
        }
        results.sort_by(federated_search_order);
        results.dedup_by(|left, right| search_result_key(left) == search_result_key(right));
        let revision_key = revision_key(repository.as_ref(), memory.as_ref());
        validate_cursor_revision(pending_cursor.as_ref(), &revision_key)?;
        if offset > results.len() {
            return Err(MemoryQueryError::StaleCursor);
        }
        let total = results.len();
        let mut candidates = results.into_iter().skip(offset).peekable();
        let mut returned = Vec::new();
        let mut returned_bytes = 0_u64;
        let mut reason = None;
        for result in candidates.by_ref() {
            if returned.len() >= budget.max_results {
                reason = Some(MemoryTruncationReason::Results);
                break;
            }
            if started.elapsed().as_millis() >= u128::from(budget.max_duration_ms) {
                reason = Some(MemoryTruncationReason::Duration);
                break;
            }
            let bytes = serialized_len(&result)?;
            if returned_bytes.saturating_add(bytes) > budget.max_bytes {
                if returned.is_empty() {
                    return Err(MemoryQueryError::BudgetExceeded(
                        MemoryTruncationReason::Bytes,
                    ));
                }
                reason = Some(MemoryTruncationReason::Bytes);
                break;
            }
            returned_bytes = returned_bytes.saturating_add(bytes);
            returned.push(result);
        }
        let has_more = offset + returned.len() < total || candidates.peek().is_some();
        let page = federated_page(
            reason,
            returned.len(),
            returned_bytes,
            0,
            has_more,
            "search",
            &fingerprint,
            &revision_key,
            offset + returned.len(),
        )?;
        Ok(FederatedSearchResponse {
            wire_version: FEDERATION_WIRE_VERSION,
            project: request.scope.project,
            requested_domain: request.scope.target.domain(),
            repository,
            memory,
            federation_diagnostics: vec![],
            page,
            results: returned,
        })
    }

    fn context(
        &self,
        request: FederatedContextRequest,
    ) -> Result<FederatedContextResponse, MemoryQueryError> {
        self.validate_scope(request.scope.wire_version)?;
        if request.seeds.is_empty() || request.seeds.len() > 32 {
            return Err(backend_error("context.seeds"));
        }
        let started = Instant::now();
        let budget = EffectiveBudget::new(&request.scope.budget, &self.limits);
        let fingerprint = request_fingerprint("context", &ContextCursorShape::from(&request))?;
        let pending_cursor = decode_cursor(request.cursor.as_ref(), "context", &fingerprint)?;
        let offset = usize::try_from(pending_cursor.as_ref().map_or(0, |cursor| cursor.offset))
            .map_err(|_| MemoryQueryError::StaleCursor)?;
        let desired = offset
            .saturating_add(budget.max_results)
            .saturating_add(1)
            .min(MAX_FEDERATION_CANDIDATES);
        let mut repository_state = None;
        let mut memory_state = None;
        let mut repository_response = None;
        let mut memory_response = None;
        let mut cross_links = Vec::new();
        let mut federation_diagnostics = Vec::new();
        let mut link_results_truncated = false;
        let mut link_duration_exceeded = false;

        match &request.scope.target {
            FederatedTarget::Repository { repository: target } => {
                let repository_seeds = repository_only_seeds(&request.seeds)?;
                let response = self
                    .repository_context(
                        target,
                        &request,
                        repository_seeds,
                        budget.remaining(started)?,
                        desired,
                    )?
                    .ok_or_else(|| backend_error("context.seeds"))?;
                repository_state = Some(repository_state_from_context(&response));
                repository_response = Some(response);
            }
            FederatedTarget::Memory { memory: selector } => {
                let memory_seeds = memory_only_seeds(&request.seeds)?;
                let response = self
                    .memory_context(
                        &request.scope.project,
                        selector,
                        &request,
                        memory_seeds,
                        budget.remaining(started)?,
                        desired,
                    )?
                    .ok_or_else(|| backend_error("context.seeds"))?;
                memory_state = Some(memory_state_from_context(&response));
                memory_response = Some(response);
            }
            FederatedTarget::All {
                repository: target,
                memory: selector,
            } => {
                let repository_status =
                    self.repository_status(target, budget.remaining(started)?)?;
                let memory_status = self.memory_status(
                    &request.scope.project,
                    selector,
                    budget.remaining(started)?,
                )?;
                let snapshot_id = repository_status.snapshot_id.as_ref();
                let revision_id = memory_status
                    .revision_id
                    .as_ref()
                    .ok_or(MemoryQueryError::Unavailable)?;
                let remaining = budget.remaining(started)?;
                let (links, diagnostics, links_truncated, links_timed_out) = self.exact_links(
                    revision_id,
                    memory_status.data.build_id.as_ref(),
                    target,
                    snapshot_id,
                    remaining.max_duration_ms,
                    budget.max_diagnostics,
                )?;
                budget.remaining(started)?;
                federation_diagnostics = diagnostics;
                link_results_truncated = links_truncated;
                link_duration_exceeded = links_timed_out;
                let (mut repository_seeds, mut memory_seeds) = split_all_seeds(&request.seeds);
                for seed in &repository_seeds {
                    for relationship in links.iter().filter(|relationship| {
                        relationship.provenance.resolution == MemoryResolutionState::Resolved
                            && repository_seed_matches(seed, &relationship.target, snapshot_id)
                    }) {
                        memory_seeds.push(MemoryContextSeed::Entity(relationship.source.clone()));
                        cross_links.push(relationship.clone());
                    }
                }
                let memory = self.memory_context(
                    &request.scope.project,
                    selector,
                    &request,
                    memory_seeds,
                    budget.remaining(started)?,
                    desired,
                )?;
                if let Some(response) = &memory {
                    let selected = response
                        .items
                        .iter()
                        .filter(|item| {
                            item.selection_reasons
                                .iter()
                                .any(|reason| reason.as_str() == "context.seed")
                        })
                        .map(|item| item.entity.id.clone())
                        .collect::<BTreeSet<_>>();
                    for relationship in links.iter().filter(|relationship| {
                        relationship.provenance.resolution == MemoryResolutionState::Resolved
                            && selected.contains(&relationship.source)
                    }) {
                        if let Some(seed) =
                            repository_seed_from_target(&relationship.target, snapshot_id)
                        {
                            repository_seeds.push(seed);
                            cross_links.push(relationship.clone());
                        }
                    }
                }
                cross_links.sort_by(|left, right| left.id.cmp(&right.id));
                cross_links.dedup_by(|left, right| left.id == right.id);
                let repository = self.repository_context(
                    target,
                    &request,
                    repository_seeds,
                    budget.remaining(started)?,
                    desired,
                )?;
                repository_state = Some(match &repository {
                    Some(response) => repository_state_from_context(response),
                    None => repository_state_from_status(&repository_status),
                });
                memory_state = Some(match &memory {
                    Some(response) => memory_state_from_context(response),
                    None => memory_state_from_status(&memory_status),
                });
                repository_response = repository;
                memory_response = memory;
            }
        }

        let revision_key = revision_key(repository_state.as_ref(), memory_state.as_ref());
        validate_cursor_revision(pending_cursor.as_ref(), &revision_key)?;
        let mut items = Vec::new();
        if let Some(response) = &repository_response {
            items.extend(response.data.items.iter().cloned().map(|mut item| {
                if cross_links
                    .iter()
                    .any(|relationship| repository_item_matches(&item, &relationship.target))
                {
                    item.selection_reasons.push(ContextSelectionReason {
                        kind: ContextSelectionKind::Relationship,
                        via_node: None,
                        via_edge: None,
                    });
                }
                FederatedContextItem::Repository(Box::new(item))
            }));
        }
        if let Some(response) = &memory_response {
            items.extend(response.items.iter().cloned().map(|mut item| {
                if cross_links
                    .iter()
                    .any(|relationship| relationship.source == item.entity.id)
                {
                    item.selection_reasons
                        .push(diagnostic_code("context.repositorylink"));
                    item.selection_reasons.sort();
                    item.selection_reasons.dedup();
                }
                FederatedContextItem::Memory(Box::new(item))
            }));
        }
        items.sort_by(federated_context_order);
        items.dedup_by(|left, right| context_item_key(left) == context_item_key(right));
        if offset > items.len() {
            return Err(MemoryQueryError::StaleCursor);
        }
        let total = items.len();
        let mut candidates = items.into_iter().skip(offset).peekable();
        let mut returned = Vec::new();
        let mut returned_bytes = 0_u64;
        let mut reason = if link_duration_exceeded {
            Some(MemoryTruncationReason::Duration)
        } else if link_results_truncated {
            Some(MemoryTruncationReason::Results)
        } else {
            None
        };
        for item in candidates.by_ref() {
            if returned.len() >= budget.max_results {
                reason = Some(MemoryTruncationReason::Results);
                break;
            }
            if started.elapsed().as_millis() >= u128::from(budget.max_duration_ms) {
                reason = Some(MemoryTruncationReason::Duration);
                break;
            }
            let bytes = serialized_len(&item)?;
            if returned_bytes.saturating_add(bytes) > budget.max_bytes {
                if returned.is_empty() {
                    return Err(MemoryQueryError::BudgetExceeded(
                        MemoryTruncationReason::Bytes,
                    ));
                }
                reason = Some(MemoryTruncationReason::Bytes);
                break;
            }
            returned_bytes = returned_bytes.saturating_add(bytes);
            returned.push(item);
        }
        let mut has_more = offset + returned.len() < total || candidates.peek().is_some();
        let explored_depth = repository_response
            .as_ref()
            .and_then(|response| response.page.truncation.as_ref())
            .map_or(0, |truncation| truncation.explored_depth)
            .max(
                memory_response
                    .as_ref()
                    .and_then(|response| response.page.truncation.as_ref())
                    .map_or(0, |truncation| truncation.explored_depth),
            );
        let returned_memory_ids = returned
            .iter()
            .filter_map(|item| match item {
                FederatedContextItem::Memory(item) => Some(item.entity.id.clone()),
                FederatedContextItem::Repository(_) => None,
            })
            .collect::<BTreeSet<_>>();
        let returned_repository_paths = returned
            .iter()
            .filter_map(|item| match item {
                FederatedContextItem::Repository(item) => Some(item.path.clone()),
                FederatedContextItem::Memory(_) => None,
            })
            .collect::<BTreeSet<_>>();
        let mut memory_relationships = memory_response
            .as_ref()
            .map(|response| response.relationships.clone())
            .unwrap_or_default();
        memory_relationships.retain(|relationship| {
            returned_memory_ids.contains(&relationship.source)
                || matches!(
                    &relationship.target,
                    MemoryRelationshipTarget::MemoryEntity { entity_id }
                        if returned_memory_ids.contains(entity_id)
                )
        });
        memory_relationships.sort_by(|left, right| left.id.cmp(&right.id));
        cross_links.retain(|relationship| returned_memory_ids.contains(&relationship.source));
        let mut repository_snippets = repository_response
            .map(|response| response.data.snippets)
            .unwrap_or_default();
        repository_snippets.retain(|snippet| returned_repository_paths.contains(&snippet.path));
        repository_snippets.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| snippet_span_key(left).cmp(&snippet_span_key(right)))
        });
        federation_diagnostics.truncate(budget.max_diagnostics);
        let mut remaining_bytes = budget.max_bytes.saturating_sub(returned_bytes);
        let mut supplemental_truncated = retain_bounded(
            &mut memory_relationships,
            budget.max_results,
            &mut remaining_bytes,
        )?;
        supplemental_truncated |=
            retain_bounded(&mut cross_links, budget.max_results, &mut remaining_bytes)?;
        supplemental_truncated |= retain_snippets_bounded(
            &mut repository_snippets,
            budget.max_snippet_bytes,
            &mut remaining_bytes,
        )?;
        if supplemental_truncated {
            reason = Some(MemoryTruncationReason::Bytes);
            has_more = true;
        }
        let page = federated_page(
            reason,
            returned.len(),
            budget.max_bytes.saturating_sub(remaining_bytes),
            explored_depth,
            has_more,
            "context",
            &fingerprint,
            &revision_key,
            offset + returned.len(),
        )?;
        Ok(FederatedContextResponse {
            wire_version: FEDERATION_WIRE_VERSION,
            project: request.scope.project,
            requested_domain: request.scope.target.domain(),
            repository: repository_state,
            memory: memory_state,
            federation_diagnostics,
            page,
            items: returned,
            memory_relationships,
            cross_domain_links: cross_links,
            repository_snippets,
        })
    }
}

fn repository_only_seeds(
    seeds: &[FederatedContextSeed],
) -> Result<Vec<ContextSeed>, MemoryQueryError> {
    seeds
        .iter()
        .map(|seed| match seed {
            FederatedContextSeed::Repository(seed) => Ok(seed.clone()),
            _ => Err(backend_error("context.domainmismatch")),
        })
        .collect()
}

fn memory_only_seeds(
    seeds: &[FederatedContextSeed],
) -> Result<Vec<MemoryContextSeed>, MemoryQueryError> {
    seeds
        .iter()
        .map(|seed| match seed {
            FederatedContextSeed::MemoryEntity(id) => Ok(MemoryContextSeed::Entity(id.clone())),
            FederatedContextSeed::Milestone(id) => Ok(MemoryContextSeed::Milestone(id.clone())),
            FederatedContextSeed::Task(id) => Ok(MemoryContextSeed::Task(id.clone())),
            FederatedContextSeed::Run(id) => Ok(MemoryContextSeed::Run(id.clone())),
            FederatedContextSeed::Repository(_) => Err(backend_error("context.domainmismatch")),
        })
        .collect()
}

fn split_all_seeds(seeds: &[FederatedContextSeed]) -> (Vec<ContextSeed>, Vec<MemoryContextSeed>) {
    let mut repository = Vec::new();
    let mut memory = Vec::new();
    for seed in seeds {
        match seed {
            FederatedContextSeed::Repository(seed) => repository.push(seed.clone()),
            FederatedContextSeed::MemoryEntity(id) => {
                memory.push(MemoryContextSeed::Entity(id.clone()));
            }
            FederatedContextSeed::Milestone(id) => {
                memory.push(MemoryContextSeed::Milestone(id.clone()));
            }
            FederatedContextSeed::Task(id) => {
                memory.push(MemoryContextSeed::Task(id.clone()));
            }
            FederatedContextSeed::Run(id) => {
                memory.push(MemoryContextSeed::Run(id.clone()));
            }
        }
    }
    (repository, memory)
}

fn repository_seed_matches(
    seed: &ContextSeed,
    target: &MemoryRelationshipTarget,
    snapshot_id: Option<&crate::repository_graph::domain::SnapshotId>,
) -> bool {
    match (seed, target) {
        (
            ContextSeed::Node(node),
            MemoryRelationshipTarget::RepositoryNode {
                snapshot_id: target_snapshot,
                node_id,
                ..
            },
        ) => Some(target_snapshot) == snapshot_id && node_id == node,
        (
            ContextSeed::Path(path),
            MemoryRelationshipTarget::RepositoryPath {
                path: target_path,
                snapshot_id: target_snapshot,
                ..
            },
        ) => {
            target_path == path
                && target_snapshot
                    .as_ref()
                    .is_none_or(|id| Some(id) == snapshot_id)
        }
        (
            ContextSeed::Symbol(symbol),
            MemoryRelationshipTarget::RepositorySymbol {
                semantic_key,
                snapshot_id: target_snapshot,
                ..
            },
        ) => {
            semantic_key == symbol
                && target_snapshot
                    .as_ref()
                    .is_none_or(|id| Some(id) == snapshot_id)
        }
        _ => false,
    }
}

fn repository_seed_from_target(
    target: &MemoryRelationshipTarget,
    snapshot_id: Option<&crate::repository_graph::domain::SnapshotId>,
) -> Option<ContextSeed> {
    match target {
        MemoryRelationshipTarget::RepositoryNode {
            snapshot_id: target_snapshot,
            node_id,
            ..
        } if Some(target_snapshot) == snapshot_id => Some(ContextSeed::Node(node_id.clone())),
        MemoryRelationshipTarget::RepositoryPath {
            path,
            snapshot_id: target_snapshot,
            ..
        } if target_snapshot
            .as_ref()
            .is_none_or(|id| Some(id) == snapshot_id) =>
        {
            Some(ContextSeed::Path(path.clone()))
        }
        MemoryRelationshipTarget::RepositorySymbol {
            semantic_key,
            snapshot_id: target_snapshot,
            ..
        } if target_snapshot
            .as_ref()
            .is_none_or(|id| Some(id) == snapshot_id) =>
        {
            Some(ContextSeed::Symbol(semantic_key.clone()))
        }
        _ => None,
    }
}

fn repository_item_matches(
    item: &crate::repository_graph::query::ContextItem,
    target: &MemoryRelationshipTarget,
) -> bool {
    match target {
        MemoryRelationshipTarget::RepositoryNode { node_id, .. } => &item.node_id == node_id,
        MemoryRelationshipTarget::RepositoryPath { path, .. } => &item.path == path,
        MemoryRelationshipTarget::RepositorySymbol { semantic_key, .. } => {
            item.semantic_key.as_ref() == Some(semantic_key)
        }
        _ => false,
    }
}

fn repository_state_from_search(response: &SearchResponse) -> RepositoryDomainState {
    RepositoryDomainState {
        repository: response.repository.clone(),
        snapshot_id: Some(response.snapshot_id.clone()),
        task_view: response.task_view.clone(),
        freshness: response.freshness.clone(),
        diagnostics: response.diagnostics.clone(),
        page: response.page.clone(),
    }
}

fn repository_state_from_context(response: &ContextResponse) -> RepositoryDomainState {
    RepositoryDomainState {
        repository: response.repository.clone(),
        snapshot_id: Some(response.snapshot_id.clone()),
        task_view: response.task_view.clone(),
        freshness: response.freshness.clone(),
        diagnostics: response.diagnostics.clone(),
        page: response.page.clone(),
    }
}

fn repository_state_from_status(response: &StatusResponse) -> RepositoryDomainState {
    RepositoryDomainState {
        repository: response.repository.clone(),
        snapshot_id: response.snapshot_id.clone(),
        task_view: response.task_view.clone(),
        freshness: response.freshness.clone(),
        diagnostics: response.diagnostics.clone(),
        page: response.page.clone(),
    }
}

fn memory_state_from_search(response: &MemorySearchResponse) -> MemoryDomainState {
    MemoryDomainState {
        revision_id: Some(response.revision_id.clone()),
        freshness: response.freshness.clone(),
        authorized_sources: authorized_sources(),
        diagnostics: response.diagnostics.clone(),
        page: FederatedPageInfo {
            next_cursor: None,
            truncation: response.page.truncation.clone(),
        },
    }
}

fn memory_state_from_context(response: &MemoryContextResponse) -> MemoryDomainState {
    MemoryDomainState {
        revision_id: Some(response.revision_id.clone()),
        freshness: response.freshness.clone(),
        authorized_sources: authorized_sources(),
        diagnostics: response.diagnostics.clone(),
        page: FederatedPageInfo {
            next_cursor: None,
            truncation: response.page.truncation.clone(),
        },
    }
}

fn memory_state_from_status(response: &MemoryStatusResponse) -> MemoryDomainState {
    MemoryDomainState {
        revision_id: response.revision_id.clone(),
        freshness: response.freshness.clone(),
        authorized_sources: response
            .data
            .source_policy
            .iter()
            .filter_map(|status| status.policy.enabled.then_some(status.category))
            .collect(),
        diagnostics: response.diagnostics.clone(),
        page: FederatedPageInfo::default(),
    }
}

fn authorized_sources() -> Vec<MemorySourceCategory> {
    MemoryPolicy::default().authorized_categories().collect()
}

fn graph_error(error: QueryError) -> MemoryQueryError {
    match error.code {
        QueryErrorCode::NotBuilt | QueryErrorCode::IndexBuilding | QueryErrorCode::IndexFailed => {
            MemoryQueryError::Unavailable
        }
        QueryErrorCode::SnapshotNotFound => MemoryQueryError::RevisionNotFound,
        QueryErrorCode::StaleCursor => MemoryQueryError::StaleCursor,
        QueryErrorCode::BudgetExceeded => {
            MemoryQueryError::BudgetExceeded(MemoryTruncationReason::Duration)
        }
        QueryErrorCode::ContentChanged => MemoryQueryError::ContentChanged,
        _ => backend_error("federation.repositoryquery"),
    }
}

fn backend_error(value: &str) -> MemoryQueryError {
    MemoryQueryError::Backend(diagnostic_code(value))
}

fn diagnostic_code(value: &str) -> MemoryDiagnosticCode {
    MemoryDiagnosticCode::new(value).expect("static federation diagnostic code is valid")
}

fn serialized_len(value: &impl Serialize) -> Result<u64, MemoryQueryError> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len() as u64)
        .map_err(|_| backend_error("federation.serialization"))
}

fn snippet_span_key(snippet: &ContextSnippet) -> (u64, u64) {
    snippet.span.as_ref().map_or((0, 0), |span| {
        (span.start.byte_offset, span.end.byte_offset)
    })
}

fn retain_bounded<T: Serialize>(
    values: &mut Vec<T>,
    max_items: usize,
    remaining_bytes: &mut u64,
) -> Result<bool, MemoryQueryError> {
    let original_len = values.len();
    let mut retained = Vec::new();
    for value in values.drain(..) {
        if retained.len() >= max_items {
            break;
        }
        let bytes = serialized_len(&value)?;
        if bytes > *remaining_bytes {
            break;
        }
        *remaining_bytes -= bytes;
        retained.push(value);
    }
    let truncated = retained.len() < original_len;
    *values = retained;
    Ok(truncated)
}

fn retain_snippets_bounded(
    snippets: &mut Vec<ContextSnippet>,
    max_snippet_bytes: u64,
    remaining_bytes: &mut u64,
) -> Result<bool, MemoryQueryError> {
    let original_len = snippets.len();
    let mut retained = Vec::new();
    let mut snippet_bytes = 0_u64;
    for snippet in snippets.drain(..) {
        let text_bytes = snippet.text.len() as u64;
        let bytes = serialized_len(&snippet)?;
        if snippet_bytes.saturating_add(text_bytes) > max_snippet_bytes || bytes > *remaining_bytes
        {
            break;
        }
        snippet_bytes = snippet_bytes.saturating_add(text_bytes);
        *remaining_bytes -= bytes;
        retained.push(snippet);
    }
    let truncated = retained.len() < original_len;
    *snippets = retained;
    Ok(truncated)
}

fn dedup<T>(values: Vec<T>) -> Result<Vec<T>, MemoryQueryError>
where
    T: Serialize,
{
    let mut values = values
        .into_iter()
        .map(|value| {
            serde_json::to_string(&value)
                .map(|key| (key, value))
                .map_err(|_| backend_error("federation.serialization"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    values.sort_by(|left, right| left.0.cmp(&right.0));
    values.dedup_by(|left, right| left.0 == right.0);
    Ok(values.into_iter().map(|(_, value)| value).collect())
}

fn federated_search_order(
    left: &FederatedSearchResult,
    right: &FederatedSearchResult,
) -> std::cmp::Ordering {
    search_score(right)
        .total_cmp(&search_score(left))
        .then_with(|| search_domain_rank(left).cmp(&search_domain_rank(right)))
        .then_with(|| search_result_key(left).cmp(&search_result_key(right)))
}

fn search_score(result: &FederatedSearchResult) -> f64 {
    match result {
        FederatedSearchResult::Repository(hit) => hit.score,
        FederatedSearchResult::Memory(hit) => hit.score,
    }
}

fn search_domain_rank(result: &FederatedSearchResult) -> u8 {
    match result {
        FederatedSearchResult::Repository(_) => 0,
        FederatedSearchResult::Memory(_) => 1,
    }
}

fn search_result_key(result: &FederatedSearchResult) -> String {
    match result {
        FederatedSearchResult::Repository(hit) => format!("repository:{}", hit.node_id.as_str()),
        FederatedSearchResult::Memory(hit) => format!("memory:{}", hit.entity.id.as_str()),
    }
}

fn federated_context_order(
    left: &FederatedContextItem,
    right: &FederatedContextItem,
) -> std::cmp::Ordering {
    context_rank(left)
        .cmp(&context_rank(right))
        .then_with(|| context_item_key(left).cmp(&context_item_key(right)))
}

fn context_rank(item: &FederatedContextItem) -> (u8, u8) {
    match item {
        FederatedContextItem::Repository(item) => {
            let exact = item
                .selection_reasons
                .iter()
                .any(|reason| reason.kind == ContextSelectionKind::ExactSeed);
            (u8::from(!exact), 0)
        }
        FederatedContextItem::Memory(item) => {
            let exact = item
                .selection_reasons
                .iter()
                .any(|reason| reason.as_str() == "context.seed");
            (u8::from(!exact), 1)
        }
    }
}

fn context_item_key(item: &FederatedContextItem) -> String {
    match item {
        FederatedContextItem::Repository(item) => {
            format!("repository:{}", item.node_id.as_str())
        }
        FederatedContextItem::Memory(item) => format!("memory:{}", item.entity.id.as_str()),
    }
}

fn revision_key(
    repository: Option<&RepositoryDomainState>,
    memory: Option<&MemoryDomainState>,
) -> String {
    hash(&(
        repository.and_then(|state| state.snapshot_id.as_ref().map(|id| id.as_str())),
        repository
            .and_then(|state| state.task_view.as_ref())
            .map(|task_view| task_view.task_view_id.as_str()),
        repository
            .and_then(|state| state.task_view.as_ref())
            .and_then(|task_view| task_view.overlay_revision_id.as_ref())
            .map(|id| id.as_str()),
        memory.and_then(|state| state.revision_id.as_ref().map(|id| id.as_str())),
    ))
    .expect("federation revision identity is serializable")
}

#[derive(Serialize)]
struct SearchCursorShape<'a> {
    project: &'a super::domain::ProjectRef,
    target: &'a FederatedTarget,
    text: String,
    repository_kinds: Vec<String>,
    repository_paths: Vec<String>,
    memory_kinds: Vec<String>,
    memory_sources: Vec<String>,
}

impl<'a> From<&'a FederatedSearchRequest> for SearchCursorShape<'a> {
    fn from(request: &'a FederatedSearchRequest) -> Self {
        let mut repository_kinds = request
            .repository_kinds
            .iter()
            .map(|kind| kind.as_str().to_string())
            .collect::<Vec<_>>();
        repository_kinds.sort();
        repository_kinds.dedup();
        let mut repository_paths = request
            .repository_paths
            .iter()
            .map(|path| path.as_str().to_string())
            .collect::<Vec<_>>();
        repository_paths.sort();
        repository_paths.dedup();
        let mut memory_kinds = request
            .memory_kinds
            .iter()
            .map(|kind| kind.as_str().to_string())
            .collect::<Vec<_>>();
        memory_kinds.sort();
        memory_kinds.dedup();
        let mut memory_sources = request
            .memory_sources
            .iter()
            .map(|source| format!("{source:?}"))
            .collect::<Vec<_>>();
        memory_sources.sort();
        memory_sources.dedup();
        Self {
            project: &request.scope.project,
            target: &request.scope.target,
            text: request.text.as_str().trim().to_lowercase(),
            repository_kinds,
            repository_paths,
            memory_kinds,
            memory_sources,
        }
    }
}

#[derive(Serialize)]
struct ContextCursorShape<'a> {
    project: &'a super::domain::ProjectRef,
    target: &'a FederatedTarget,
    seeds: Vec<String>,
    repository_policy: &'a crate::repository_graph::query::ContextPolicy,
    memory_policy: &'a super::query::MemoryContextPolicy,
    max_depth: u32,
}

impl<'a> From<&'a FederatedContextRequest> for ContextCursorShape<'a> {
    fn from(request: &'a FederatedContextRequest) -> Self {
        let mut seeds = request
            .seeds
            .iter()
            .map(|seed| serde_json::to_string(seed).expect("federation seed is serializable"))
            .collect::<Vec<_>>();
        seeds.sort();
        seeds.dedup();
        Self {
            project: &request.scope.project,
            target: &request.scope.target,
            seeds,
            repository_policy: &request.repository_policy,
            memory_policy: &request.memory_policy,
            max_depth: request.scope.budget.max_depth.get(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct FederationCursorPayload {
    version: u32,
    operation: String,
    fingerprint: String,
    revision_key: String,
    offset: u64,
}

fn request_fingerprint(
    operation: &str,
    value: &impl Serialize,
) -> Result<String, MemoryQueryError> {
    hash(&(operation, value))
}

fn hash(value: &impl Serialize) -> Result<String, MemoryQueryError> {
    let encoded =
        serde_json::to_vec(value).map_err(|_| backend_error("federation.serialization"))?;
    let mut hasher = Sha256::new();
    hasher.update(b"ferrus.project-memory.federation.v1\0");
    hasher.update(encoded);
    Ok(hex(&hasher.finalize()))
}

fn encode_cursor(
    operation: &str,
    fingerprint: &str,
    revision_key: &str,
    offset: usize,
) -> Result<FederationPageCursor, MemoryQueryError> {
    let payload = FederationCursorPayload {
        version: CURSOR_VERSION,
        operation: operation.to_string(),
        fingerprint: fingerprint.to_string(),
        revision_key: revision_key.to_string(),
        offset: u64::try_from(offset).unwrap_or(u64::MAX),
    };
    let bytes =
        serde_json::to_vec(&payload).map_err(|_| backend_error("federation.serialization"))?;
    FederationPageCursor::new(format!("cursor:{}", hex(&bytes)))
        .map_err(|_| backend_error("federation.cursor"))
}

fn decode_cursor(
    cursor: Option<&FederationPageCursor>,
    operation: &str,
    fingerprint: &str,
) -> Result<Option<FederationCursorPayload>, MemoryQueryError> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    let encoded = cursor
        .as_str()
        .strip_prefix("cursor:")
        .ok_or(MemoryQueryError::StaleCursor)?;
    if encoded.len() > 4_096 || encoded.len() % 2 != 0 {
        return Err(MemoryQueryError::StaleCursor);
    }
    let bytes = unhex(encoded).ok_or(MemoryQueryError::StaleCursor)?;
    let payload: FederationCursorPayload =
        serde_json::from_slice(&bytes).map_err(|_| MemoryQueryError::StaleCursor)?;
    if payload.version != CURSOR_VERSION
        || payload.operation != operation
        || payload.fingerprint != fingerprint
    {
        return Err(MemoryQueryError::StaleCursor);
    }
    Ok(Some(payload))
}

fn validate_cursor_revision(
    cursor: Option<&FederationCursorPayload>,
    revision_key: &str,
) -> Result<(), MemoryQueryError> {
    if cursor.is_some_and(|cursor| cursor.revision_key != revision_key) {
        Err(MemoryQueryError::StaleCursor)
    } else {
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn federated_page(
    reason: Option<MemoryTruncationReason>,
    returned_results: usize,
    returned_bytes: u64,
    explored_depth: u32,
    has_more: bool,
    operation: &str,
    fingerprint: &str,
    revision_key: &str,
    offset: usize,
) -> Result<FederatedPageInfo, MemoryQueryError> {
    let truncation = reason.map(|reason| MemoryTruncation {
        reason,
        returned_results: u32::try_from(returned_results).unwrap_or(u32::MAX),
        returned_bytes,
        explored_depth,
    });
    let next_cursor = (has_more
        && returned_results > 0
        && matches!(
            reason,
            Some(MemoryTruncationReason::Results | MemoryTruncationReason::Bytes)
        ))
    .then(|| encode_cursor(operation, fingerprint, revision_key, offset))
    .transpose()?;
    Ok(FederatedPageInfo {
        next_cursor,
        truncation,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(value: &str) -> Option<Vec<u8>> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some(((high << 4) | low) as u8)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, process::Command};

    use tempfile::TempDir;

    use crate::project_memory::federation::ContextDomain;
    use crate::{
        project_memory::{
            domain::{MemoryQueryText, MemoryViewName, ProjectId, ProjectNamespace, ProjectRef},
            index::{MemoryIndexOptions, MemoryIndexer},
            policy::MemoryPolicy,
            ports::MemorySource,
            query::{MemoryContextPolicy, MemoryPageRequest},
            query_sqlite::{SqliteMemoryQuery, default_budget as default_memory_budget},
            source::LocalMemorySource,
            sqlite::MemorySidecar,
        },
        repository_graph::{
            config::RepositoryGraphConfig,
            domain::{
                BuildId, PublishedViewName, RepoPath, RepositoryId, RepositoryNamespace,
                RepositoryRef,
            },
            index::{IndexCoordinator, IndexRequest, active_extractor_identities},
            query::{ContextPolicy, EdgeDirection, SnapshotSelector},
            query_sqlite::{FreshnessComparison, SqliteGraphQuery},
            source::{FilesystemRepositorySource, SourceDiscoveryContext},
            sqlite::{OpenSidecarResult, open_for_build_at},
        },
    };

    use super::*;

    struct Fixture {
        _root: TempDir,
        _data: TempDir,
        project: ProjectRef,
        repository: RepositoryRef,
        graph_sidecar: crate::repository_graph::sqlite::Sidecar,
        memory_sidecar: MemorySidecar,
        config: RepositoryGraphConfig,
        graph_freshness: FreshnessComparison,
        memory_freshness: MemoryFreshnessComparison,
    }

    fn project() -> ProjectRef {
        ProjectRef {
            namespace: ProjectNamespace::new("local:test").unwrap(),
            project_id: ProjectId::new("federation-project").unwrap(),
        }
    }

    fn repository() -> RepositoryRef {
        RepositoryRef {
            namespace: RepositoryNamespace::new("local:federation-project").unwrap(),
            repository_id: RepositoryId::new("root").unwrap(),
        }
    }

    fn initialize(root: &Path) {
        assert!(
            Command::new("git")
                .arg("init")
                .arg(root)
                .status()
                .unwrap()
                .success()
        );
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("docs/specs")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"federation-fixture\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub struct ContextService;\npub fn federated_context() {}\n",
        )
        .unwrap();
        fs::write(
            root.join("docs/specs/federation.md"),
            "# Federation\n\n- [x] #4.4 Context\n\nID: rg4.4-test\nDepends on: none\n\n## Outcome\n\nImplemented `path:src/lib.rs` with bounded retrieval.\n",
        )
        .unwrap();
        assert!(
            Command::new("git")
                .current_dir(root)
                .args([
                    "add",
                    "--",
                    "Cargo.toml",
                    "src/lib.rs",
                    "docs/specs/federation.md"
                ])
                .status()
                .unwrap()
                .success()
        );
    }

    fn fixture() -> Fixture {
        let root = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        initialize(root.path());
        let project = project();
        let repository = repository();
        let config = RepositoryGraphConfig::default();
        let identities = active_extractor_identities(&config).unwrap();
        let discovery =
            SourceDiscoveryContext::from_config(repository.clone(), &config, &identities).unwrap();
        let repository_source =
            FilesystemRepositorySource::discover(root.path(), discovery).unwrap();
        let graph_freshness = FreshnessComparison::from_manifest(repository_source.manifest());
        let OpenSidecarResult::Ready(mut graph_sidecar) =
            open_for_build_at(&data.path().join("repo-graph.db")).unwrap()
        else {
            panic!("new graph sidecar should be writable");
        };
        IndexCoordinator::new(&mut graph_sidecar)
            .index(
                &repository_source,
                &config,
                IndexRequest {
                    build_id: BuildId::new("federation-build").unwrap(),
                    view_name: PublishedViewName::new("canonical").unwrap(),
                    force_full: false,
                },
            )
            .unwrap();

        let memory_source = LocalMemorySource::discover_at(
            root.path().to_path_buf(),
            data.path().to_path_buf(),
            project.clone(),
            RepoPath::new("docs/specs").unwrap(),
            MemoryPolicy::default(),
        )
        .unwrap();
        let memory_freshness =
            MemoryFreshnessComparison::from_manifest(&memory_source.manifest().unwrap());
        let mut memory_sidecar = MemorySidecar::open_at(data.path()).unwrap();
        MemoryIndexer::new(&memory_source, &mut memory_sidecar)
            .unwrap()
            .index(MemoryIndexOptions::default())
            .unwrap();
        Fixture {
            _root: root,
            _data: data,
            project,
            repository,
            graph_sidecar,
            memory_sidecar,
            config,
            graph_freshness,
            memory_freshness,
        }
    }

    fn budget(config: &RepositoryGraphConfig) -> MemoryQueryBudget {
        default_memory_budget(&config.query_limits).unwrap()
    }

    fn target(fixture: &Fixture, domain: ContextDomain) -> FederatedTarget {
        let repository = RepositoryContextTarget {
            repository: fixture.repository.clone(),
            snapshot: SnapshotSelector::Published(PublishedViewName::new("canonical").unwrap()),
        };
        let memory = MemoryRevisionSelector::Published(MemoryViewName::new("project").unwrap());
        match domain {
            ContextDomain::Repository => FederatedTarget::Repository { repository },
            ContextDomain::Memory => FederatedTarget::Memory { memory },
            ContextDomain::All => FederatedTarget::All { repository, memory },
        }
    }

    fn service<'a>(
        fixture: &'a Fixture,
        graph: &'a SqliteGraphQuery<'a>,
        memory: &'a SqliteMemoryQuery<'a>,
    ) -> FederatedContextService<'a, SqliteGraphQuery<'a>, SqliteMemoryQuery<'a>, MemorySidecar>
    {
        FederatedContextService::new(
            graph,
            memory,
            &fixture.memory_sidecar,
            fixture.config.query_limits.clone(),
            Some(fixture.memory_freshness.clone()),
        )
    }

    #[test]
    fn cursor_is_bound_to_both_domain_revisions() {
        let cursor = encode_cursor("search", "request", "revision-one", 4).unwrap();
        let decoded = decode_cursor(Some(&cursor), "search", "request")
            .unwrap()
            .unwrap();
        assert_eq!(
            validate_cursor_revision(Some(&decoded), "revision-two"),
            Err(MemoryQueryError::StaleCursor)
        );
    }

    #[test]
    fn explicit_domain_seed_validation_never_broadens_scope() {
        let seed = FederatedContextSeed::MemoryEntity(
            super::super::domain::MemoryEntityId::new("memory-entity").unwrap(),
        );
        assert!(repository_only_seeds(&[seed]).is_err());
    }

    #[test]
    fn repository_and_memory_search_scopes_remain_explicit() {
        let fixture = fixture();
        let graph = SqliteGraphQuery::new(
            &fixture.graph_sidecar,
            fixture.config.query_limits.clone(),
            Some(fixture.graph_freshness.clone()),
        );
        let memory =
            SqliteMemoryQuery::new(&fixture.memory_sidecar, fixture.config.query_limits.clone());
        let service = service(&fixture, &graph, &memory);
        let request = |domain| FederatedSearchRequest {
            scope: super::super::federation::FederatedScope::current(
                fixture.project.clone(),
                target(&fixture, domain),
                budget(&fixture.config),
            ),
            text: MemoryQueryText::new("federat").unwrap(),
            repository_kinds: vec![],
            repository_paths: vec![],
            memory_kinds: vec![],
            memory_sources: vec![],
            cursor: None,
        };

        let repository = service.search(request(ContextDomain::Repository)).unwrap();
        assert!(repository.repository.is_some());
        assert!(repository.memory.is_none());
        assert!(
            repository
                .results
                .iter()
                .all(|result| matches!(result, FederatedSearchResult::Repository(_)))
        );

        let memory = service.search(request(ContextDomain::Memory)).unwrap();
        assert!(memory.repository.is_none());
        assert!(memory.memory.is_some());
        assert!(
            memory
                .results
                .iter()
                .all(|result| matches!(result, FederatedSearchResult::Memory(_)))
        );
    }

    #[test]
    fn combined_context_crosses_only_the_exact_resolved_link_set() {
        let fixture = fixture();
        let graph = SqliteGraphQuery::new(
            &fixture.graph_sidecar,
            fixture.config.query_limits.clone(),
            Some(fixture.graph_freshness.clone()),
        );
        let memory =
            SqliteMemoryQuery::new(&fixture.memory_sidecar, fixture.config.query_limits.clone());
        let memory_search = memory
            .search(MemorySearchRequest {
                scope: MemoryQueryScope::current(
                    fixture.project.clone(),
                    MemoryRevisionSelector::Published(MemoryViewName::new("project").unwrap()),
                    budget(&fixture.config),
                ),
                text: MemoryQueryText::new("src/lib.rs").unwrap(),
                entity_kinds: vec![],
                source_categories: vec![],
                page: MemoryPageRequest::default(),
            })
            .unwrap();
        let memory_entity = memory_search.hits[0].entity.id.clone();
        let service = service(&fixture, &graph, &memory);
        let response = service
            .context(FederatedContextRequest {
                scope: super::super::federation::FederatedScope::current(
                    fixture.project.clone(),
                    target(&fixture, ContextDomain::All),
                    budget(&fixture.config),
                ),
                seeds: vec![FederatedContextSeed::MemoryEntity(memory_entity)],
                repository_policy: ContextPolicy {
                    direction: EdgeDirection::Both,
                    edge_kinds: vec![],
                    include_unresolved: false,
                    include_external: false,
                },
                memory_policy: MemoryContextPolicy {
                    relationship_kinds: vec![],
                    include_unresolved: false,
                    include_stale: false,
                    include_snippets: false,
                },
                cursor: None,
            })
            .unwrap();
        assert!(response.repository.is_some());
        assert!(response.memory.is_some());
        assert!(response.items.iter().any(|item| {
            matches!(item, FederatedContextItem::Repository(item) if item.path.as_str() == "src/lib.rs")
        }));
        assert!(
            response
                .items
                .iter()
                .any(|item| matches!(item, FederatedContextItem::Memory(_)))
        );
        assert!(!response.cross_domain_links.is_empty());
        assert!(response.cross_domain_links.iter().all(|relationship| {
            relationship.provenance.resolution == MemoryResolutionState::Resolved
        }));
        assert_eq!(
            response.repository.as_ref().unwrap().freshness.freshness,
            crate::repository_graph::domain::Freshness::Fresh
        );
        assert_eq!(
            response.memory.as_ref().unwrap().freshness.freshness,
            super::super::query::MemoryFreshness::Fresh
        );
    }

    #[test]
    fn combined_search_cursor_is_deterministic_and_does_not_repeat_results() {
        let fixture = fixture();
        let graph = SqliteGraphQuery::new(
            &fixture.graph_sidecar,
            fixture.config.query_limits.clone(),
            Some(fixture.graph_freshness.clone()),
        );
        let memory =
            SqliteMemoryQuery::new(&fixture.memory_sidecar, fixture.config.query_limits.clone());
        let service = service(&fixture, &graph, &memory);
        let mut page_budget = budget(&fixture.config);
        page_budget.max_results = NonZeroU32::new(1).unwrap();
        let mut request = FederatedSearchRequest {
            scope: super::super::federation::FederatedScope::current(
                fixture.project.clone(),
                target(&fixture, ContextDomain::All),
                page_budget,
            ),
            text: MemoryQueryText::new("federat").unwrap(),
            repository_kinds: vec![],
            repository_paths: vec![],
            memory_kinds: vec![],
            memory_sources: vec![],
            cursor: None,
        };
        let first = service.search(request.clone()).unwrap();
        assert_eq!(first.results.len(), 1);
        request.cursor = first.page.next_cursor.clone();
        assert!(request.cursor.is_some());
        let second = service.search(request).unwrap();
        assert_eq!(second.results.len(), 1);
        assert_ne!(
            search_result_key(&first.results[0]),
            search_result_key(&second.results[0])
        );
    }
}
