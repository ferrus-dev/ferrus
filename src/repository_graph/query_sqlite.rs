//! Bounded read-only SQLite implementation of the portable graph query contract.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    num::{NonZeroU32, NonZeroU64},
    time::{Duration, Instant},
};

use rusqlite::{Connection, Error as SqliteError, ErrorCode, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::{
    QUERY_WIRE_VERSION,
    config::QueryLimitsConfig,
    domain::{
        Availability, BuildId, BuildState, Confidence, DiagnosticCode, DiagnosticLocation,
        DiagnosticSeverity, Digest, EdgeId, EdgeTarget, ExtractorId, ExtractorIdentity,
        FactProvenance, Freshness, GraphBuild, GraphEdge, GraphNode, GraphSnapshot, GraphValue,
        NodeId, PageCursor, RepoPath, RepositoryRef, ResolutionState, SemanticKey, SnapshotId,
        SourceEvidence, SourcePosition, SourceSpan,
    },
    ports::{GraphQuery, SourceFileDescriptor, SourceFileMode, SourceManifest},
    query::{
        ContextData, ContextItem, ContextRequest, ContextResponse, ContextSeed,
        ContextSelectionKind, ContextSelectionReason, DiagnosticSummary, DiagnosticsEnvelope,
        EdgeDirection, FreshnessEnvelope, NeighborhoodData, NeighborhoodEdge, NeighborhoodNode,
        NeighborhoodRequest, NeighborhoodResponse, PageInfo, QueryDiagnostic, QueryError,
        QueryErrorCode, QueryResponse, RetrievalAction, SearchData, SearchHit, SearchMatchKind,
        SearchRequest, SearchResponse, ShowData, ShowLookup, ShowRequest, ShowResponse,
        SnapshotSelector, SnapshotStatistics, SourceRevisionEnvelope, StatusData, StatusRequest,
        StatusResponse, Truncation, TruncationReason,
    },
    sqlite::Sidecar,
    store::StoreError,
};

const NODE_COLUMNS: &str = "snapshot_id, id, kind, semantic_key, extractor_id, extractor_version, \
    extractor_contract_version, resolution_state, confidence, evidence_path, \
    evidence_content_algorithm, evidence_content_digest, span_start_byte, span_end_byte, \
    properties_json, span_start_line, span_start_column, span_end_line, span_end_column";
const EDGE_COLUMNS: &str = "snapshot_id, id, kind, source_node_id, target_node_id, external_target, \
    extractor_id, extractor_version, extractor_contract_version, resolution_state, confidence, \
    evidence_path, evidence_content_algorithm, evidence_content_digest, span_start_byte, \
    span_end_byte, properties_json, span_start_line, span_start_column, span_end_line, \
    span_end_column";
const MAX_FILTERS: usize = 32;
const MAX_QUERY_TEXT_BYTES: usize = 512;
const MAX_CONTEXT_CANDIDATES: usize = 4_096;
const SQLITE_PROGRESS_OPS: i32 = 100;

struct QueryDeadline<'connection> {
    connection: &'connection Connection,
}

impl<'connection> QueryDeadline<'connection> {
    fn install(
        connection: &'connection Connection,
        started: Instant,
        budget: Duration,
    ) -> Result<Self, QueryError> {
        connection
            .progress_handler(
                SQLITE_PROGRESS_OPS,
                Some(move || started.elapsed() >= budget),
            )
            .map_err(|_| backend_error())?;
        Ok(Self { connection })
    }
}

impl Drop for QueryDeadline<'_> {
    fn drop(&mut self) {
        let _ = self.connection.progress_handler(0, None::<fn() -> bool>);
    }
}

struct SearchRows {
    rows: Vec<(GraphNode, SearchMatchKind)>,
    deadline_exceeded: bool,
}

struct NodeRows {
    rows: Vec<GraphNode>,
    deadline_exceeded: bool,
}

struct ContextCandidate {
    node: GraphNode,
    depth: u32,
    selection_reasons: Vec<ContextSelectionReason>,
}

struct ContextAssembly {
    candidates: Vec<ContextCandidate>,
    explored_depth: u32,
    truncation: Option<TruncationReason>,
}

#[derive(Serialize)]
struct SearchPathFilter<'a> {
    exact: &'a str,
    descendants: String,
}

impl SearchRows {
    fn deadline_exceeded() -> Self {
        Self {
            rows: Vec::new(),
            deadline_exceeded: true,
        }
    }
}

