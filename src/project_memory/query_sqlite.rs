//! Bounded read-only SQLite queries for immutable project-memory revisions.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    num::NonZeroU64,
    time::{Duration, Instant},
};

use rusqlite::{Connection, Error as SqliteError, ErrorCode, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::repository_graph::config::QueryLimitsConfig;

use super::{
    MEMORY_QUERY_WIRE_VERSION,
    diagnostics::{MemoryDiagnostic, MemoryDiagnosticCode},
    domain::{
        MemoryBuild, MemoryBuildId, MemoryEntity, MemoryEntityData, MemoryEntityId,
        MemoryEntityKind, MemoryPageCursor, MemoryRelationship, MemoryRelationshipKind,
        MemoryRelationshipTarget, MemoryResolutionState, MemoryRevision, MemoryRevisionId,
        MemorySourceCategory, ProjectRef,
    },
    policy::MemoryPolicy,
    ports::{MemoryContent, MemoryQuery, MemoryStore},
    query::{
        MemoryAvailability, MemoryContentRequest, MemoryContextItem, MemoryContextRequest,
        MemoryContextResponse, MemoryContextSeed, MemoryFreshness, MemoryFreshnessEnvelope,
        MemoryPageInfo, MemoryQueryBudget, MemoryQueryError, MemoryRetrievalAction,
        MemorySearchHit, MemorySearchMatchKind, MemorySearchRequest, MemorySearchResponse,
        MemorySnippet, MemorySourcePolicyStatus, MemoryStatistics, MemoryStatusData,
        MemoryStatusRequest, MemoryStatusResponse, MemoryTruncation, MemoryTruncationReason,
    },
    sqlite::{MemorySidecar, MemoryStoreError},
};

const MAX_FILTERS: usize = 32;
const MAX_CONTEXT_CANDIDATES: usize = 4_096;
const SQLITE_PROGRESS_OPS: i32 = 100;
const CURSOR_VERSION: u32 = 1;

pub fn default_budget(limits: &QueryLimitsConfig) -> Result<MemoryQueryBudget, MemoryQueryError> {
    use std::num::NonZeroU32;

    Ok(MemoryQueryBudget {
        max_results: NonZeroU32::new(limits.max_results)
            .ok_or_else(|| invalid_request("query.maxresults"))?,
        max_bytes: NonZeroU64::new(limits.max_bytes)
            .ok_or_else(|| invalid_request("query.maxbytes"))?,
        max_snippet_bytes: NonZeroU64::new(limits.max_snippet_bytes)
            .ok_or_else(|| invalid_request("query.maxsnippetbytes"))?,
        max_depth: NonZeroU32::new(limits.max_depth)
            .ok_or_else(|| invalid_request("query.maxdepth"))?,
        max_duration_ms: NonZeroU64::new(limits.max_duration_ms)
            .ok_or_else(|| invalid_request("query.maxduration"))?,
        max_diagnostics: NonZeroU32::new(limits.max_diagnostics)
            .ok_or_else(|| invalid_request("query.maxdiagnostics"))?,
    })
}

struct QueryDeadline<'connection> {
    connection: &'connection Connection,
}

impl<'connection> QueryDeadline<'connection> {
    fn install(
        connection: &'connection Connection,
        started: Instant,
        duration: Duration,
    ) -> Result<Self, MemoryQueryError> {
        connection
            .progress_handler(
                SQLITE_PROGRESS_OPS,
                Some(move || started.elapsed() >= duration),
            )
            .map_err(sqlite_error)?;
        Ok(Self { connection })
    }
}

impl Drop for QueryDeadline<'_> {
    fn drop(&mut self) {
        let _ = self.connection.progress_handler(0, None::<fn() -> bool>);
    }
}

#[derive(Debug, Clone, Copy)]
struct EffectiveBudget {
    max_results: u32,
    max_bytes: u64,
    max_snippet_bytes: u64,
    max_depth: u32,
    max_duration_ms: u64,
    max_diagnostics: u32,
}

impl EffectiveBudget {
    fn new(requested: &MemoryQueryBudget, limits: &QueryLimitsConfig) -> Self {
        Self {
            max_results: requested.max_results.get().min(limits.max_results),
            max_bytes: requested.max_bytes.get().min(limits.max_bytes),
            max_snippet_bytes: requested
                .max_snippet_bytes
                .get()
                .min(limits.max_snippet_bytes),
            max_depth: requested.max_depth.get().min(limits.max_depth),
            max_duration_ms: requested.max_duration_ms.get().min(limits.max_duration_ms),
            max_diagnostics: requested.max_diagnostics.get().min(limits.max_diagnostics),
        }
    }

    fn duration(self) -> Duration {
        Duration::from_millis(self.max_duration_ms)
    }
}

struct ResolvedScope {
    revision: MemoryRevision,
    budget: EffectiveBudget,
    freshness: MemoryFreshnessEnvelope,
}

struct ContextCandidate {
    entity: MemoryEntity,
    depth: u32,
    selection_reasons: Vec<MemoryDiagnosticCode>,
}

struct ContextAssembly {
    candidates: Vec<ContextCandidate>,
    relationships: Vec<MemoryRelationship>,
    explored_depth: u32,
    duration_exceeded: bool,
}

/// Local query implementation. It never creates, migrates, or mutates a sidecar.
pub struct SqliteMemoryQuery<'a> {
    sidecar: &'a MemorySidecar,
    limits: QueryLimitsConfig,
    content: Option<&'a dyn MemoryContent>,
}

impl<'a> SqliteMemoryQuery<'a> {
    pub fn new(sidecar: &'a MemorySidecar, limits: QueryLimitsConfig) -> Self {
        Self {
            sidecar,
            limits,
            content: None,
        }
    }

    pub fn with_content(mut self, content: &'a dyn MemoryContent) -> Self {
        self.content = Some(content);
        self
    }

    fn resolve_scope(
        &self,
        scope: &super::query::MemoryQueryScope,
    ) -> Result<ResolvedScope, MemoryQueryError> {
        validate_wire_version(scope.wire_version)?;
        let revision = match &scope.revision {
            super::query::MemoryRevisionSelector::Published(view_name) => self
                .sidecar
                .published_view(&scope.project, view_name)
                .map_err(store_error)?
                .ok_or(MemoryQueryError::Unavailable)
                .and_then(|view| {
                    self.sidecar
                        .revision(&view.revision_id)
                        .map_err(store_error)?
                        .ok_or(MemoryQueryError::RevisionNotFound)
                })?,
            super::query::MemoryRevisionSelector::Revision(revision_id) => self
                .sidecar
                .revision(revision_id)
                .map_err(store_error)?
                .ok_or(MemoryQueryError::RevisionNotFound)?,
        };
        if revision.project != scope.project {
            return Err(MemoryQueryError::RevisionNotFound);
        }
        Ok(ResolvedScope {
            freshness: freshness(&revision, scope.freshness_comparison.as_ref()),
            revision,
            budget: EffectiveBudget::new(&scope.budget, &self.limits),
        })
    }

