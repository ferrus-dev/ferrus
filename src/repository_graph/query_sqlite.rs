//! Bounded read-only SQLite implementation of the portable graph query contract.

use std::{
    collections::{BTreeMap, VecDeque},
    num::{NonZeroU32, NonZeroU64},
    time::Instant,
};

use rusqlite::{Row, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::{
    QUERY_WIRE_VERSION,
    config::QueryLimitsConfig,
    domain::{
        Availability, Confidence, Digest, EdgeId, EdgeTarget, ExtractorId, ExtractorIdentity,
        FactProvenance, Freshness, GraphEdge, GraphNode, GraphValue, NodeId, PageCursor, RepoPath,
        RepositoryRef, ResolutionState, SemanticKey, SnapshotId, SourceEvidence, SourcePosition,
        SourceSpan,
    },
    ports::GraphQuery,
    query::{
        ContextRequest, ContextResponse, DiagnosticSummary, EdgeDirection, FreshnessEnvelope,
        NeighborhoodData, NeighborhoodEdge, NeighborhoodNode, NeighborhoodRequest,
        NeighborhoodResponse, PageInfo, QueryError, QueryErrorCode, QueryResponse, SearchData,
        SearchHit, SearchRequest, SearchResponse, ShowData, ShowLookup, ShowRequest, ShowResponse,
        SnapshotSelector, SnapshotStatistics, StatusData, StatusRequest, StatusResponse,
        Truncation, TruncationReason,
    },
    sqlite::Sidecar,
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

pub struct SqliteGraphQuery<'a> {
    sidecar: &'a Sidecar,
    limits: QueryLimitsConfig,
    compared_manifest: Option<Digest>,
}

impl<'a> SqliteGraphQuery<'a> {
    pub fn new(
        sidecar: &'a Sidecar,
        limits: QueryLimitsConfig,
        compared_manifest: Option<Digest>,
    ) -> Self {
        Self {
            sidecar,
            limits,
            compared_manifest,
        }
    }

    fn resolve_scope(&self, scope: &super::query::QueryScope) -> Result<ResolvedScope, QueryError> {
        validate_wire_version(scope.wire_version)?;
        let budget = EffectiveBudget::new(&scope.budget, &self.limits)?;
        let (snapshot, published_view) = match &scope.snapshot {
            SnapshotSelector::Published(name) => {
                let view = self
                    .sidecar
                    .published_view(&scope.repository, name)
                    .map_err(|_| backend_error())?
                    .ok_or_else(not_built_error)?;
                let snapshot = self
                    .sidecar
                    .snapshot(&view.snapshot_id)
                    .map_err(|_| backend_error())?
                    .ok_or_else(backend_error)?;
                (snapshot, Some(name.clone()))
            }
            SnapshotSelector::Snapshot(id) => {
                let snapshot = self
                    .sidecar
                    .snapshot(id)
                    .map_err(|_| backend_error())?
                    .ok_or_else(snapshot_not_found_error)?;
                (snapshot, None)
            }
        };
        if snapshot.repository != scope.repository {
            return Err(snapshot_not_found_error());
        }
        let freshness = freshness(
            &snapshot.source_manifest_digest,
            self.compared_manifest.as_ref(),
        );
        Ok(ResolvedScope {
            repository: scope.repository.clone(),
            snapshot,
            published_view,
            freshness,
            budget,
        })
    }

    fn diagnostics(&self, snapshot: &SnapshotId) -> Result<DiagnosticSummary, QueryError> {
        self.sidecar
            .connection()
            .query_row(
                "SELECT \
                    SUM(CASE WHEN severity = 'info' THEN 1 ELSE 0 END), \
                    SUM(CASE WHEN severity = 'warning' THEN 1 ELSE 0 END), \
                    SUM(CASE WHEN severity = 'error' THEN 1 ELSE 0 END) \
                 FROM diagnostics WHERE snapshot_id = ?1",
                [snapshot.as_str()],
                |row| {
                    Ok(DiagnosticSummary {
                        info: unsigned(row.get::<_, Option<i64>>(0)?.unwrap_or(0))?,
                        warning: unsigned(row.get::<_, Option<i64>>(1)?.unwrap_or(0))?,
                        error: unsigned(row.get::<_, Option<i64>>(2)?.unwrap_or(0))?,
                    })
                },
            )
            .map_err(|_| backend_error())
    }

    fn statistics(&self, snapshot: &SnapshotId) -> Result<SnapshotStatistics, QueryError> {
        self.sidecar
            .connection()
            .query_row(
                "SELECT \
                    (SELECT COUNT(*) FROM files WHERE snapshot_id = ?1), \
                    (SELECT COUNT(*) FROM nodes WHERE snapshot_id = ?1), \
                    (SELECT COUNT(*) FROM edges WHERE snapshot_id = ?1)",
                [snapshot.as_str()],
                |row| {
                    Ok(SnapshotStatistics {
                        files: unsigned(row.get(0)?)?,
                        nodes: unsigned(row.get(1)?)?,
                        edges: unsigned(row.get(2)?)?,
                    })
                },
            )
            .map_err(|_| backend_error())
    }

    fn search_rows(
        &self,
        scope: &ResolvedScope,
        request: &SearchRequest,
        offset: u64,
    ) -> Result<Vec<(GraphNode, f64)>, QueryError> {
        let text = request.text.trim();
        let normalized = text.to_lowercase();
        let escaped = escape_like(&normalized);
        let prefix = format!("{escaped}%");
        let contains = format!("%{escaped}%");
        let kinds = serde_json::to_string(&request.node_kinds).map_err(|_| backend_error())?;
        let paths = serde_json::to_string(&request.paths).map_err(|_| backend_error())?;
        let sql = format!(
            "SELECT {NODE_COLUMNS}, \
                CASE \
                    WHEN normalized_name = ?2 THEN 1.0 \
                    WHEN normalized_name LIKE ?3 ESCAPE '\\' THEN 0.9 \
                    WHEN normalized_name LIKE ?4 ESCAPE '\\' THEN 0.8 \
                    WHEN lower(COALESCE(semantic_key, '')) LIKE ?4 ESCAPE '\\' THEN 0.7 \
                    ELSE 0.6 \
                END AS score \
             FROM nodes \
             WHERE snapshot_id = ?1 \
               AND (normalized_name = ?2 \
                    OR normalized_name LIKE ?3 ESCAPE '\\' \
                    OR normalized_name LIKE ?4 ESCAPE '\\' \
                    OR lower(COALESCE(semantic_key, '')) LIKE ?4 ESCAPE '\\' \
                    OR lower(COALESCE(evidence_path, '')) LIKE ?4 ESCAPE '\\') \
               AND (?5 = '[]' OR kind IN (SELECT value FROM json_each(?5))) \
               AND (?6 = '[]' OR EXISTS (\
                    SELECT 1 FROM json_each(?6) AS requested_path \
                    WHERE evidence_path = requested_path.value \
                       OR evidence_path LIKE requested_path.value || '/%'\
               )) \
             ORDER BY score DESC, normalized_name, id \
             LIMIT ?7 OFFSET ?8"
        );
        let limit = i64::from(scope.budget.max_results.saturating_add(1));
        let offset = i64::try_from(offset).map_err(|_| stale_cursor_error())?;
        let mut statement = self
            .sidecar
            .connection()
            .prepare(&sql)
            .map_err(|_| backend_error())?;
        let mut rows = statement
            .query(params![
                scope.snapshot.id.as_str(),
                normalized,
                prefix,
                contains,
                kinds,
                paths,
                limit,
                offset,
            ])
            .map_err(|_| backend_error())?;
        let mut found = Vec::new();
        while let Some(row) = rows.next().map_err(|_| backend_error())? {
            found.push((decode_node(row)?, value(row, 19)?));
        }
        Ok(found)
    }

    fn show_rows(
        &self,
        scope: &ResolvedScope,
        request: &ShowRequest,
        offset: u64,
    ) -> Result<Vec<GraphNode>, QueryError> {
        let (predicate, lookup) = match &request.lookup {
            ShowLookup::Node(id) => ("id = ?2", id.as_str()),
            ShowLookup::Symbol(key) => ("semantic_key = ?2", key.as_str()),
            ShowLookup::Path(path) => ("evidence_path = ?2", path.as_str()),
        };
        let sql = format!(
            "SELECT {NODE_COLUMNS} FROM nodes WHERE snapshot_id = ?1 AND {predicate} \
             ORDER BY kind, semantic_key, id LIMIT ?3 OFFSET ?4"
        );
        let mut statement = self
            .sidecar
            .connection()
            .prepare(&sql)
            .map_err(|_| backend_error())?;
        let mut rows = statement
            .query(params![
                scope.snapshot.id.as_str(),
                lookup,
                i64::from(scope.budget.max_results.saturating_add(1)),
                i64::try_from(offset).map_err(|_| stale_cursor_error())?,
            ])
            .map_err(|_| backend_error())?;
        let mut found = Vec::new();
        while let Some(row) = rows.next().map_err(|_| backend_error())? {
            found.push(decode_node(row)?);
        }
        Ok(found)
    }

    fn node(&self, snapshot: &SnapshotId, id: &NodeId) -> Result<Option<GraphNode>, QueryError> {
        let sql = format!("SELECT {NODE_COLUMNS} FROM nodes WHERE snapshot_id = ?1 AND id = ?2");
        let mut statement = self
            .sidecar
            .connection()
            .prepare(&sql)
            .map_err(|_| backend_error())?;
        let mut rows = statement
            .query(params![snapshot.as_str(), id.as_str()])
            .map_err(|_| backend_error())?;
        rows.next()
            .map_err(|_| backend_error())?
            .map(decode_node)
            .transpose()
    }

    fn edges(
        &self,
        snapshot: &SnapshotId,
        node: &NodeId,
        direction: EdgeDirection,
        kinds: &[String],
        limit: u32,
    ) -> Result<Vec<GraphEdge>, QueryError> {
        let predicate = match direction {
            EdgeDirection::Outgoing => "source_node_id = ?2",
            EdgeDirection::Incoming => "target_node_id = ?2",
            EdgeDirection::Both => "(source_node_id = ?2 OR target_node_id = ?2)",
        };
        let sql = format!(
            "SELECT {EDGE_COLUMNS} FROM edges \
             WHERE snapshot_id = ?1 AND {predicate} \
               AND (?3 = '[]' OR kind IN (SELECT value FROM json_each(?3))) \
             ORDER BY id LIMIT ?4"
        );
        let kinds = serde_json::to_string(kinds).map_err(|_| backend_error())?;
        let mut statement = self
            .sidecar
            .connection()
            .prepare(&sql)
            .map_err(|_| backend_error())?;
        let mut rows = statement
            .query(params![
                snapshot.as_str(),
                node.as_str(),
                kinds,
                i64::from(limit.saturating_add(1)),
            ])
            .map_err(|_| backend_error())?;
        let mut found = Vec::new();
        while let Some(row) = rows.next().map_err(|_| backend_error())? {
            found.push(decode_edge(row)?);
        }
        Ok(found)
    }
}

impl GraphQuery for SqliteGraphQuery<'_> {
    fn status(&self, request: &StatusRequest) -> Result<StatusResponse, QueryError> {
        validate_wire_version(request.scope.wire_version)?;
        EffectiveBudget::new(&request.scope.budget, &self.limits)?;
        let resolved = match self.resolve_scope(&request.scope) {
            Ok(resolved) => resolved,
            Err(error)
                if error.code == QueryErrorCode::NotBuilt
                    && matches!(request.scope.snapshot, SnapshotSelector::Published(_)) =>
            {
                let published_view = match &request.scope.snapshot {
                    SnapshotSelector::Published(name) => Some(name.clone()),
                    SnapshotSelector::Snapshot(_) => None,
                };
                return Ok(StatusResponse {
                    wire_version: QUERY_WIRE_VERSION,
                    repository: request.scope.repository.clone(),
                    snapshot_id: None,
                    freshness: FreshnessEnvelope {
                        freshness: Freshness::NotApplicable,
                        compared_manifest: self.compared_manifest.clone(),
                        reason_codes: vec!["not_built".to_string()],
                    },
                    diagnostics: DiagnosticSummary::default(),
                    data: StatusData {
                        availability: Availability::NotBuilt,
                        build_state: None,
                        published_view,
                        graph_model_version: None,
                        statistics: None,
                    },
                });
            }
            Err(error) => return Err(error),
        };
        let build_state = self
            .sidecar
            .build(&resolved.snapshot.completed_by)
            .map_err(|_| backend_error())?
            .map(|build| build.state);
        Ok(StatusResponse {
            wire_version: QUERY_WIRE_VERSION,
            repository: resolved.repository,
            snapshot_id: Some(resolved.snapshot.id.clone()),
            freshness: resolved.freshness,
            diagnostics: self.diagnostics(&resolved.snapshot.id)?,
            data: StatusData {
                availability: Availability::Available,
                build_state,
                published_view: resolved.published_view,
                graph_model_version: Some(resolved.snapshot.graph_model_version),
                statistics: Some(self.statistics(&resolved.snapshot.id)?),
            },
        })
    }

    fn search(&self, request: &SearchRequest) -> Result<SearchResponse, QueryError> {
        let started = Instant::now();
        let scope = self.resolve_scope(&request.scope)?;
        validate_search_request(request)?;
        let fingerprint = search_cursor_fingerprint(request)?;
        let offset = decode_cursor(
            request.page.cursor.as_ref(),
            "search",
            &scope.snapshot.id,
            &fingerprint,
        )?;
        let rows = self.search_rows(&scope, request, offset)?;
        let mut hits = Vec::new();
        let mut returned_bytes = 0_u64;
        let mut reason = None;
        for (node, score) in rows {
            if hits.len() >= scope.budget.max_results as usize {
                reason = Some(TruncationReason::Results);
                break;
            }
            if started.elapsed().as_millis() >= u128::from(scope.budget.max_duration_ms) {
                reason = Some(TruncationReason::Duration);
                break;
            }
            let hit = search_hit(node, score, request.text.trim());
            let bytes = serialized_len(&hit)?;
            if returned_bytes.saturating_add(bytes) > scope.budget.max_bytes {
                reason = Some(TruncationReason::Bytes);
                break;
            }
            returned_bytes = returned_bytes.saturating_add(bytes);
            hits.push(hit);
        }
        let page = page_info(
            reason,
            hits.len(),
            returned_bytes,
            0,
            Some((
                "search",
                &scope.snapshot.id,
                &fingerprint,
                offset + hits.len() as u64,
            )),
        )?;
        Ok(QueryResponse {
            wire_version: QUERY_WIRE_VERSION,
            repository: scope.repository,
            snapshot_id: scope.snapshot.id.clone(),
            freshness: scope.freshness,
            diagnostics: self.diagnostics(&scope.snapshot.id)?,
            page,
            data: SearchData { hits },
        })
    }

    fn show(&self, request: &ShowRequest) -> Result<ShowResponse, QueryError> {
        let started = Instant::now();
        let scope = self.resolve_scope(&request.scope)?;
        let fingerprint = show_cursor_fingerprint(request)?;
        let offset = decode_cursor(
            request.page.cursor.as_ref(),
            "show",
            &scope.snapshot.id,
            &fingerprint,
        )?;
        let rows = self.show_rows(&scope, request, offset)?;
        if rows.is_empty() && offset == 0 {
            return Err(invalid_request("graph lookup matched no nodes"));
        }
        let mut nodes = Vec::new();
        let mut returned_bytes = 0_u64;
        let mut reason = None;
        for node in rows {
            if nodes.len() >= scope.budget.max_results as usize {
                reason = Some(TruncationReason::Results);
                break;
            }
            if started.elapsed().as_millis() >= u128::from(scope.budget.max_duration_ms) {
                reason = Some(TruncationReason::Duration);
                break;
            }
            let bytes = serialized_len(&node)?;
            if returned_bytes.saturating_add(bytes) > scope.budget.max_bytes {
                reason = Some(TruncationReason::Bytes);
                break;
            }
            returned_bytes = returned_bytes.saturating_add(bytes);
            nodes.push(node);
        }
        let page = page_info(
            reason,
            nodes.len(),
            returned_bytes,
            0,
            Some((
                "show",
                &scope.snapshot.id,
                &fingerprint,
                offset + nodes.len() as u64,
            )),
        )?;
        Ok(QueryResponse {
            wire_version: QUERY_WIRE_VERSION,
            repository: scope.repository,
            snapshot_id: scope.snapshot.id.clone(),
            freshness: scope.freshness,
            diagnostics: self.diagnostics(&scope.snapshot.id)?,
            page,
            data: ShowData { nodes },
        })
    }

    fn neighborhood(
        &self,
        request: &NeighborhoodRequest,
    ) -> Result<NeighborhoodResponse, QueryError> {
        let started = Instant::now();
        let scope = self.resolve_scope(&request.scope)?;
        if request.page.cursor.is_some() {
            return Err(invalid_request(
                "neighborhood cursors are not supported by the local phase 1 backend",
            ));
        }
        if request.roots.is_empty() || request.roots.len() > MAX_FILTERS {
            return Err(invalid_request("neighborhood requires 1..=32 roots"));
        }
        validate_filters(&request.edge_kinds, 0)?;

        let mut nodes = BTreeMap::new();
        let mut edges = BTreeMap::new();
        let mut frontier = VecDeque::new();
        let mut returned_bytes = 0_u64;
        let mut reason = None;
        for root in &request.roots {
            let node = self
                .node(&scope.snapshot.id, root)?
                .ok_or_else(|| invalid_request("neighborhood root was not found"))?;
            let item = neighborhood_node(node);
            let bytes = serialized_len(&item)?;
            if nodes.len() >= scope.budget.max_results as usize
                || returned_bytes.saturating_add(bytes) > scope.budget.max_bytes
            {
                reason = Some(if nodes.len() >= scope.budget.max_results as usize {
                    TruncationReason::Results
                } else {
                    TruncationReason::Bytes
                });
                break;
            }
            returned_bytes = returned_bytes.saturating_add(bytes);
            nodes.insert(item.id.clone(), item);
            frontier.push_back((root.clone(), 0_u32));
        }

        let mut explored_depth = 0_u32;
        while let Some((current, depth)) = frontier.pop_front() {
            explored_depth = explored_depth.max(depth);
            if reason.is_some() {
                break;
            }
            if started.elapsed().as_millis() >= u128::from(scope.budget.max_duration_ms) {
                reason = Some(TruncationReason::Duration);
                break;
            }
            if depth >= scope.budget.max_depth {
                if !self
                    .edges(
                        &scope.snapshot.id,
                        &current,
                        request.direction,
                        &request.edge_kinds,
                        1,
                    )?
                    .is_empty()
                {
                    reason = Some(TruncationReason::Depth);
                }
                continue;
            }
            let remaining = scope
                .budget
                .max_results
                .saturating_sub((nodes.len() + edges.len()) as u32);
            if remaining == 0 {
                reason = Some(TruncationReason::Results);
                break;
            }
            for edge in self.edges(
                &scope.snapshot.id,
                &current,
                request.direction,
                &request.edge_kinds,
                remaining,
            )? {
                if edges.contains_key(&edge.id) {
                    continue;
                }
                if nodes.len() + edges.len() >= scope.budget.max_results as usize {
                    reason = Some(TruncationReason::Results);
                    break;
                }
                let item = neighborhood_edge(edge.clone());
                let bytes = serialized_len(&item)?;
                if returned_bytes.saturating_add(bytes) > scope.budget.max_bytes {
                    reason = Some(TruncationReason::Bytes);
                    break;
                }
                returned_bytes = returned_bytes.saturating_add(bytes);
                edges.insert(item.id.clone(), item);

                let Some(next_id) = adjacent_node(&edge, &current, request.direction) else {
                    continue;
                };
                if nodes.contains_key(&next_id) {
                    continue;
                }
                if nodes.len() + edges.len() >= scope.budget.max_results as usize {
                    reason = Some(TruncationReason::Results);
                    break;
                }
                let next = self
                    .node(&scope.snapshot.id, &next_id)?
                    .ok_or_else(backend_error)?;
                let next = neighborhood_node(next);
                let bytes = serialized_len(&next)?;
                if returned_bytes.saturating_add(bytes) > scope.budget.max_bytes {
                    reason = Some(TruncationReason::Bytes);
                    break;
                }
                returned_bytes = returned_bytes.saturating_add(bytes);
                nodes.insert(next.id.clone(), next);
                frontier.push_back((next_id, depth.saturating_add(1)));
            }
        }

        let page = page_info(
            reason,
            nodes.len() + edges.len(),
            returned_bytes,
            explored_depth,
            None,
        )?;
        Ok(QueryResponse {
            wire_version: QUERY_WIRE_VERSION,
            repository: scope.repository,
            snapshot_id: scope.snapshot.id.clone(),
            freshness: scope.freshness,
            diagnostics: self.diagnostics(&scope.snapshot.id)?,
            page,
            data: NeighborhoodData {
                nodes: nodes.into_values().collect(),
                edges: edges.into_values().collect(),
            },
        })
    }

    fn context(&self, _request: &ContextRequest) -> Result<ContextResponse, QueryError> {
        Err(invalid_request(
            "ranked repository context is not available until repository graph phase 2",
        ))
    }
}