impl NodeRows {
    fn deadline_exceeded() -> Self {
        Self {
            rows: Vec::new(),
            deadline_exceeded: true,
        }
    }
}

pub struct SqliteGraphQuery<'a> {
    sidecar: &'a Sidecar,
    limits: QueryLimitsConfig,
    freshness_comparison: Option<FreshnessComparison>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreshnessComparison {
    source_manifest_digest: Digest,
    analysis_config_digest: Digest,
    extractor_set_digest: Digest,
}

impl FreshnessComparison {
    pub fn from_manifest(manifest: &SourceManifest) -> Self {
        Self {
            source_manifest_digest: manifest.revision.manifest_digest.clone(),
            analysis_config_digest: manifest.revision.analysis_config_digest.clone(),
            extractor_set_digest: manifest.extractor_set_digest.clone(),
        }
    }
}

mod engine;

mod service;

struct ResolvedScope {
    repository: RepositoryRef,
    snapshot: super::domain::GraphSnapshot,
    published_view: Option<super::domain::PublishedViewName>,
    source_revision_id: super::domain::SourceRevisionId,
    freshness: FreshnessEnvelope,
    budget: EffectiveBudget,
}

#[derive(Clone, Copy)]
struct EffectiveBudget {
    max_results: u32,
    max_bytes: u64,
    max_depth: u32,
    max_duration_ms: u64,
    max_diagnostics: u32,
}

impl EffectiveBudget {
    fn new(
        requested: &super::domain::QueryBudget,
        service: &QueryLimitsConfig,
    ) -> Result<Self, QueryError> {
        if service.max_results == 0
            || service.max_bytes == 0
            || service.max_depth == 0
            || service.max_duration_ms == 0
            || service.max_diagnostics == 0
        {
            return Err(QueryError {
                wire_version: QUERY_WIRE_VERSION,
                code: QueryErrorCode::BackendUnavailable,
                message: "repository graph query limits are invalid".to_string(),
                retryable: false,
                recommended_action: None,
                details: BTreeMap::new(),
            });
        }
        Ok(Self {
            max_results: requested.max_results.get().min(service.max_results),
            max_bytes: requested.max_bytes.get().min(service.max_bytes),
            max_depth: requested.max_depth.get().min(service.max_depth),
            max_duration_ms: requested.max_duration_ms.get().min(service.max_duration_ms),
            max_diagnostics: requested.max_diagnostics.get().min(service.max_diagnostics),
        })
    }
}

fn validate_wire_version(version: u32) -> Result<(), QueryError> {
    if version == QUERY_WIRE_VERSION {
        Ok(())
    } else {
        Err(QueryError {
            wire_version: QUERY_WIRE_VERSION,
            code: QueryErrorCode::UnsupportedWireVersion,
            message: "repository graph query wire version is unsupported".to_string(),
            retryable: false,
            recommended_action: None,
            details: BTreeMap::new(),
        })
    }
}

fn validate_filters(filters: &[String], path_count: usize) -> Result<(), QueryError> {
    if filters.len() > MAX_FILTERS
        || path_count > MAX_FILTERS
        || filters
            .iter()
            .any(|filter| filter.is_empty() || filter.len() > 128)
    {
        return Err(invalid_request(
            "query filters must contain at most 32 non-empty values of at most 128 bytes",
        ));
    }
    Ok(())
}

fn validate_search_request(request: &SearchRequest) -> Result<(), QueryError> {
    validate_filters(&request.node_kinds, request.paths.len())?;
    let text = request.text.trim();
    if text.is_empty() || text.len() > MAX_QUERY_TEXT_BYTES {
        return Err(invalid_request("search text must contain 1..=512 bytes"));
    }
    Ok(())
}

fn freshness(expected: &GraphSnapshot, actual: Option<&FreshnessComparison>) -> FreshnessEnvelope {
    match actual {
        Some(actual) => {
            let mut reason_codes = Vec::new();
            if actual.source_manifest_digest != expected.source_manifest_digest {
                reason_codes.push("source_manifest_changed".to_string());
            }
            if actual.analysis_config_digest != expected.analysis_config_digest {
                reason_codes.push("analysis_config_changed".to_string());
            }
            if actual.extractor_set_digest != expected.extractor_set_digest {
                reason_codes.push("extractor_set_changed".to_string());
            }
            FreshnessEnvelope {
                freshness: if reason_codes.is_empty() {
                    Freshness::Fresh
                } else {
                    Freshness::Stale
                },
                compared_manifest: Some(actual.source_manifest_digest.clone()),
                reason_codes,
            }
        }
        None => FreshnessEnvelope {
            freshness: Freshness::Unknown,
            compared_manifest: None,
            reason_codes: vec!["source_not_compared".to_string()],
        },
    }
}