    fn entities(
        &self,
        revision_id: &MemoryRevisionId,
    ) -> Result<Vec<MemoryEntity>, MemoryQueryError> {
        let mut statement = self
            .sidecar
            .connection()
            .prepare("SELECT entity_json FROM memory_entities WHERE revision_id = ?1 ORDER BY id")
            .map_err(sqlite_error)?;
        let mut rows = statement
            .query([revision_id.as_str()])
            .map_err(sqlite_error)?;
        let mut entities = Vec::new();
        while let Some(row) = rows.next().map_err(sqlite_error)? {
            let encoded = row.get::<_, String>(0).map_err(sqlite_error)?;
            entities.push(serde_json::from_str(&encoded).map_err(serialization_error)?);
        }
        Ok(entities)
    }

    fn relationships(
        &self,
        revision_id: &MemoryRevisionId,
    ) -> Result<Vec<MemoryRelationship>, MemoryQueryError> {
        let mut relationships = BTreeMap::new();
        let mut statement = self
            .sidecar
            .connection()
            .prepare(
                "SELECT relationship_json FROM memory_relationships \
                 WHERE revision_id = ?1 ORDER BY id",
            )
            .map_err(sqlite_error)?;
        let mut rows = statement
            .query([revision_id.as_str()])
            .map_err(sqlite_error)?;
        while let Some(row) = rows.next().map_err(sqlite_error)? {
            let encoded = row.get::<_, String>(0).map_err(sqlite_error)?;
            let relationship: MemoryRelationship =
                serde_json::from_str(&encoded).map_err(serialization_error)?;
            relationships.insert(relationship.id.clone(), relationship);
        }

        // Link sets are independent from semantic revisions. Prefer the newest
        // stored resolution for duplicate deterministic relationship IDs.
        let mut statement = self
            .sidecar
            .connection()
            .prepare(
                "SELECT links.relationship_json FROM memory_repository_links links \
                 JOIN memory_repository_link_sets sets ON sets.id = links.link_set_id \
                 WHERE sets.memory_revision_id = ?1 \
                 ORDER BY sets.sequence DESC, links.relationship_id",
            )
            .map_err(sqlite_error)?;
        let mut rows = statement
            .query([revision_id.as_str()])
            .map_err(sqlite_error)?;
        while let Some(row) = rows.next().map_err(sqlite_error)? {
            let encoded = row.get::<_, String>(0).map_err(sqlite_error)?;
            let relationship: MemoryRelationship =
                serde_json::from_str(&encoded).map_err(serialization_error)?;
            relationships
                .entry(relationship.id.clone())
                .or_insert(relationship);
        }
        Ok(relationships.into_values().collect())
    }

    fn diagnostics(
        &self,
        revision_id: &MemoryRevisionId,
        max_diagnostics: u32,
    ) -> Result<Vec<MemoryDiagnostic>, MemoryQueryError> {
        let mut statement = self
            .sidecar
            .connection()
            .prepare(
                "SELECT diagnostics.diagnostic_json FROM memory_diagnostics diagnostics \
                 JOIN memory_revision_diagnostic_sets sets \
                   ON sets.build_id = diagnostics.build_id \
                 WHERE sets.revision_id = ?1 ORDER BY diagnostics.sequence LIMIT ?2",
            )
            .map_err(sqlite_error)?;
        let mut rows = statement
            .query(params![
                revision_id.as_str(),
                i64::from(max_diagnostics.saturating_add(1)),
            ])
            .map_err(sqlite_error)?;
        let mut diagnostics = Vec::new();
        while let Some(row) = rows.next().map_err(sqlite_error)? {
            let encoded = row.get::<_, String>(0).map_err(sqlite_error)?;
            diagnostics.push(serde_json::from_str(&encoded).map_err(serialization_error)?);
        }
        diagnostics.truncate(max_diagnostics as usize);
        Ok(diagnostics)
    }

    fn diagnostics_with_deadline(
        &self,
        scope: &ResolvedScope,
        started: Instant,
    ) -> Result<(Vec<MemoryDiagnostic>, bool), MemoryQueryError> {
        if started.elapsed() >= scope.budget.duration() {
            return Ok((vec![], true));
        }
        let deadline =
            QueryDeadline::install(self.sidecar.connection(), started, scope.budget.duration())?;
        let diagnostics = self.diagnostics(&scope.revision.id, scope.budget.max_diagnostics);
        drop(deadline);
        match diagnostics {
            Ok(diagnostics) => Ok((diagnostics, started.elapsed() >= scope.budget.duration())),
            Err(MemoryQueryError::BudgetExceeded(MemoryTruncationReason::Duration)) => {
                Ok((vec![], true))
            }
            Err(error) => Err(error),
        }
    }