struct ResolvedScope {
    repository: RepositoryRef,
    snapshot: super::domain::GraphSnapshot,
    published_view: Option<super::domain::PublishedViewName>,
    freshness: FreshnessEnvelope,
    budget: EffectiveBudget,
}

#[derive(Clone, Copy)]
struct EffectiveBudget {
    max_results: u32,
    max_bytes: u64,
    max_depth: u32,
    max_duration_ms: u64,
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
        {
            return Err(QueryError {
                wire_version: QUERY_WIRE_VERSION,
                code: QueryErrorCode::BackendUnavailable,
                message: "repository graph query limits are invalid".to_string(),
                retryable: false,
                details: BTreeMap::new(),
            });
        }
        Ok(Self {
            max_results: requested.max_results.get().min(service.max_results),
            max_bytes: requested.max_bytes.get().min(service.max_bytes),
            max_depth: requested.max_depth.get().min(service.max_depth),
            max_duration_ms: requested.max_duration_ms.get().min(service.max_duration_ms),
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

fn freshness(expected: &Digest, actual: Option<&Digest>) -> FreshnessEnvelope {
    match actual {
        Some(actual) if actual == expected => FreshnessEnvelope {
            freshness: Freshness::Fresh,
            compared_manifest: Some(actual.clone()),
            reason_codes: vec![],
        },
        Some(actual) => FreshnessEnvelope {
            freshness: Freshness::Stale,
            compared_manifest: Some(actual.clone()),
            reason_codes: vec!["source_manifest_changed".to_string()],
        },
        None => FreshnessEnvelope {
            freshness: Freshness::Unknown,
            compared_manifest: None,
            reason_codes: vec!["source_not_compared".to_string()],
        },
    }
}

fn search_hit(node: GraphNode, score: f64, query: &str) -> SearchHit {
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
        score,
        matched_fields,
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
    let next_cursor = match (truncation.as_ref(), cursor) {
        (Some(_), Some((operation, snapshot, fingerprint, offset))) => {
            Some(encode_cursor(operation, snapshot, fingerprint, offset)?)
        }
        _ => None,
    };
    Ok(PageInfo {
        next_cursor,
        truncation,
    })
}

#[derive(Serialize, Deserialize)]
struct CursorPayload {
    version: u32,
    operation: String,
    snapshot_id: SnapshotId,
    query_fingerprint: String,
    offset: u64,
}

fn encode_cursor(
    operation: &str,
    snapshot: &SnapshotId,
    query_fingerprint: &str,
    offset: u64,
) -> Result<PageCursor, QueryError> {
    let payload = CursorPayload {
        version: 2,
        operation: operation.to_string(),
        snapshot_id: snapshot.clone(),
        query_fingerprint: query_fingerprint.to_string(),
        offset,
    };
    let bytes = serde_json::to_vec(&payload).map_err(|_| backend_error())?;
    PageCursor::new(format!("cursor:{}", hex(&bytes))).map_err(|_| backend_error())
}

fn decode_cursor(
    cursor: Option<&PageCursor>,
    operation: &str,
    snapshot: &SnapshotId,
    query_fingerprint: &str,
) -> Result<u64, QueryError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let encoded = cursor
        .as_str()
        .strip_prefix("cursor:")
        .ok_or_else(stale_cursor_error)?;
    if encoded.len() > 4096 || encoded.len() % 2 != 0 {
        return Err(stale_cursor_error());
    }
    let bytes = unhex(encoded).ok_or_else(stale_cursor_error)?;
    let payload: CursorPayload =
        serde_json::from_slice(&bytes).map_err(|_| stale_cursor_error())?;
    if payload.version != 2
        || payload.operation != operation
        || payload.snapshot_id != *snapshot
        || payload.query_fingerprint != query_fingerprint
    {
        return Err(stale_cursor_error());
    }
    Ok(payload.offset)
}

#[derive(Serialize)]
struct SearchCursorParameters<'a> {
    text: String,
    node_kinds: Vec<&'a str>,
    paths: Vec<&'a str>,
}

fn search_cursor_fingerprint(request: &SearchRequest) -> Result<String, QueryError> {
    let mut node_kinds = request
        .node_kinds
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    node_kinds.sort_unstable();
    node_kinds.dedup();
    let mut paths = request
        .paths
        .iter()
        .map(RepoPath::as_str)
        .collect::<Vec<_>>();
    paths.sort_unstable();
    paths.dedup();
    cursor_fingerprint(
        "search",
        &SearchCursorParameters {
            text: request.text.trim().to_lowercase(),
            node_kinds,
            paths,
        },
    )
}

fn show_cursor_fingerprint(request: &ShowRequest) -> Result<String, QueryError> {
    cursor_fingerprint("show", &request.lookup)
}

fn cursor_fingerprint<T: Serialize>(operation: &str, parameters: &T) -> Result<String, QueryError> {
    let bytes = serde_json::to_vec(parameters).map_err(|_| backend_error())?;
    let mut hasher = Sha256::new();
    hasher.update(b"ferrus.repository-graph.cursor-parameters.v1\0");
    hasher.update(operation.as_bytes());
    hasher.update(b"\0");
    hasher.update(bytes);
    Ok(hex(&hasher.finalize()))
}

fn decode_node(row: &Row<'_>) -> Result<GraphNode, QueryError> {
    let snapshot_id = SnapshotId::new(value::<String>(row, 0)?).map_err(|_| backend_error())?;
    let id = NodeId::new(value::<String>(row, 1)?).map_err(|_| backend_error())?;
    let kind = value(row, 2)?;
    let semantic_key = value::<Option<String>>(row, 3)?
        .map(SemanticKey::new)
        .transpose()
        .map_err(|_| backend_error())?;
    let provenance = decode_provenance(row, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 15, 16, 17, 18)?;
    let properties =
        serde_json::from_str(&value::<String>(row, 14)?).map_err(|_| backend_error())?;
    Ok(GraphNode {
        snapshot_id,
        id,
        kind,
        semantic_key,
        provenance,
        properties,
    })
}

fn decode_edge(row: &Row<'_>) -> Result<GraphEdge, QueryError> {
    let snapshot_id = SnapshotId::new(value::<String>(row, 0)?).map_err(|_| backend_error())?;
    let id = EdgeId::new(value::<String>(row, 1)?).map_err(|_| backend_error())?;
    let kind = value(row, 2)?;
    let source = NodeId::new(value::<String>(row, 3)?).map_err(|_| backend_error())?;
    let target_node = value::<Option<String>>(row, 4)?;
    let external_target = value::<Option<String>>(row, 5)?;
    let provenance = decode_provenance(row, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 17, 18, 19, 20)?;
    let target = match (target_node, external_target, provenance.resolution) {
        (Some(target), None, _) => {
            EdgeTarget::Node(NodeId::new(target).map_err(|_| backend_error())?)
        }
        (None, Some(target), ResolutionState::External) => EdgeTarget::External(target),
        (None, Some(target), _) => EdgeTarget::Unresolved(target),
        _ => return Err(backend_error()),
    };
    let properties =
        serde_json::from_str(&value::<String>(row, 16)?).map_err(|_| backend_error())?;
    Ok(GraphEdge {
        snapshot_id,
        id,
        kind,
        source,
        target,
        provenance,
        properties,
    })
}

#[allow(clippy::too_many_arguments)]
fn decode_provenance(
    row: &Row<'_>,
    extractor_id: usize,
    extractor_version: usize,
    contract_version: usize,
    resolution: usize,
    confidence_index: usize,
    evidence_path: usize,
    evidence_algorithm: usize,
    evidence_digest: usize,
    start_byte: usize,
    end_byte: usize,
    start_line: usize,
    start_column: usize,
    end_line: usize,
    end_column: usize,
) -> Result<FactProvenance, QueryError> {
    let resolution = resolution_state(&value::<String>(row, resolution)?)?;
    let confidence = confidence(&value::<String>(row, confidence_index)?)?;
    let evidence = match value::<Option<String>>(row, evidence_path)? {
        None => None,
        Some(path) => {
            let algorithm =
                value::<Option<String>>(row, evidence_algorithm)?.ok_or_else(backend_error)?;
            let digest =
                value::<Option<String>>(row, evidence_digest)?.ok_or_else(backend_error)?;
            Some(SourceEvidence {
                path: RepoPath::new(path).map_err(|_| backend_error())?,
                content_identity: Digest::new(algorithm, digest).map_err(|_| backend_error())?,
                span: decode_span(
                    row,
                    start_byte,
                    end_byte,
                    start_line,
                    start_column,
                    end_line,
                    end_column,
                )?,
            })
        }
    };
    Ok(FactProvenance {
        extractor: ExtractorIdentity {
            id: ExtractorId::new(value::<String>(row, extractor_id)?)
                .map_err(|_| backend_error())?,
            version: value(row, extractor_version)?,
            contract_version: value(row, contract_version)?,
        },
        evidence,
        resolution,
        confidence,
    })
}

#[allow(clippy::too_many_arguments)]
fn decode_span(
    row: &Row<'_>,
    start_byte: usize,
    end_byte: usize,
    start_line: usize,
    start_column: usize,
    end_line: usize,
    end_column: usize,
) -> Result<Option<SourceSpan>, QueryError> {
    let start = value::<Option<i64>>(row, start_byte)?;
    let end = value::<Option<i64>>(row, end_byte)?;
    match (start, end) {
        (None, None) => Ok(None),
        (Some(start), Some(end)) => Ok(Some(SourceSpan {
            start: SourcePosition {
                byte_offset: unsigned(start).map_err(|_| backend_error())?,
                line: optional_u32(value(row, start_line)?)?,
                column: optional_u32(value(row, start_column)?)?,
            },
            end: SourcePosition {
                byte_offset: unsigned(end).map_err(|_| backend_error())?,
                line: optional_u32(value(row, end_line)?)?,
                column: optional_u32(value(row, end_column)?)?,
            },
        })),
        _ => Err(backend_error()),
    }
}

fn resolution_state(value: &str) -> Result<ResolutionState, QueryError> {
    match value {
        "resolved" => Ok(ResolutionState::Resolved),
        "unresolved" => Ok(ResolutionState::Unresolved),
        "external" => Ok(ResolutionState::External),
        _ => Err(backend_error()),
    }
}

fn confidence(value: &str) -> Result<Confidence, QueryError> {
    match value {
        "exact" => Ok(Confidence::Exact),
        "high" => Ok(Confidence::High),
        "medium" => Ok(Confidence::Medium),
        "low" => Ok(Confidence::Low),
        _ => Err(backend_error()),
    }
}

fn optional_u32(value: Option<i64>) -> Result<Option<u32>, QueryError> {
    value
        .map(u32::try_from)
        .transpose()
        .map_err(|_| backend_error())
}

fn value<T: rusqlite::types::FromSql>(row: &Row<'_>, index: usize) -> Result<T, QueryError> {
    row.get(index).map_err(|_| backend_error())
}

fn unsigned(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))
}

