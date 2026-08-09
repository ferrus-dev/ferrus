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
        let has_more = offset + returned.len() < total || candidates.peek().is_some();
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
        }
        let page = federated_page(
            reason,
            returned.len(),
            budget.max_bytes.saturating_sub(remaining_bytes),
            explored_depth,
            has_more && !supplemental_truncated,
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

mod support;
use support::*;

#[cfg(test)]
#[path = "federation_service_tests.rs"]
mod tests;