    fn context_assembly(
        &self,
        scope: &ResolvedScope,
        request: &MemoryContextRequest,
        started: Instant,
    ) -> Result<ContextAssembly, MemoryQueryError> {
        let entities = self.entities(&scope.revision.id)?;
        let relationships = self.relationships(&scope.revision.id)?;
        let entity_map = entities
            .into_iter()
            .map(|entity| (entity.id.clone(), entity))
            .collect::<BTreeMap<_, _>>();
        let mut seed_ids = BTreeMap::<MemoryEntityId, Vec<MemoryDiagnosticCode>>::new();
        for seed in &request.seeds {
            for entity_id in
                matching_seed_entities(seed, &entity_map, &relationships, &request.policy)
            {
                let reason = match seed {
                    MemoryContextSeed::RepositoryPath(_)
                    | MemoryContextSeed::RepositorySymbol(_) => "context.repositorylink",
                    _ => "context.seed",
                };
                seed_ids
                    .entry(entity_id)
                    .or_default()
                    .push(diagnostic_code(reason));
            }
        }

        let allowed_kinds = request
            .policy
            .relationship_kinds
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let mut adjacency =
            BTreeMap::<MemoryEntityId, Vec<(MemoryEntityId, MemoryRelationship)>>::new();
        for relationship in &relationships {
            if !allowed_kinds.is_empty() && !allowed_kinds.contains(&relationship.kind) {
                continue;
            }
            if !resolution_visible(relationship.provenance.resolution, &request.policy) {
                continue;
            }
            let MemoryRelationshipTarget::MemoryEntity { entity_id } = &relationship.target else {
                continue;
            };
            adjacency
                .entry(relationship.source.clone())
                .or_default()
                .push((entity_id.clone(), relationship.clone()));
            adjacency
                .entry(entity_id.clone())
                .or_default()
                .push((relationship.source.clone(), relationship.clone()));
        }
        for neighbors in adjacency.values_mut() {
            neighbors.sort_by(|left, right| {
                left.0
                    .cmp(&right.0)
                    .then_with(|| left.1.id.cmp(&right.1.id))
            });
        }

        let mut queue = VecDeque::new();
        let mut selected = BTreeMap::<MemoryEntityId, (u32, Vec<MemoryDiagnosticCode>)>::new();
        for (entity_id, reasons) in seed_ids {
            if entity_map.contains_key(&entity_id) {
                selected.insert(entity_id.clone(), (0, reasons));
                queue.push_back((entity_id, 0_u32));
            }
        }
        let mut selected_relationships = BTreeMap::new();
        let mut explored_depth = 0_u32;
        let mut duration_exceeded = false;
        while let Some((entity_id, depth)) = queue.pop_front() {
            if started.elapsed() >= scope.budget.duration() {
                duration_exceeded = true;
                break;
            }
            explored_depth = explored_depth.max(depth);
            if depth >= scope.budget.max_depth {
                continue;
            }
            for (neighbor, relationship) in adjacency.get(&entity_id).into_iter().flatten() {
                selected_relationships
                    .entry(relationship.id.clone())
                    .or_insert_with(|| relationship.clone());
                if selected.len() >= MAX_CONTEXT_CANDIDATES {
                    continue;
                }
                if !selected.contains_key(neighbor) {
                    selected.insert(
                        neighbor.clone(),
                        (depth + 1, vec![diagnostic_code("context.relationship")]),
                    );
                    queue.push_back((neighbor.clone(), depth + 1));
                }
            }
        }
        let mut candidates = selected
            .into_iter()
            .filter_map(|(entity_id, (depth, mut reasons))| {
                let entity = entity_map.get(&entity_id)?.clone();
                reasons.sort();
                reasons.dedup();
                Some(ContextCandidate {
                    entity,
                    depth,
                    selection_reasons: reasons,
                })
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            left.depth
                .cmp(&right.depth)
                .then_with(|| left.entity.data.kind().cmp(&right.entity.data.kind()))
                .then_with(|| left.entity.id.cmp(&right.entity.id))
        });
        Ok(ContextAssembly {
            candidates,
            relationships: selected_relationships.into_values().collect(),
            explored_depth,
            duration_exceeded,
        })
    }

    fn attach_snippet(
        &self,
        scope: &ResolvedScope,
        entity: &MemoryEntity,
        remaining_snippet_bytes: u64,
    ) -> Result<Option<MemorySnippet>, MemoryQueryError> {
        let Some(content) = self.content else {
            return Ok(None);
        };
        let max_bytes = remaining_snippet_bytes.min(scope.budget.max_snippet_bytes);
        let Some(max_bytes) = NonZeroU64::new(max_bytes) else {
            return Ok(None);
        };
        let response = content.content(MemoryContentRequest {
            project: scope.revision.project.clone(),
            revision_id: scope.revision.id.clone(),
            source_category: entity.provenance.source_category,
            locator: entity.provenance.source_locator.clone(),
            expected_fingerprint: entity.provenance.source_fingerprint.clone(),
            evidence: Some(entity.provenance.evidence.clone()),
            max_bytes,
        })?;
        if response.verified_fingerprint != entity.provenance.source_fingerprint {
            return Err(MemoryQueryError::ContentChanged);
        }
        let text = String::from_utf8(response.bytes)
            .map_err(|_| MemoryQueryError::Backend(diagnostic_code("content.nonutf8")))?;
        Ok(Some(MemorySnippet {
            source_locator: entity.provenance.source_locator.clone(),
            evidence: Some(entity.provenance.evidence.clone()),
            verified_fingerprint: response.verified_fingerprint,
            text,
            truncated: response.truncated,
        }))
    }
}