fn serialized_len<T: Serialize>(value: &T) -> Result<u64, QueryError> {
    let len = serde_json::to_vec(value)
        .map_err(|_| backend_error())?
        .len();
    Ok(u64::try_from(len).unwrap_or(u64::MAX))
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn unhex(value: &str) -> Option<Vec<u8>> {
    fn nibble(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        }
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| Some((nibble(pair[0])? << 4) | nibble(pair[1])?))
        .collect()
}

fn invalid_request(message: &'static str) -> QueryError {
    QueryError {
        wire_version: QUERY_WIRE_VERSION,
        code: QueryErrorCode::InvalidRequest,
        message: message.to_string(),
        retryable: false,
        details: BTreeMap::new(),
    }
}

fn backend_error() -> QueryError {
    QueryError {
        wire_version: QUERY_WIRE_VERSION,
        code: QueryErrorCode::BackendUnavailable,
        message: "repository graph storage is unavailable or inconsistent".to_string(),
        retryable: true,
        details: BTreeMap::new(),
    }
}

fn not_built_error() -> QueryError {
    QueryError {
        wire_version: QUERY_WIRE_VERSION,
        code: QueryErrorCode::NotBuilt,
        message: "repository graph is not built; run `ferrus graph index`".to_string(),
        retryable: false,
        details: BTreeMap::new(),
    }
}