fn source_revision(
    snapshot: &GraphSnapshot,
    source_revision_id: &super::domain::SourceRevisionId,
) -> SourceRevisionEnvelope {
    SourceRevisionEnvelope {
        id: source_revision_id.clone(),
        manifest_digest: snapshot.source_manifest_digest.clone(),
    }
}

fn available_status_response(
    resolved: &ResolvedScope,
    latest_build: Option<&GraphBuild>,
    diagnostics: DiagnosticsEnvelope,
    statistics: Option<SnapshotStatistics>,
    duration_truncated: bool,
) -> StatusResponse {
    StatusResponse {
        wire_version: QUERY_WIRE_VERSION,
        repository: resolved.repository.clone(),
        snapshot_id: Some(resolved.snapshot.id.clone()),
        source_revision: Some(source_revision(
            &resolved.snapshot,
            &resolved.source_revision_id,
        )),
        task_view: None,
        freshness: resolved.freshness.clone(),
        diagnostics,
        page: PageInfo {
            next_cursor: None,
            truncation: duration_truncated.then_some(Truncation {
                reason: TruncationReason::Duration,
                returned_results: 0,
                returned_bytes: 0,
                explored_depth: 0,
            }),
        },
        data: StatusData {
            availability: Availability::Available,
            build_state: latest_build.map(|build| build.state),
            build_id: latest_build.map(|build| build.id.clone()),
            published_view: resolved.published_view.clone(),
            graph_model_version: Some(resolved.snapshot.graph_model_version),
            statistics,
            recommended_action: status_action(latest_build, resolved.freshness.freshness),
            task_view_status: None,
            fallback: None,
        },
    }
}

fn truncated_diagnostics() -> DiagnosticsEnvelope {
    DiagnosticsEnvelope {
        truncated: true,
        ..DiagnosticsEnvelope::default()
    }
}

fn status_action(build: Option<&GraphBuild>, freshness: Freshness) -> Option<RetrievalAction> {
    match build.map(|build| build.state) {
        Some(BuildState::Building) => Some(RetrievalAction::WaitForBuild),
        Some(BuildState::Failed) => Some(RetrievalAction::RetryIndex),
        _ if freshness == Freshness::Stale => Some(RetrievalAction::RefreshIndex),
        _ => None,
    }
}

fn search_hit(node: GraphNode, match_kind: SearchMatchKind, query: &str) -> SearchHit {
    let normalized = query.to_lowercase();
    let mut matched_fields = Vec::new();
    if node
        .properties
        .get("name")
        .and_then(graph_string)
        .is_some_and(|name| name.to_lowercase().contains(&normalized))
    {
        matched_fields.push("name".to_string());
    }
    if node
        .semantic_key
        .as_ref()
        .is_some_and(|key| key.as_str().to_lowercase().contains(&normalized))
    {
        matched_fields.push("semantic_key".to_string());
    }
    if node
        .provenance
        .evidence
        .as_ref()
        .is_some_and(|evidence| evidence.path.as_str().to_lowercase().contains(&normalized))
    {
        matched_fields.push("path".to_string());
    }
    let path = node
        .provenance
        .evidence
        .as_ref()
        .map(|evidence| evidence.path.clone());
    let span = node
        .provenance
        .evidence
        .as_ref()
        .and_then(|evidence| evidence.span.clone());
    SearchHit {
        node_id: node.id,
        kind: node.kind,
        semantic_key: node.semantic_key,
        path,
        span,
        provenance: node.provenance,
        match_kind,
        score: match_kind.score(),
        matched_fields,
    }
}

impl SearchMatchKind {
    fn score(self) -> f64 {
        match self {
            Self::ExactSemanticKey => 1.0,
            Self::ExactPath => 0.99,
            Self::ExactNormalizedName => 0.98,
            Self::NormalizedNamePrefix => 0.9,
            Self::NormalizedNameContains => 0.8,
            Self::SemanticKeyContains => 0.7,
            Self::PathContains => 0.6,
        }
    }
}