impl MemoryQuery for SqliteMemoryQuery<'_> {
    fn status(
        &self,
        request: MemoryStatusRequest,
    ) -> Result<MemoryStatusResponse, MemoryQueryError> {
        validate_wire_version(request.scope.wire_version)?;
        let budget = EffectiveBudget::new(&request.scope.budget, &self.limits);
        let started = Instant::now();
        let deadline =
            QueryDeadline::install(self.sidecar.connection(), started, budget.duration())?;
        let resolved = self.resolve_scope(&request.scope);
        let scope = match resolved {
            Ok(scope) => scope,
            Err(MemoryQueryError::Unavailable)
                if matches!(
                    request.scope.revision,
                    super::query::MemoryRevisionSelector::Published(_)
                ) =>
            {
                drop(deadline);
                let latest = self.latest_build(&request.scope.project)?;
                return Ok(MemoryStatusResponse {
                    wire_version: MEMORY_QUERY_WIRE_VERSION,
                    project: request.scope.project.clone(),
                    revision_id: None,
                    freshness: unknown_freshness(),
                    diagnostics: vec![],
                    data: MemoryStatusData {
                        availability: MemoryAvailability::NotBuilt,
                        build_state: latest.as_ref().map(|build| build.state),
                        build_id: latest.map(|build| build.id),
                        memory_model_version: None,
                        statistics: None,
                        recommended_action: Some(MemoryRetrievalAction::Build),
                        source_policy: source_policy_status(),
                    },
                });
            }
            Err(error) => return Err(error),
        };
        let latest = self.latest_build(&scope.revision.project)?;
        let statistics = self.statistics(&scope.revision.id)?;
        let diagnostics = self.diagnostics(&scope.revision.id, budget.max_diagnostics)?;
        drop(deadline);
        if started.elapsed() >= budget.duration() {
            return Err(MemoryQueryError::BudgetExceeded(
                MemoryTruncationReason::Duration,
            ));
        }
        Ok(MemoryStatusResponse {
            wire_version: MEMORY_QUERY_WIRE_VERSION,
            project: scope.revision.project.clone(),
            revision_id: Some(scope.revision.id.clone()),
            freshness: scope.freshness.clone(),
            diagnostics,
            data: MemoryStatusData {
                availability: MemoryAvailability::Available,
                build_state: latest.as_ref().map(|build| build.state),
                build_id: latest.map(|build| build.id),
                memory_model_version: Some(scope.revision.memory_model_version),
                statistics: Some(statistics),
                recommended_action: freshness_action(&scope.freshness),
                source_policy: source_policy_status(),
            },
        })
    }

    fn search(
        &self,
        request: MemorySearchRequest,
    ) -> Result<MemorySearchResponse, MemoryQueryError> {
        if request.entity_kinds.len() > MAX_FILTERS || request.source_categories.len() > MAX_FILTERS
        {
            return Err(invalid_request("query.filters"));
        }
        let started = Instant::now();
        let scope = self.resolve_scope(&request.scope)?;
        let fingerprint = search_fingerprint(&request)?;
        let offset = decode_cursor(
            request.page.cursor.as_ref(),
            "search",
            &scope.revision.id,
            &fingerprint,
        )?;
        let deadline =
            QueryDeadline::install(self.sidecar.connection(), started, scope.budget.duration())?;
        let (entities, query_timed_out) = match self.entities(&scope.revision.id) {
            Ok(entities) => (entities, false),
            Err(MemoryQueryError::BudgetExceeded(MemoryTruncationReason::Duration)) => {
                (vec![], true)
            }
            Err(error) => return Err(error),
        };
        drop(deadline);
        let mut hits = entities
            .into_iter()
            .filter(|entity| {
                (request.entity_kinds.is_empty()
                    || request.entity_kinds.contains(&entity.data.kind()))
                    && (request.source_categories.is_empty()
                        || request
                            .source_categories
                            .contains(&entity.provenance.source_category))
            })
            .filter_map(|entity| memory_search_hit(entity, request.text.as_str()))
            .collect::<Vec<_>>();
        hits.sort_by(memory_hit_order);
        hits.dedup_by(|left, right| left.entity.id == right.entity.id);
        let offset = usize::try_from(offset).map_err(|_| MemoryQueryError::StaleCursor)?;
        if offset > hits.len() {
            return Err(MemoryQueryError::StaleCursor);
        }
        let total = hits.len();
        let mut hits = hits.into_iter().skip(offset).peekable();
        let mut returned = Vec::new();
        let mut returned_bytes = 0_u64;
        let mut reason = query_timed_out.then_some(MemoryTruncationReason::Duration);
        for hit in hits.by_ref() {
            if returned.len() >= scope.budget.max_results as usize {
                reason = Some(MemoryTruncationReason::Results);
                break;
            }
            if started.elapsed() >= scope.budget.duration() {
                reason = Some(MemoryTruncationReason::Duration);
                break;
            }
            let bytes = serialized_len(&hit)?;
            if returned_bytes.saturating_add(bytes) > scope.budget.max_bytes {
                if returned.is_empty() {
                    return Err(MemoryQueryError::BudgetExceeded(
                        MemoryTruncationReason::Bytes,
                    ));
                }
                reason = Some(MemoryTruncationReason::Bytes);
                break;
            }
            returned_bytes = returned_bytes.saturating_add(bytes);
            returned.push(hit);
        }
        let has_more = offset + returned.len() < total || hits.peek().is_some();
        let (diagnostics, diagnostics_timed_out) =
            self.diagnostics_with_deadline(&scope, started)?;
        if diagnostics_timed_out {
            reason = Some(MemoryTruncationReason::Duration);
        }
        let page = memory_page(
            reason,
            returned.len(),
            returned_bytes,
            0,
            has_more,
            "search",
            &scope.revision.id,
            &fingerprint,
            offset + returned.len(),
        )?;
        Ok(MemorySearchResponse {
            wire_version: MEMORY_QUERY_WIRE_VERSION,
            project: scope.revision.project,
            revision_id: scope.revision.id,
            freshness: scope.freshness,
            diagnostics,
            page,
            hits: returned,
        })
    }

    fn context(
        &self,
        request: MemoryContextRequest,
    ) -> Result<MemoryContextResponse, MemoryQueryError> {
        if request.seeds.is_empty() || request.seeds.len() > MAX_FILTERS {
            return Err(invalid_request("context.seeds"));
        }
        if request.policy.relationship_kinds.len() > MAX_FILTERS {
            return Err(invalid_request("context.filters"));
        }
        let started = Instant::now();
        let scope = self.resolve_scope(&request.scope)?;
        let fingerprint = context_fingerprint(&request, scope.budget.max_depth)?;
        let offset = decode_cursor(
            request.page.cursor.as_ref(),
            "context",
            &scope.revision.id,
            &fingerprint,
        )?;
        let deadline =
            QueryDeadline::install(self.sidecar.connection(), started, scope.budget.duration())?;
        let assembly = match self.context_assembly(&scope, &request, started) {
            Ok(assembly) => assembly,
            Err(MemoryQueryError::BudgetExceeded(MemoryTruncationReason::Duration)) => {
                ContextAssembly {
                    candidates: vec![],
                    relationships: vec![],
                    explored_depth: 0,
                    duration_exceeded: true,
                }
            }
            Err(error) => return Err(error),
        };
        drop(deadline);
        let offset = usize::try_from(offset).map_err(|_| MemoryQueryError::StaleCursor)?;
        if offset > assembly.candidates.len() {
            return Err(MemoryQueryError::StaleCursor);
        }
        let total = assembly.candidates.len();
        let mut candidates = assembly.candidates.into_iter().skip(offset).peekable();
        let mut items = Vec::new();
        let mut returned_bytes = 0_u64;
        let mut snippet_bytes = 0_u64;
        let mut reason = assembly
            .duration_exceeded
            .then_some(MemoryTruncationReason::Duration);
        if request.policy.include_snippets && self.content.is_none() && reason.is_none() {
            reason = Some(MemoryTruncationReason::Capability);
        }
        for candidate in candidates.by_ref() {
            if items.len() >= scope.budget.max_results as usize {
                reason = Some(MemoryTruncationReason::Results);
                break;
            }
            if started.elapsed() >= scope.budget.duration() {
                reason = Some(MemoryTruncationReason::Duration);
                break;
            }
            let snippet = if request.policy.include_snippets {
                self.attach_snippet(
                    &scope,
                    &candidate.entity,
                    scope.budget.max_snippet_bytes.saturating_sub(snippet_bytes),
                )?
            } else {
                None
            };
            if let Some(snippet) = &snippet {
                snippet_bytes = snippet_bytes.saturating_add(snippet.text.len() as u64);
            }
            let item = MemoryContextItem {
                entity: candidate.entity,
                snippet,
                selection_reasons: candidate.selection_reasons,
            };
            let bytes = serialized_len(&item)?;
            if returned_bytes.saturating_add(bytes) > scope.budget.max_bytes {
                if items.is_empty() {
                    return Err(MemoryQueryError::BudgetExceeded(
                        MemoryTruncationReason::Bytes,
                    ));
                }
                reason = Some(MemoryTruncationReason::Bytes);
                break;
            }
            returned_bytes = returned_bytes.saturating_add(bytes);
            items.push(item);
        }
        let has_more = offset + items.len() < total || candidates.peek().is_some();
        let returned_ids = items
            .iter()
            .map(|item| item.entity.id.clone())
            .collect::<BTreeSet<_>>();
        let relationships = assembly
            .relationships
            .into_iter()
            .filter(|relationship| {
                returned_ids.contains(&relationship.source)
                    || matches!(
                        &relationship.target,
                        MemoryRelationshipTarget::MemoryEntity { entity_id }
                            if returned_ids.contains(entity_id)
                    )
            })
            .collect();
        let (diagnostics, diagnostics_timed_out) =
            self.diagnostics_with_deadline(&scope, started)?;
        if diagnostics_timed_out {
            reason = Some(MemoryTruncationReason::Duration);
        }
        let page = memory_page(
            reason,
            items.len(),
            returned_bytes,
            assembly.explored_depth,
            has_more,
            "context",
            &scope.revision.id,
            &fingerprint,
            offset + items.len(),
        )?;
        Ok(MemoryContextResponse {
            wire_version: MEMORY_QUERY_WIRE_VERSION,
            project: scope.revision.project,
            revision_id: scope.revision.id,
            freshness: scope.freshness,
            diagnostics,
            page,
            items,
            relationships,
        })
    }
}