fn snapshot_not_found_error() -> QueryError {
    QueryError {
        wire_version: QUERY_WIRE_VERSION,
        code: QueryErrorCode::SnapshotNotFound,
        message: "repository graph snapshot was not found".to_string(),
        retryable: false,
        details: BTreeMap::new(),
    }
}

fn stale_cursor_error() -> QueryError {
    QueryError {
        wire_version: QUERY_WIRE_VERSION,
        code: QueryErrorCode::StaleCursor,
        message: "repository graph cursor does not match this query snapshot or parameters"
            .to_string(),
        retryable: false,
        details: BTreeMap::new(),
    }
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
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository_graph::{
        config::RepositoryGraphConfig,
        domain::{BuildId, PublishedViewName, RepositoryId, RepositoryNamespace},
        index::{IndexCoordinator, IndexRequest, active_extractor_identities},
        source::{FilesystemRepositorySource, SourceDiscoveryContext},
        sqlite::{OpenSidecarResult, open_for_build_at},
    };

    fn repository() -> RepositoryRef {
        RepositoryRef {
            namespace: RepositoryNamespace::new("local:test").unwrap(),
            repository_id: RepositoryId::new("root").unwrap(),
        }
    }

    fn indexed_fixture() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        Sidecar,
        RepositoryGraphConfig,
    ) {
        let source_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(source_dir.path().join("src")).unwrap();
        std::fs::write(
            source_dir.path().join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.1.0'\n",
        )
        .unwrap();
        std::fs::write(
            source_dir.path().join("src/lib.rs"),
            "pub struct RuntimeTaskContext;\npub fn claim_task() {}\n",
        )
        .unwrap();
        let config = RepositoryGraphConfig::default();
        let identities = active_extractor_identities(&config).unwrap();
        let context =
            SourceDiscoveryContext::from_config(repository(), &config, &identities).unwrap();
        let source = FilesystemRepositorySource::discover(source_dir.path(), context).unwrap();
        let sidecar_dir = tempfile::tempdir().unwrap();
        let OpenSidecarResult::Ready(mut sidecar) =
            open_for_build_at(&sidecar_dir.path().join("repo-graph.db")).unwrap()
        else {
            panic!("new sidecar unexpectedly requires rebuild");
        };
        IndexCoordinator::new(&mut sidecar)
            .index(
                &source,
                &config,
                IndexRequest {
                    build_id: BuildId::new("build-query").unwrap(),
                    view_name: PublishedViewName::new("canonical").unwrap(),
                    force_full: false,
                },
            )
            .unwrap();
        (source_dir, sidecar_dir, sidecar, config)
    }

    fn scope(config: &RepositoryGraphConfig) -> super::super::query::QueryScope {
        super::super::query::QueryScope::v1(
            repository(),
            SnapshotSelector::Published(PublishedViewName::new("canonical").unwrap()),
            default_budget(&config.query_limits).unwrap(),
        )
    }

    #[test]
    fn search_show_and_neighborhood_return_evidence_and_provenance() {
        let (_source, _sidecar_dir, sidecar, config) = indexed_fixture();
        let snapshot = sidecar
            .published_snapshot(&repository(), &PublishedViewName::new("canonical").unwrap())
            .unwrap()
            .unwrap();
        let query = SqliteGraphQuery::new(
            &sidecar,
            config.query_limits.clone(),
            Some(snapshot.source_manifest_digest),
        );
        let search = query
            .search(&SearchRequest {
                scope: scope(&config),
                text: "RuntimeTaskContext".to_string(),
                node_kinds: vec!["struct".to_string()],
                paths: vec![],
                page: super::super::query::PageRequest { cursor: None },
            })
            .unwrap();
        assert_eq!(search.freshness.freshness, Freshness::Fresh);
        assert!(!search.data.hits.is_empty());
        let hit = &search.data.hits[0];
        assert_eq!(hit.path.as_ref().unwrap().as_str(), "src/lib.rs");
        assert!(hit.span.is_some());
        assert_eq!(hit.provenance.resolution, ResolutionState::Resolved);

        let shown = query
            .show(&ShowRequest {
                scope: scope(&config),
                lookup: ShowLookup::Node(hit.node_id.clone()),
                page: super::super::query::PageRequest { cursor: None },
            })
            .unwrap();
        assert_eq!(shown.data.nodes.len(), 1);

        let neighborhood = query
            .neighborhood(&NeighborhoodRequest {
                scope: scope(&config),
                roots: vec![hit.node_id.clone()],
                direction: EdgeDirection::Both,
                edge_kinds: vec![],
                page: super::super::query::PageRequest { cursor: None },
            })
            .unwrap();
        assert!(
            neighborhood
                .data
                .nodes
                .iter()
                .any(|node| node.id == hit.node_id)
        );
    }

    #[test]
    fn service_limits_cap_results_and_cursors_are_query_bound() {
        let (_source, _sidecar_dir, sidecar, mut config) = indexed_fixture();
        config.query_limits.max_results = 1;
        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
        let first = query
            .search(&SearchRequest {
                scope: scope(&config),
                text: "rust".to_string(),
                node_kinds: vec![],
                paths: vec![],
                page: super::super::query::PageRequest { cursor: None },
            })
            .unwrap();
        assert_eq!(first.data.hits.len(), 1);
        assert_eq!(
            first.page.truncation.as_ref().unwrap().reason,
            TruncationReason::Results
        );
        assert!(first.page.next_cursor.is_some());
        let cursor = first.page.next_cursor.unwrap();

        query
            .search(&SearchRequest {
                scope: scope(&config),
                text: "rust".to_string(),
                node_kinds: vec![],
                paths: vec![],
                page: super::super::query::PageRequest {
                    cursor: Some(cursor.clone()),
                },
            })
            .unwrap();

        for (text, node_kinds, paths) in [
            ("RuntimeTaskContext", vec![], vec![]),
            ("rust", vec!["struct".to_string()], vec![]),
            ("rust", vec![], vec![RepoPath::new("src").unwrap()]),
        ] {
            let error = query
                .search(&SearchRequest {
                    scope: scope(&config),
                    text: text.to_string(),
                    node_kinds,
                    paths,
                    page: super::super::query::PageRequest {
                        cursor: Some(cursor.clone()),
                    },
                })
                .unwrap_err();
            assert_eq!(error.code, QueryErrorCode::StaleCursor);
        }

        let shown = query
            .show(&ShowRequest {
                scope: scope(&config),
                lookup: ShowLookup::Path(RepoPath::new("src/lib.rs").unwrap()),
                page: super::super::query::PageRequest { cursor: None },
            })
            .unwrap();
        let show_cursor = shown.page.next_cursor.unwrap();
        let error = query
            .show(&ShowRequest {
                scope: scope(&config),
                lookup: ShowLookup::Path(RepoPath::new("Cargo.toml").unwrap()),
                page: super::super::query::PageRequest {
                    cursor: Some(show_cursor),
                },
            })
            .unwrap_err();
        assert_eq!(error.code, QueryErrorCode::StaleCursor);

        let error = query
            .search(&SearchRequest {
                scope: scope(&config),
                text: "rust".to_string(),
                node_kinds: vec![],
                paths: vec![],
                page: super::super::query::PageRequest {
                    cursor: Some(PageCursor::new("cursor:00").unwrap()),
                },
            })
            .unwrap_err();
        assert_eq!(error.code, QueryErrorCode::StaleCursor);
    }

    #[test]
    fn status_reports_counts_and_missing_show_is_actionable() {
        let (_source, _sidecar_dir, sidecar, config) = indexed_fixture();
        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
        let status = query
            .status(&StatusRequest {
                scope: scope(&config),
            })
            .unwrap();
        let statistics = status.data.statistics.unwrap();
        assert_eq!(statistics.files, 2);
        assert!(statistics.nodes > 0);
        assert!(statistics.edges > 0);

        let error = query
            .show(&ShowRequest {
                scope: scope(&config),
                lookup: ShowLookup::Node(NodeId::new("node:missing").unwrap()),
                page: super::super::query::PageRequest { cursor: None },
            })
            .unwrap_err();
        assert_eq!(error.code, QueryErrorCode::InvalidRequest);
    }
}