fn search_match_kind(rank: i64) -> Result<SearchMatchKind, QueryError> {
    match rank {
        0 => Ok(SearchMatchKind::ExactSemanticKey),
        1 => Ok(SearchMatchKind::ExactPath),
        2 => Ok(SearchMatchKind::ExactNormalizedName),
        3 => Ok(SearchMatchKind::NormalizedNamePrefix),
        4 => Ok(SearchMatchKind::NormalizedNameContains),
        5 => Ok(SearchMatchKind::SemanticKeyContains),
        6 => Ok(SearchMatchKind::PathContains),
        _ => Err(backend_error()),
    }
}

fn insert_context_candidate(
    candidates: &mut BTreeMap<NodeId, ContextCandidate>,
    node: GraphNode,
    depth: u32,
    reason: ContextSelectionReason,
) -> bool {
    match candidates.entry(node.id.clone()) {
        std::collections::btree_map::Entry::Occupied(mut entry) => {
            let candidate = entry.get_mut();
            candidate.depth = candidate.depth.min(depth);
            if !candidate.selection_reasons.contains(&reason) {
                candidate.selection_reasons.push(reason);
            }
            false
        }
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(ContextCandidate {
                node,
                depth,
                selection_reasons: vec![reason],
            });
            true
        }
    }
}

fn context_edge_allowed(edge: &GraphEdge, request: &ContextRequest) -> bool {
    match edge.provenance.resolution {
        ResolutionState::Resolved => true,
        ResolutionState::Unresolved => request.policy.include_unresolved,
        ResolutionState::External => request.policy.include_external,
    }
}

fn context_node_allowed(node: &GraphNode, request: &ContextRequest) -> bool {
    match node.provenance.resolution {
        ResolutionState::Resolved => true,
        ResolutionState::Unresolved => request.policy.include_unresolved,
        ResolutionState::External => request.policy.include_external,
    }
}

fn edge_targets_node(edge: &GraphEdge) -> bool {
    matches!(edge.target, EdgeTarget::Node(_))
}

fn context_selection_kind(
    edge: &GraphEdge,
    current: &NodeId,
    next: &GraphNode,
) -> ContextSelectionKind {
    if edge.provenance.resolution == ResolutionState::Resolved
        && matches!(edge.kind.as_str(), "imports" | "re_exports" | "depends_on")
    {
        return ContextSelectionKind::ResolvedDependency;
    }
    if next.kind == "document" {
        return ContextSelectionKind::Documentation;
    }
    if matches!(
        next.kind.as_str(),
        "manifest"
            | "configuration"
            | "entry_point"
            | "cargo_workspace"
            | "cargo_package"
            | "cargo_target"
            | "dependency"
    ) {
        return ContextSelectionKind::Configuration;
    }
    if edge.kind == "declares_module"
        || edge.kind == "contains" && edge.source == *current && is_declaration_node(next)
    {
        return ContextSelectionKind::Declaration;
    }
    if edge.kind == "contains" {
        return ContextSelectionKind::Containment;
    }
    ContextSelectionKind::Relationship
}

fn is_declaration_node(node: &GraphNode) -> bool {
    matches!(
        node.kind.as_str(),
        "module"
            | "mod_declaration"
            | "struct"
            | "enum"
            | "union"
            | "trait"
            | "impl"
            | "function"
            | "type_alias"
            | "const"
            | "static"
            | "macro"
    )
}

fn context_selection_rank(kind: ContextSelectionKind) -> u8 {
    match kind {
        ContextSelectionKind::ExactSeed => 0,
        ContextSelectionKind::Containment => 1,
        ContextSelectionKind::Declaration => 2,
        ContextSelectionKind::ResolvedDependency => 3,
        ContextSelectionKind::Documentation => 4,
        ContextSelectionKind::Configuration => 5,
        ContextSelectionKind::Relationship => 6,
    }
}

fn sort_context_reasons(reasons: &mut Vec<ContextSelectionReason>) {
    reasons.sort_by(|left, right| {
        context_selection_rank(left.kind)
            .cmp(&context_selection_rank(right.kind))
            .then_with(|| {
                left.via_node
                    .as_ref()
                    .map(NodeId::as_str)
                    .cmp(&right.via_node.as_ref().map(NodeId::as_str))
            })
            .then_with(|| {
                left.via_edge
                    .as_ref()
                    .map(EdgeId::as_str)
                    .cmp(&right.via_edge.as_ref().map(EdgeId::as_str))
            })
    });
    reasons.dedup();
}