impl SqliteMemoryQuery<'_> {
    fn latest_build(&self, project: &ProjectRef) -> Result<Option<MemoryBuild>, MemoryQueryError> {
        let build_id = self
            .sidecar
            .connection()
            .query_row(
                "SELECT id FROM memory_builds WHERE project_namespace = ?1 AND project_id = ?2 \
                 ORDER BY sequence DESC LIMIT 1",
                params![project.namespace.as_str(), project.project_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(sqlite_error)?
            .map(MemoryBuildId::new)
            .transpose()
            .map_err(|_| backend_error("storage.corrupt"))?;
        build_id
            .map(|build_id| self.sidecar.build(&build_id).map_err(store_error))
            .transpose()
            .map(Option::flatten)
    }

    fn statistics(
        &self,
        revision_id: &MemoryRevisionId,
    ) -> Result<MemoryStatistics, MemoryQueryError> {
        self.sidecar
            .connection()
            .query_row(
                "SELECT \
                    (SELECT COUNT(DISTINCT source_category) FROM memory_entities WHERE revision_id = ?1), \
                    (SELECT COUNT(*) FROM memory_entities WHERE revision_id = ?1), \
                    (SELECT COUNT(*) FROM memory_relationships WHERE revision_id = ?1) + \
                      (SELECT COUNT(*) FROM memory_repository_links links \
                       JOIN memory_repository_link_sets sets ON sets.id = links.link_set_id \
                       WHERE sets.memory_revision_id = ?1), \
                    (SELECT COUNT(*) FROM memory_repository_links links \
                       JOIN memory_repository_link_sets sets ON sets.id = links.link_set_id \
                       WHERE sets.memory_revision_id = ?1 AND links.resolution = 'stale')",
                [revision_id.as_str()],
                |row| {
                    Ok(MemoryStatistics {
                        sources: unsigned(row.get(0)?)?,
                        entities: unsigned(row.get(1)?)?,
                        relationships: unsigned(row.get(2)?)?,
                        stale_links: unsigned(row.get(3)?)?,
                    })
                },
            )
            .map_err(sqlite_error)
    }
}

fn matching_seed_entities(
    seed: &MemoryContextSeed,
    entities: &BTreeMap<MemoryEntityId, MemoryEntity>,
    relationships: &[MemoryRelationship],
    policy: &super::query::MemoryContextPolicy,
) -> Vec<MemoryEntityId> {
    let mut matches = match seed {
        MemoryContextSeed::Entity(entity_id) => entities
            .contains_key(entity_id)
            .then_some(entity_id.clone())
            .into_iter()
            .collect::<Vec<_>>(),
        MemoryContextSeed::Milestone(record_id) => entities
            .values()
            .filter_map(|entity| match &entity.data {
                MemoryEntityData::Milestone { milestone_id, .. } if milestone_id == record_id => {
                    Some(entity.id.clone())
                }
                MemoryEntityData::TaskReference { milestone_id, .. }
                | MemoryEntityData::FollowUpWork { milestone_id, .. }
                    if milestone_id.as_ref() == Some(record_id) =>
                {
                    Some(entity.id.clone())
                }
                _ => None,
            })
            .collect(),
        MemoryContextSeed::Task(record_id) => entities
            .values()
            .filter_map(|entity| match &entity.data {
                MemoryEntityData::TaskReference { task_id, .. } if task_id == record_id => {
                    Some(entity.id.clone())
                }
                MemoryEntityData::RunReference { task_id, .. } if task_id == record_id => {
                    Some(entity.id.clone())
                }
                _ => None,
            })
            .collect(),
        MemoryContextSeed::Run(record_id) => entities
            .values()
            .filter_map(|entity| match &entity.data {
                MemoryEntityData::RunReference { run_id, .. } if run_id == record_id => {
                    Some(entity.id.clone())
                }
                _ => None,
            })
            .collect(),
        MemoryContextSeed::RepositoryPath(path) => relationships
            .iter()
            .filter(|relationship| resolution_visible(relationship.provenance.resolution, policy))
            .filter_map(|relationship| match &relationship.target {
                MemoryRelationshipTarget::RepositoryPath { path: target, .. } if target == path => {
                    Some(relationship.source.clone())
                }
                MemoryRelationshipTarget::RepositoryNode { .. } => None,
                _ => None,
            })
            .collect(),
        MemoryContextSeed::RepositorySymbol(symbol) => relationships
            .iter()
            .filter(|relationship| resolution_visible(relationship.provenance.resolution, policy))
            .filter_map(|relationship| match &relationship.target {
                MemoryRelationshipTarget::RepositorySymbol { semantic_key, .. }
                    if semantic_key == symbol =>
                {
                    Some(relationship.source.clone())
                }
                _ => None,
            })
            .collect(),
    };
    matches.sort();
    matches.dedup();
    matches
}

fn resolution_visible(
    resolution: MemoryResolutionState,
    policy: &super::query::MemoryContextPolicy,
) -> bool {
    match resolution {
        MemoryResolutionState::Resolved => true,
        MemoryResolutionState::Stale => policy.include_stale,
        MemoryResolutionState::Unresolved => policy.include_unresolved,
    }
}

fn memory_search_hit(entity: MemoryEntity, raw_query: &str) -> Option<MemorySearchHit> {
    let query = raw_query.trim().to_lowercase();
    let id = entity.id.as_str().to_lowercase();
    let title = entity_title(&entity).map(str::to_lowercase);
    let text = entity_text(&entity).map(str::to_lowercase);
    let provenance = serde_json::to_string(&entity.provenance)
        .expect("memory provenance is serializable")
        .to_lowercase();
    let (match_kind, score, reason) = if id == query {
        (MemorySearchMatchKind::ExactId, 1.0, "search.exactid")
    } else if title.as_deref() == Some(query.as_str()) {
        (MemorySearchMatchKind::ExactTitle, 0.95, "search.exacttitle")
    } else if title
        .as_ref()
        .is_some_and(|title| title.starts_with(&query))
    {
        (
            MemorySearchMatchKind::TitlePrefix,
            0.85,
            "search.titleprefix",
        )
    } else if title.as_ref().is_some_and(|title| title.contains(&query)) {
        (
            MemorySearchMatchKind::TitleContains,
            0.75,
            "search.titlecontains",
        )
    } else if text.as_ref().is_some_and(|text| text.contains(&query)) {
        (
            MemorySearchMatchKind::CuratedTextContains,
            0.65,
            "search.textcontains",
        )
    } else if provenance.contains(&query) {
        (
            MemorySearchMatchKind::ProvenanceReference,
            0.55,
            "search.provenance",
        )
    } else {
        return None;
    };
    Some(MemorySearchHit {
        entity,
        match_kind,
        score,
        selection_reasons: vec![diagnostic_code(reason)],
    })
}

fn entity_title(entity: &MemoryEntity) -> Option<&str> {
    match &entity.data {
        MemoryEntityData::Specification { title } | MemoryEntityData::Milestone { title, .. } => {
            Some(title.as_str())
        }
        _ => None,
    }
}

fn entity_text(entity: &MemoryEntity) -> Option<&str> {
    match &entity.data {
        MemoryEntityData::Outcome { text }
        | MemoryEntityData::Decision { text }
        | MemoryEntityData::Deviation { text }
        | MemoryEntityData::FollowUpWork { text, .. } => Some(text.as_str()),
        MemoryEntityData::ValidationEvidence {
            text: Some(text), ..
        } => Some(text.as_str()),
        _ => None,
    }
}

fn memory_hit_order(left: &MemorySearchHit, right: &MemorySearchHit) -> std::cmp::Ordering {
    right
        .score
        .total_cmp(&left.score)
        .then_with(|| left.entity.data.kind().cmp(&right.entity.data.kind()))
        .then_with(|| left.entity.id.cmp(&right.entity.id))
}

fn freshness(
    revision: &MemoryRevision,
    comparison: Option<&super::query::MemoryFreshnessComparison>,
) -> MemoryFreshnessEnvelope {
    let Some(comparison) = comparison else {
        return unknown_freshness();
    };
    let mut reason_codes = Vec::new();
    if comparison.source_set_digest != revision.source_set_digest {
        reason_codes.push(diagnostic_code("freshness.sourceschanged"));
    }
    if comparison.policy_digest != revision.policy_digest {
        reason_codes.push(diagnostic_code("freshness.policychanged"));
    }
    if comparison.extractor_set_digest != revision.extractor_set_digest {
        reason_codes.push(diagnostic_code("freshness.extractorschanged"));
    }
    MemoryFreshnessEnvelope {
        freshness: if reason_codes.is_empty() {
            MemoryFreshness::Fresh
        } else {
            MemoryFreshness::Stale
        },
        compared_source_set_digest: Some(comparison.source_set_digest.clone()),
        reason_codes,
    }
}

fn unknown_freshness() -> MemoryFreshnessEnvelope {
    MemoryFreshnessEnvelope {
        freshness: MemoryFreshness::Unknown,
        compared_source_set_digest: None,
        reason_codes: vec![diagnostic_code("freshness.notcompared")],
    }
}

fn freshness_action(freshness: &MemoryFreshnessEnvelope) -> Option<MemoryRetrievalAction> {
    matches!(freshness.freshness, MemoryFreshness::Stale).then_some(MemoryRetrievalAction::Refresh)
}

fn source_policy_status() -> Vec<MemorySourcePolicyStatus> {
    let policy = MemoryPolicy::default();
    MemorySourceCategory::ALL
        .into_iter()
        .filter_map(|category| {
            policy
                .category(category)
                .copied()
                .map(|policy| MemorySourcePolicyStatus { category, policy })
        })
        .collect()
}

fn validate_wire_version(version: u32) -> Result<(), MemoryQueryError> {
    if version == MEMORY_QUERY_WIRE_VERSION {
        Ok(())
    } else {
        Err(invalid_request("query.wireversion"))
    }
}

fn invalid_request(code: &str) -> MemoryQueryError {
    MemoryQueryError::Backend(diagnostic_code(code))
}

fn store_error(error: MemoryStoreError) -> MemoryQueryError {
    match error {
        MemoryStoreError::RevisionNotFound => MemoryQueryError::RevisionNotFound,
        MemoryStoreError::RequiresRebuild => backend_error("storage.incompatible"),
        _ => backend_error("storage.unavailable"),
    }
}

fn sqlite_error(error: SqliteError) -> MemoryQueryError {
    if matches!(
        error,
        SqliteError::SqliteFailure(ref failure, _) if failure.code == ErrorCode::OperationInterrupted
    ) {
        MemoryQueryError::BudgetExceeded(MemoryTruncationReason::Duration)
    } else {
        backend_error("storage.unavailable")
    }
}

fn serialization_error(_: serde_json::Error) -> MemoryQueryError {
    backend_error("storage.corrupt")
}

fn backend_error(code: &str) -> MemoryQueryError {
    MemoryQueryError::Backend(diagnostic_code(code))
}

fn diagnostic_code(value: &str) -> MemoryDiagnosticCode {
    MemoryDiagnosticCode::new(value).expect("static memory diagnostic code is valid")
}

fn unsigned(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn serialized_len(value: &impl Serialize) -> Result<u64, MemoryQueryError> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len() as u64)
        .map_err(serialization_error)
}

#[derive(Serialize, Deserialize)]
struct CursorPayload {
    version: u32,
    operation: String,
    revision_id: MemoryRevisionId,
    fingerprint: String,
    offset: u64,
}

fn encode_cursor(
    operation: &str,
    revision_id: &MemoryRevisionId,
    fingerprint: &str,
    offset: usize,
) -> Result<MemoryPageCursor, MemoryQueryError> {
    let payload = CursorPayload {
        version: CURSOR_VERSION,
        operation: operation.to_string(),
        revision_id: revision_id.clone(),
        fingerprint: fingerprint.to_string(),
        offset: u64::try_from(offset).unwrap_or(u64::MAX),
    };
    let bytes = serde_json::to_vec(&payload).map_err(serialization_error)?;
    MemoryPageCursor::new(format!("cursor:{}", hex(&bytes)))
        .map_err(|_| backend_error("query.cursor"))
}

fn decode_cursor(
    cursor: Option<&MemoryPageCursor>,
    operation: &str,
    revision_id: &MemoryRevisionId,
    fingerprint: &str,
) -> Result<u64, MemoryQueryError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let encoded = cursor
        .as_str()
        .strip_prefix("cursor:")
        .ok_or(MemoryQueryError::StaleCursor)?;
    if encoded.len() > 4_096 || encoded.len() % 2 != 0 {
        return Err(MemoryQueryError::StaleCursor);
    }
    let bytes = unhex(encoded).ok_or(MemoryQueryError::StaleCursor)?;
    let payload: CursorPayload =
        serde_json::from_slice(&bytes).map_err(|_| MemoryQueryError::StaleCursor)?;
    if payload.version != CURSOR_VERSION
        || payload.operation != operation
        || payload.revision_id != *revision_id
        || payload.fingerprint != fingerprint
    {
        return Err(MemoryQueryError::StaleCursor);
    }
    Ok(payload.offset)
}