fn context_candidate_order(
    left: &ContextCandidate,
    right: &ContextCandidate,
) -> std::cmp::Ordering {
    let left_evidence = left
        .node
        .provenance
        .evidence
        .as_ref()
        .expect("context candidates without evidence are filtered before sorting");
    let right_evidence = right
        .node
        .provenance
        .evidence
        .as_ref()
        .expect("context candidates without evidence are filtered before sorting");
    let left_rank = left
        .selection_reasons
        .iter()
        .map(|reason| context_selection_rank(reason.kind))
        .min()
        .unwrap_or(u8::MAX);
    let right_rank = right
        .selection_reasons
        .iter()
        .map(|reason| context_selection_rank(reason.kind))
        .min()
        .unwrap_or(u8::MAX);
    left_rank
        .cmp(&right_rank)
        .then_with(|| left.depth.cmp(&right.depth))
        .then_with(|| left_evidence.path.cmp(&right_evidence.path))
        .then_with(|| {
            left_evidence
                .span
                .as_ref()
                .map(|span| span.start.byte_offset)
                .unwrap_or(u64::MAX)
                .cmp(
                    &right_evidence
                        .span
                        .as_ref()
                        .map(|span| span.start.byte_offset)
                        .unwrap_or(u64::MAX),
                )
        })
        .then_with(|| left.node.kind.cmp(&right.node.kind))
        .then_with(|| left.node.semantic_key.cmp(&right.node.semantic_key))
        .then_with(|| left.node.id.cmp(&right.node.id))
}

fn context_item(candidate: ContextCandidate) -> ContextItem {
    let evidence = candidate
        .node
        .provenance
        .evidence
        .clone()
        .expect("context candidates without evidence are filtered before materialization");
    ContextItem {
        node_id: candidate.node.id,
        kind: candidate.node.kind,
        semantic_key: candidate.node.semantic_key,
        path: evidence.path,
        span: evidence.span,
        content_identity: evidence.content_identity,
        provenance: candidate.node.provenance,
        selection_reasons: candidate.selection_reasons,
    }
}

fn set_context_truncation(current: &mut Option<TruncationReason>, next: TruncationReason) {
    fn priority(reason: TruncationReason) -> u8 {
        match reason {
            TruncationReason::Duration => 0,
            TruncationReason::Capability => 1,
            TruncationReason::Depth => 2,
            TruncationReason::Bytes => 3,
            TruncationReason::Results => 4,
        }
    }
    if current.is_none_or(|reason| priority(next) < priority(reason)) {
        *current = Some(next);
    }
}

fn neighborhood_node(node: GraphNode) -> NeighborhoodNode {
    let path = node
        .provenance
        .evidence
        .as_ref()
        .map(|evidence| evidence.path.clone());
    let span = node
        .provenance
        .evidence
        .as_ref()
        .and_then(|evidence| evidence.span.clone());
    NeighborhoodNode {
        id: node.id,
        kind: node.kind,
        semantic_key: node.semantic_key,
        path,
        span,
        provenance: node.provenance,
    }
}

fn neighborhood_edge(edge: GraphEdge) -> NeighborhoodEdge {
    NeighborhoodEdge {
        id: edge.id,
        kind: edge.kind,
        source: edge.source,
        target: edge.target,
        provenance: edge.provenance,
    }
}

fn adjacent_node(edge: &GraphEdge, current: &NodeId, direction: EdgeDirection) -> Option<NodeId> {
    match direction {
        EdgeDirection::Outgoing => match &edge.target {
            EdgeTarget::Node(target) if &edge.source == current => Some(target.clone()),
            _ => None,
        },
        EdgeDirection::Incoming => match &edge.target {
            EdgeTarget::Node(target) if target == current => Some(edge.source.clone()),
            _ => None,
        },
        EdgeDirection::Both => match &edge.target {
            EdgeTarget::Node(target) if &edge.source == current => Some(target.clone()),
            EdgeTarget::Node(target) if target == current => Some(edge.source.clone()),
            _ => None,
        },
    }
}

fn sqlite_deadline_exceeded(error: &SqliteError) -> bool {
    error.sqlite_error_code() == Some(ErrorCode::OperationInterrupted)
}

fn sqlite_query_error(error: SqliteError) -> QueryError {
    if sqlite_deadline_exceeded(&error) {
        duration_budget_exceeded_error()
    } else {
        backend_error()
    }
}

fn store_query_error(error: StoreError) -> QueryError {
    match error {
        StoreError::Database(error) => sqlite_query_error(error),
        _ => backend_error(),
    }
}

fn graph_string(value: &GraphValue) -> Option<&str> {
    match value {
        GraphValue::String(value) => Some(value),
        _ => None,
    }
}