#[allow(clippy::too_many_arguments)]
fn memory_page(
    reason: Option<MemoryTruncationReason>,
    returned_results: usize,
    returned_bytes: u64,
    explored_depth: u32,
    has_more: bool,
    operation: &str,
    revision_id: &MemoryRevisionId,
    fingerprint: &str,
    offset: usize,
) -> Result<MemoryPageInfo, MemoryQueryError> {
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
    .then(|| encode_cursor(operation, revision_id, fingerprint, offset))
    .transpose()?;
    Ok(MemoryPageInfo {
        next_cursor,
        truncation,
    })
}

#[derive(Serialize)]
struct SearchFingerprint<'a> {
    text: String,
    entity_kinds: Vec<MemoryEntityKind>,
    source_categories: Vec<MemorySourceCategory>,
    marker: &'a str,
}

fn search_fingerprint(request: &MemorySearchRequest) -> Result<String, MemoryQueryError> {
    let mut entity_kinds = request.entity_kinds.clone();
    entity_kinds.sort();
    entity_kinds.dedup();
    let mut source_categories = request.source_categories.clone();
    source_categories.sort();
    source_categories.dedup();
    fingerprint(&SearchFingerprint {
        text: request.text.as_str().trim().to_lowercase(),
        entity_kinds,
        source_categories,
        marker: "search",
    })
}

#[derive(Serialize)]
struct ContextFingerprint<'a> {
    seeds: Vec<String>,
    relationship_kinds: Vec<MemoryRelationshipKind>,
    include_unresolved: bool,
    include_stale: bool,
    include_snippets: bool,
    max_depth: u32,
    marker: &'a str,
}

fn context_fingerprint(
    request: &MemoryContextRequest,
    max_depth: u32,
) -> Result<String, MemoryQueryError> {
    let mut seeds = request
        .seeds
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .map_err(serialization_error)?;
    seeds.sort();
    seeds.dedup();
    let mut relationship_kinds = request.policy.relationship_kinds.clone();
    relationship_kinds.sort();
    relationship_kinds.dedup();
    fingerprint(&ContextFingerprint {
        seeds,
        relationship_kinds,
        include_unresolved: request.policy.include_unresolved,
        include_stale: request.policy.include_stale,
        include_snippets: request.policy.include_snippets,
        max_depth,
        marker: "context",
    })
}