fn page_info(
    reason: Option<TruncationReason>,
    returned_results: usize,
    returned_bytes: u64,
    explored_depth: u32,
    cursor: Option<(&str, &SnapshotId, &str, u64)>,
) -> Result<PageInfo, QueryError> {
    let truncation = reason.map(|reason| Truncation {
        reason,
        returned_results: u32::try_from(returned_results).unwrap_or(u32::MAX),
        returned_bytes,
        explored_depth,
    });
    let next_cursor = match (truncation.as_ref(), cursor, returned_results) {
        (Some(_), Some((operation, snapshot, fingerprint, offset)), 1..) => {
            Some(encode_cursor(operation, snapshot, fingerprint, offset)?)
        }
        _ => None,
    };
    Ok(PageInfo {
        next_cursor,
        truncation,
    })
}

mod support;
use support::*;

/// Loads the immutable file identities needed by a `SnapshotContent`
/// implementation without exposing the SQLite connection outside the graph
/// library boundary.
pub fn snapshot_file_descriptors(
    sidecar: &Sidecar,
    snapshot_id: &SnapshotId,
    paths: &BTreeSet<RepoPath>,
) -> Result<Vec<SourceFileDescriptor>, QueryError> {
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let stored = sidecar
            .connection()
            .query_row(
                "SELECT content_algorithm, content_digest, byte_length, file_mode \
                 FROM files WHERE snapshot_id = ?1 AND path = ?2",
                params![snapshot_id.as_str(), path.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| backend_error())?;
        let Some((algorithm, value, byte_len, file_mode)) = stored else {
            continue;
        };
        let byte_len = u64::try_from(byte_len).map_err(|_| backend_error())?;
        let file_mode = match file_mode {
            0 => SourceFileMode::Regular,
            1 => SourceFileMode::Executable,
            _ => return Err(backend_error()),
        };
        files.push(SourceFileDescriptor {
            path: path.clone(),
            content_identity: Digest::new(algorithm, value).map_err(|_| backend_error())?,
            byte_len,
            file_mode,
        });
    }
    Ok(files)
}

/// Loads the complete immutable file manifest for a snapshot. Overlay
/// composition uses this as the baseline descriptor set; source bodies remain
/// outside SQLite and are read only through a verified repository source.
pub fn all_snapshot_file_descriptors(
    sidecar: &Sidecar,
    snapshot_id: &SnapshotId,
) -> Result<Vec<SourceFileDescriptor>, QueryError> {
    let mut statement = sidecar
        .connection()
        .prepare(
            "SELECT path, content_algorithm, content_digest, byte_length, file_mode \
             FROM files WHERE snapshot_id = ?1 ORDER BY path",
        )
        .map_err(|_| backend_error())?;
    let rows = statement
        .query_map([snapshot_id.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(|_| backend_error())?;
    let mut files = Vec::new();
    for row in rows {
        let (path, algorithm, value, byte_len, file_mode) = row.map_err(|_| backend_error())?;
        files.push(SourceFileDescriptor {
            path: RepoPath::new(path).map_err(|_| backend_error())?,
            content_identity: Digest::new(algorithm, value).map_err(|_| backend_error())?,
            byte_len: u64::try_from(byte_len).map_err(|_| backend_error())?,
            file_mode: match file_mode {
                0 => SourceFileMode::Regular,
                1 => SourceFileMode::Executable,
                _ => return Err(backend_error()),
            },
        });
    }
    Ok(files)
}

pub fn default_budget(
    limits: &QueryLimitsConfig,
) -> Result<super::domain::QueryBudget, QueryError> {
    Ok(super::domain::QueryBudget::new(
        NonZeroU32::new(limits.max_results)
            .ok_or_else(|| invalid_request("query max_results must be greater than zero"))?,
        NonZeroU64::new(limits.max_bytes)
            .ok_or_else(|| invalid_request("query max_bytes must be greater than zero"))?,
        NonZeroU32::new(limits.max_depth)
            .ok_or_else(|| invalid_request("query max_depth must be greater than zero"))?,
        NonZeroU64::new(limits.max_duration_ms)
            .ok_or_else(|| invalid_request("query max_duration_ms must be greater than zero"))?,
        NonZeroU32::new(limits.max_diagnostics)
            .ok_or_else(|| invalid_request("query max_diagnostics must be greater than zero"))?,
    ))
}

#[cfg(test)]
#[path = "query_sqlite_tests.rs"]
mod tests;