fn fingerprint(value: &impl Serialize) -> Result<String, MemoryQueryError> {
    let encoded = serde_json::to_vec(value).map_err(serialization_error)?;
    let mut hasher = Sha256::new();
    hasher.update(b"ferrus.project-memory.cursor.v1\0");
    hasher.update(encoded);
    Ok(hex(&hasher.finalize()))
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
    use std::{cell::Cell, fs, path::Path, process::Command};

    use tempfile::TempDir;

    use crate::project_memory::{
        domain::{MemoryRecordId, MemoryViewName, ProjectId, ProjectNamespace, ProjectRef},
        index::{MemoryIndexOptions, MemoryIndexer},
        policy::MemoryPolicy,
        query::{MemoryPageRequest, MemoryQueryScope},
        source::LocalMemorySource,
        sqlite::{MEMORY_SIDECAR_FILE_NAME, OpenMemoryQuerySidecarResult, open_for_query_at},
    };
    use crate::repository_graph::domain::RepoPath;

    use super::*;

    fn project() -> ProjectRef {
        ProjectRef {
            namespace: ProjectNamespace::new("local:test").unwrap(),
            project_id: ProjectId::new("query-project").unwrap(),
        }
    }

    fn initialize_repository(root: &Path) {
        assert!(
            Command::new("git")
                .arg("init")
                .arg(root)
                .status()
                .unwrap()
                .success()
        );
        fs::create_dir_all(root.join("docs/specs")).unwrap();
        fs::write(
            root.join("docs/specs/query.md"),
            "# Query memory\n\n- [x] #4.4 Federation\n\nID: rg-test\nDepends on: none\n\n## Outcome\n\nDelivered bounded SQLite retrieval.\n\n### Decisions\n\nUse deterministic ranking.\n",
        )
        .unwrap();
        assert!(
            Command::new("git")
                .current_dir(root)
                .args(["add", "--", "docs/specs/query.md"])
                .status()
                .unwrap()
                .success()
        );
    }

    fn indexed_fixture() -> (TempDir, TempDir, ProjectRef, MemoryRevisionId) {
        let root = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        initialize_repository(root.path());
        let project = project();
        let source = LocalMemorySource::discover_at(
            root.path().to_path_buf(),
            data.path().to_path_buf(),
            project.clone(),
            RepoPath::new("docs/specs").unwrap(),
            MemoryPolicy::default(),
        )
        .unwrap();
        let mut sidecar = MemorySidecar::open_at(data.path()).unwrap();
        let outcome = MemoryIndexer::new(&source, &mut sidecar)
            .unwrap()
            .index(MemoryIndexOptions::default())
            .unwrap();
        (root, data, project, outcome.revision.id)
    }

    fn published_scope(project: ProjectRef, limits: &QueryLimitsConfig) -> MemoryQueryScope {
        MemoryQueryScope::current(
            project,
            super::super::query::MemoryRevisionSelector::Published(
                MemoryViewName::new("project").unwrap(),
            ),
            default_budget(limits).unwrap(),
        )
    }

    struct RecordingContent {
        max_requested: Cell<u64>,
    }

    impl MemoryContent for RecordingContent {
        fn content(
            &self,
            request: MemoryContentRequest,
        ) -> Result<super::super::query::MemoryContentResponse, MemoryQueryError> {
            self.max_requested
                .set(self.max_requested.get().max(request.max_bytes.get()));
            Ok(super::super::query::MemoryContentResponse {
                verified_fingerprint: request.expected_fingerprint,
                bytes: vec![b'x'; request.max_bytes.get() as usize],
                truncated: true,
            })
        }
    }

    #[test]
    fn cursor_rejects_a_different_request_fingerprint() {
        let revision = MemoryRevisionId::new("memory:revision").unwrap();
        let cursor = encode_cursor("search", &revision, "one", 2).unwrap();
        assert_eq!(
            decode_cursor(Some(&cursor), "search", &revision, "two"),
            Err(MemoryQueryError::StaleCursor)
        );
    }

    #[test]
    fn effective_budget_clamps_every_caller_value() {
        use std::num::{NonZeroU32, NonZeroU64};

        let requested = MemoryQueryBudget {
            max_results: NonZeroU32::new(1_000).unwrap(),
            max_bytes: NonZeroU64::new(1_000_000).unwrap(),
            max_snippet_bytes: NonZeroU64::new(1_000_000).unwrap(),
            max_depth: NonZeroU32::new(100).unwrap(),
            max_duration_ms: NonZeroU64::new(100_000).unwrap(),
            max_diagnostics: NonZeroU32::new(1_000).unwrap(),
        };
        let limits = QueryLimitsConfig::default();
        let effective = EffectiveBudget::new(&requested, &limits);
        assert_eq!(effective.max_results, limits.max_results);
        assert_eq!(effective.max_bytes, limits.max_bytes);
        assert_eq!(effective.max_snippet_bytes, limits.max_snippet_bytes);
        assert_eq!(effective.max_depth, limits.max_depth);
        assert_eq!(effective.max_duration_ms, limits.max_duration_ms);
        assert_eq!(effective.max_diagnostics, limits.max_diagnostics);
    }

    #[test]
    fn search_and_context_are_revision_bound_and_deterministic() {
        let (_root, data, project, revision_id) = indexed_fixture();
        let OpenMemoryQuerySidecarResult::Ready(sidecar) =
            open_for_query_at(&data.path().join(MEMORY_SIDECAR_FILE_NAME)).unwrap()
        else {
            panic!("memory query sidecar should be ready");
        };
        let limits = QueryLimitsConfig::default();
        let query = SqliteMemoryQuery::new(&sidecar, limits.clone());
        let search = query
            .search(MemorySearchRequest {
                scope: published_scope(project.clone(), &limits),
                text: super::super::domain::MemoryQueryText::new("sqlite").unwrap(),
                entity_kinds: vec![],
                source_categories: vec![],
                page: MemoryPageRequest::default(),
            })
            .unwrap();
        assert_eq!(search.revision_id, revision_id);
        assert!(!search.hits.is_empty());
        assert!(
            search.hits.windows(2).all(|pair| {
                memory_hit_order(&pair[0], &pair[1]) != std::cmp::Ordering::Greater
            })
        );

        let context = query
            .context(MemoryContextRequest {
                scope: published_scope(project, &limits),
                seeds: vec![MemoryContextSeed::Milestone(
                    MemoryRecordId::new("rg-test").unwrap(),
                )],
                policy: super::super::query::MemoryContextPolicy {
                    relationship_kinds: vec![],
                    include_unresolved: false,
                    include_stale: false,
                    include_snippets: false,
                },
                page: MemoryPageRequest::default(),
            })
            .unwrap();
        assert!(context.items.len() >= 2);
        assert!(!context.relationships.is_empty());
    }

    #[test]
    fn context_snippets_use_the_verified_content_boundary_and_effective_cap() {
        let (_root, data, project, _) = indexed_fixture();
        let OpenMemoryQuerySidecarResult::Ready(sidecar) =
            open_for_query_at(&data.path().join(MEMORY_SIDECAR_FILE_NAME)).unwrap()
        else {
            panic!("memory query sidecar should be ready");
        };
        let limits = QueryLimitsConfig {
            max_snippet_bytes: 7,
            ..QueryLimitsConfig::default()
        };
        let content = RecordingContent {
            max_requested: Cell::new(0),
        };
        let query = SqliteMemoryQuery::new(&sidecar, limits.clone()).with_content(&content);
        let response = query
            .context(MemoryContextRequest {
                scope: published_scope(project, &limits),
                seeds: vec![MemoryContextSeed::Milestone(
                    MemoryRecordId::new("rg-test").unwrap(),
                )],
                policy: super::super::query::MemoryContextPolicy {
                    relationship_kinds: vec![],
                    include_unresolved: false,
                    include_stale: false,
                    include_snippets: true,
                },
                page: MemoryPageRequest::default(),
            })
            .unwrap();
        let snippet_bytes = response
            .items
            .iter()
            .filter_map(|item| item.snippet.as_ref())
            .map(|snippet| snippet.text.len())
            .sum::<usize>();
        assert_eq!(snippet_bytes, 7);
        assert_eq!(content.max_requested.get(), 7);
    }

    #[test]
    fn query_open_is_absent_or_read_only_and_never_creates_or_migrates() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join(MEMORY_SIDECAR_FILE_NAME);
        assert!(matches!(
            open_for_query_at(&path).unwrap(),
            OpenMemoryQuerySidecarResult::Absent
        ));
        assert!(!path.exists());

        drop(MemorySidecar::open(path.clone()).unwrap());
        let OpenMemoryQuerySidecarResult::Ready(sidecar) = open_for_query_at(&path).unwrap() else {
            panic!("current memory sidecar should be queryable");
        };
        assert!(
            sidecar
                .connection()
                .execute("DELETE FROM memory_builds", [])
                .is_err()
        );
    }
}
