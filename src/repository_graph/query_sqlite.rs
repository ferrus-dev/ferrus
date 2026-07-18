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
    ports::{GraphQuery, SourceManifest},
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

impl<'a> SqliteGraphQuery<'a> {
    pub fn new(
        sidecar: &'a Sidecar,
        limits: QueryLimitsConfig,
        freshness_comparison: Option<FreshnessComparison>,
    ) -> Self {
        Self {
            sidecar,
            limits,
            freshness_comparison,
        }
    }

    fn resolve_scope(&self, scope: &super::query::QueryScope) -> Result<ResolvedScope, QueryError> {
        validate_wire_version(scope.wire_version)?;
        let budget = EffectiveBudget::new(&scope.budget, &self.limits)?;
        let (snapshot, published_view, source_revision_id) = match &scope.snapshot {
            SnapshotSelector::Published(name) => {
                let view = self
                    .sidecar
                    .published_view(&scope.repository, name)
                    .map_err(store_query_error)?
                    .ok_or_else(|| self.unpublished_index_error(&scope.repository))?;
                let snapshot = self
                    .sidecar
                    .snapshot(&view.snapshot_id)
                    .map_err(store_query_error)?
                    .ok_or_else(backend_error)?;
                let source_revision_id = self
                    .sidecar
                    .build(&view.build_id)
                    .map_err(store_query_error)?
                    .ok_or_else(backend_error)?
                    .source_revision_id;
                (snapshot, Some(name.clone()), source_revision_id)
            }
            SnapshotSelector::Snapshot(id) => {
                let snapshot = self
                    .sidecar
                    .snapshot(id)
                    .map_err(store_query_error)?
                    .ok_or_else(snapshot_not_found_error)?;
                let source_revision_id = snapshot.source_revision_id.clone();
                (snapshot, None, source_revision_id)
            }
        };
        if snapshot.repository != scope.repository {
            return Err(snapshot_not_found_error());
        }
        let freshness = freshness(&snapshot, self.freshness_comparison.as_ref());
        Ok(ResolvedScope {
            repository: scope.repository.clone(),
            snapshot,
            published_view,
            source_revision_id,
            freshness,
            budget,
        })
    }

    fn latest_build(&self, repository: &RepositoryRef) -> Result<Option<GraphBuild>, QueryError> {
        let build_id = self
            .sidecar
            .connection()
            .query_row(
                "SELECT id FROM index_builds \
                 WHERE repository_namespace = ?1 AND repository_id = ?2 \
                 ORDER BY started_at DESC, id DESC LIMIT 1",
                params![
                    repository.namespace.as_str(),
                    repository.repository_id.as_str(),
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(sqlite_query_error)?
            .map(BuildId::new)
            .transpose()
            .map_err(|_| backend_error())?;
        build_id
            .map(|id| self.sidecar.build(&id).map_err(store_query_error))
            .transpose()
            .map(Option::flatten)
    }

    fn unpublished_index_error(&self, repository: &RepositoryRef) -> QueryError {
        match self.latest_build(repository) {
            Ok(Some(build)) if build.state == BuildState::Building => index_building_error(),
            Ok(Some(build)) if build.state == BuildState::Failed => index_failed_error(),
            _ => not_built_error(),
        }
    }

    fn status_at(
        &self,
        request: &StatusRequest,
        started: Instant,
    ) -> Result<StatusResponse, QueryError> {
        validate_wire_version(request.scope.wire_version)?;
        let budget = EffectiveBudget::new(&request.scope.budget, &self.limits)?;
        let duration = Duration::from_millis(budget.max_duration_ms);
        let resolved = match self.resolve_scope(&request.scope) {
            Ok(resolved) => resolved,
            Err(error)
                if matches!(
                    error.code,
                    QueryErrorCode::NotBuilt
                        | QueryErrorCode::IndexBuilding
                        | QueryErrorCode::IndexFailed
                ) && matches!(request.scope.snapshot, SnapshotSelector::Published(_)) =>
            {
                let latest_build = self.latest_build(&request.scope.repository)?;
                let published_view = match &request.scope.snapshot {
                    SnapshotSelector::Published(name) => Some(name.clone()),
                    SnapshotSelector::Snapshot(_) => None,
                };
                return Ok(StatusResponse {
                    wire_version: QUERY_WIRE_VERSION,
                    repository: request.scope.repository.clone(),
                    snapshot_id: None,
                    source_revision: None,
                    freshness: FreshnessEnvelope {
                        freshness: Freshness::NotApplicable,
                        compared_manifest: self
                            .freshness_comparison
                            .as_ref()
                            .map(|comparison| comparison.source_manifest_digest.clone()),
                        reason_codes: vec!["not_built".to_string()],
                    },
                    diagnostics: if started.elapsed() >= duration {
                        truncated_diagnostics()
                    } else {
                        DiagnosticsEnvelope::default()
                    },
                    page: page_info(
                        (started.elapsed() >= duration).then_some(TruncationReason::Duration),
                        0,
                        0,
                        0,
                        None,
                    )?,
                    data: StatusData {
                        availability: Availability::NotBuilt,
                        build_state: latest_build.as_ref().map(|build| build.state),
                        build_id: latest_build.as_ref().map(|build| build.id.clone()),
                        published_view,
                        graph_model_version: None,
                        statistics: None,
                        recommended_action: Some(match error.code {
                            QueryErrorCode::IndexBuilding => RetrievalAction::WaitForBuild,
                            QueryErrorCode::IndexFailed => RetrievalAction::RetryIndex,
                            _ => RetrievalAction::Index,
                        }),
                    },
                });
            }
            Err(error) => return Err(error),
        };
        if started.elapsed() >= duration {
            return Ok(available_status_response(
                &resolved,
                None,
                truncated_diagnostics(),
                None,
                true,
            ));
        }
        let deadline = QueryDeadline::install(self.sidecar.connection(), started, duration)?;
        let latest_build = match self.latest_build(&resolved.repository) {
            Ok(build) => build,
            Err(error) if error.code == QueryErrorCode::BudgetExceeded => {
                return Ok(available_status_response(
                    &resolved,
                    None,
                    truncated_diagnostics(),
                    None,
                    true,
                ));
            }
            Err(error) => return Err(error),
        };
        let diagnostics =
            match self.diagnostics(&resolved.snapshot.id, resolved.budget.max_diagnostics) {
                Ok(diagnostics) => diagnostics,
                Err(error) if error.code == QueryErrorCode::BudgetExceeded => {
                    return Ok(available_status_response(
                        &resolved,
                        latest_build.as_ref(),
                        truncated_diagnostics(),
                        None,
                        true,
                    ));
                }
                Err(error) => return Err(error),
            };
        let statistics = match self.statistics(&resolved.snapshot.id) {
            Ok(statistics) => statistics,
            Err(error) if error.code == QueryErrorCode::BudgetExceeded => {
                return Ok(available_status_response(
                    &resolved,
                    latest_build.as_ref(),
                    diagnostics,
                    None,
                    true,
                ));
            }
            Err(error) => return Err(error),
        };
        drop(deadline);
        Ok(available_status_response(
            &resolved,
            latest_build.as_ref(),
            diagnostics,
            Some(statistics),
            started.elapsed() >= duration,
        ))
    }

    fn diagnostics(
        &self,
        snapshot: &SnapshotId,
        max_diagnostics: u32,
    ) -> Result<DiagnosticsEnvelope, QueryError> {
        let summary = self
            .sidecar
            .connection()
            .query_row(
                "SELECT \
                    SUM(CASE WHEN severity = 'info' THEN 1 ELSE 0 END), \
                    SUM(CASE WHEN severity = 'warning' THEN 1 ELSE 0 END), \
                    SUM(CASE WHEN severity = 'error' THEN 1 ELSE 0 END) \
                 FROM diagnostics AS diagnostic \
                 JOIN snapshot_diagnostic_sets AS current \
                   ON current.snapshot_id = diagnostic.snapshot_id \
                  AND current.build_id = diagnostic.build_id \
                 WHERE diagnostic.snapshot_id = ?1",
                [snapshot.as_str()],
                |row| {
                    Ok(DiagnosticSummary {
                        info: unsigned(row.get::<_, Option<i64>>(0)?.unwrap_or(0))?,
                        warning: unsigned(row.get::<_, Option<i64>>(1)?.unwrap_or(0))?,
                        error: unsigned(row.get::<_, Option<i64>>(2)?.unwrap_or(0))?,
                    })
                },
            )
            .map_err(sqlite_query_error)?;
        let mut statement = self
            .sidecar
            .connection()
            .prepare(
                "SELECT diagnostic.severity, diagnostic.code, diagnostic.path, \
                        diagnostic.span_start_byte, diagnostic.span_end_byte, \
                        diagnostic.span_start_line, diagnostic.span_start_column, \
                        diagnostic.span_end_line, diagnostic.span_end_column \
                 FROM diagnostics AS diagnostic \
                 JOIN snapshot_diagnostic_sets AS current \
                   ON current.snapshot_id = diagnostic.snapshot_id \
                  AND current.build_id = diagnostic.build_id \
                 WHERE diagnostic.snapshot_id = ?1 \
                 ORDER BY CASE diagnostic.severity \
                              WHEN 'error' THEN 0 WHEN 'warning' THEN 1 ELSE 2 END, \
                          diagnostic.code, COALESCE(diagnostic.path, ''), \
                          COALESCE(diagnostic.span_start_byte, -1), diagnostic.id \
                 LIMIT ?2",
            )
            .map_err(sqlite_query_error)?;
        let mut rows = statement
            .query(params![
                snapshot.as_str(),
                i64::from(max_diagnostics.saturating_add(1)),
            ])
            .map_err(sqlite_query_error)?;
        let mut items = Vec::new();
        while let Some(row) = rows.next().map_err(sqlite_query_error)? {
            let severity = diagnostic_severity(&value::<String>(row, 0)?)?;
            let code =
                DiagnosticCode::new(value::<String>(row, 1)?).map_err(|_| backend_error())?;
            let path = value::<Option<String>>(row, 2)?;
            let span = decode_span(row, 3, 4, 5, 6, 7, 8)?;
            let location = match (path, span) {
                (Some(path), span) => Some(DiagnosticLocation {
                    path: RepoPath::new(path).map_err(|_| backend_error())?,
                    span,
                }),
                (None, None) => None,
                (None, Some(_)) => return Err(backend_error()),
            };
            items.push(QueryDiagnostic {
                severity,
                code,
                location,
            });
        }
        let truncated = items.len() > max_diagnostics as usize;
        items.truncate(max_diagnostics as usize);
        Ok(DiagnosticsEnvelope {
            summary,
            items,
            truncated,
        })
    }

    fn diagnostics_with_deadline(
        &self,
        scope: &ResolvedScope,
        started: Instant,
    ) -> Result<(DiagnosticsEnvelope, bool), QueryError> {
        let duration = Duration::from_millis(scope.budget.max_duration_ms);
        if started.elapsed() >= duration {
            return Ok((truncated_diagnostics(), true));
        }
        let deadline = QueryDeadline::install(self.sidecar.connection(), started, duration)?;
        let diagnostics = self.diagnostics(&scope.snapshot.id, scope.budget.max_diagnostics);
        drop(deadline);
        match diagnostics {
            Ok(diagnostics) => Ok((diagnostics, started.elapsed() >= duration)),
            Err(error) if error.code == QueryErrorCode::BudgetExceeded => {
                Ok((truncated_diagnostics(), true))
            }
            Err(error) => Err(error),
        }
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
            .map_err(sqlite_query_error)
    }

    fn search_rows(
        &self,
        scope: &ResolvedScope,
        request: &SearchRequest,
        offset: u64,
        started: Instant,
    ) -> Result<SearchRows, QueryError> {
        let text = request.text.trim();
        let normalized = text.to_lowercase();
        let escaped = escape_like(&normalized);
        let prefix = format!("{escaped}%");
        let contains = format!("%{escaped}%");
        let kinds = serde_json::to_string(&request.node_kinds).map_err(|_| backend_error())?;
        let paths = request
            .paths
            .iter()
            .map(|path| SearchPathFilter {
                exact: path.as_str(),
                descendants: format!("{}/%", escape_like(path.as_str())),
            })
            .collect::<Vec<_>>();
        let paths = serde_json::to_string(&paths).map_err(|_| backend_error())?;
        let sql = format!(
            "SELECT {NODE_COLUMNS}, \
                CASE \
                    WHEN lower(COALESCE(semantic_key, '')) = ?2 THEN 0 \
                    WHEN lower(COALESCE(evidence_path, '')) = ?2 THEN 1 \
                    WHEN normalized_name = ?2 THEN 2 \
                    WHEN normalized_name LIKE ?3 ESCAPE '\\' THEN 3 \
                    WHEN normalized_name LIKE ?4 ESCAPE '\\' THEN 4 \
                    WHEN lower(COALESCE(semantic_key, '')) LIKE ?4 ESCAPE '\\' THEN 5 \
                    ELSE 6 \
                END AS match_rank \
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
                    WHERE evidence_path = json_extract(requested_path.value, '$.exact') \
                       OR evidence_path LIKE \
                          json_extract(requested_path.value, '$.descendants') ESCAPE '\\'\
               )) \
             ORDER BY match_rank, normalized_name, id \
             LIMIT ?7 OFFSET ?8"
        );
        let limit = i64::from(scope.budget.max_results.saturating_add(1));
        let offset = i64::try_from(offset).map_err(|_| stale_cursor_error())?;
        let connection = self.sidecar.connection();
        let _deadline = QueryDeadline::install(
            connection,
            started,
            Duration::from_millis(scope.budget.max_duration_ms),
        )?;
        let mut statement = match connection.prepare(&sql) {
            Ok(statement) => statement,
            Err(error) if sqlite_deadline_exceeded(&error) => {
                return Ok(SearchRows::deadline_exceeded());
            }
            Err(_) => return Err(backend_error()),
        };
        let mut rows = match statement.query(params![
            scope.snapshot.id.as_str(),
            normalized,
            prefix,
            contains,
            kinds,
            paths,
            limit,
            offset,
        ]) {
            Ok(rows) => rows,
            Err(error) if sqlite_deadline_exceeded(&error) => {
                return Ok(SearchRows::deadline_exceeded());
            }
            Err(_) => return Err(backend_error()),
        };
        let mut found = Vec::new();
        loop {
            match rows.next() {
                Ok(Some(row)) => found.push((
                    decode_node(row)?,
                    search_match_kind(value::<i64>(row, 19)?)?,
                )),
                Ok(None) => break,
                Err(error) if sqlite_deadline_exceeded(&error) => {
                    return Ok(SearchRows::deadline_exceeded());
                }
                Err(_) => return Err(backend_error()),
            }
        }
        Ok(SearchRows {
            rows: found,
            deadline_exceeded: false,
        })
    }

    fn show_rows(
        &self,
        scope: &ResolvedScope,
        request: &ShowRequest,
        offset: u64,
        started: Instant,
    ) -> Result<NodeRows, QueryError> {
        let (predicate, lookup) = match &request.lookup {
            ShowLookup::Node(id) => ("id = ?2", id.as_str()),
            ShowLookup::Symbol(key) => ("semantic_key = ?2", key.as_str()),
            ShowLookup::Path(path) => ("evidence_path = ?2", path.as_str()),
        };
        let sql = format!(
            "SELECT {NODE_COLUMNS} FROM nodes WHERE snapshot_id = ?1 AND {predicate} \
             ORDER BY kind, semantic_key, id LIMIT ?3 OFFSET ?4"
        );
        let connection = self.sidecar.connection();
        let _deadline = QueryDeadline::install(
            connection,
            started,
            Duration::from_millis(scope.budget.max_duration_ms),
        )?;
        let mut statement = match connection.prepare(&sql) {
            Ok(statement) => statement,
            Err(error) if sqlite_deadline_exceeded(&error) => {
                return Ok(NodeRows::deadline_exceeded());
            }
            Err(_) => return Err(backend_error()),
        };
        let mut rows = match statement.query(params![
            scope.snapshot.id.as_str(),
            lookup,
            i64::from(scope.budget.max_results.saturating_add(1)),
            i64::try_from(offset).map_err(|_| stale_cursor_error())?,
        ]) {
            Ok(rows) => rows,
            Err(error) if sqlite_deadline_exceeded(&error) => {
                return Ok(NodeRows::deadline_exceeded());
            }
            Err(_) => return Err(backend_error()),
        };
        let mut found = Vec::new();
        loop {
            match rows.next() {
                Ok(Some(row)) => found.push(decode_node(row)?),
                Ok(None) => break,
                Err(error) if sqlite_deadline_exceeded(&error) => {
                    return Ok(NodeRows::deadline_exceeded());
                }
                Err(_) => return Err(backend_error()),
            }
        }
        Ok(NodeRows {
            rows: found,
            deadline_exceeded: false,
        })
    }

    fn node(&self, snapshot: &SnapshotId, id: &NodeId) -> Result<Option<GraphNode>, QueryError> {
        let sql = format!("SELECT {NODE_COLUMNS} FROM nodes WHERE snapshot_id = ?1 AND id = ?2");
        let mut statement = self
            .sidecar
            .connection()
            .prepare(&sql)
            .map_err(sqlite_query_error)?;
        let mut rows = statement
            .query(params![snapshot.as_str(), id.as_str()])
            .map_err(sqlite_query_error)?;
        rows.next()
            .map_err(sqlite_query_error)?
            .map(decode_node)
            .transpose()
    }

    fn edges<'edge>(
        &self,
        snapshot: &SnapshotId,
        node: &NodeId,
        direction: EdgeDirection,
        kinds: &[String],
        excluded: impl IntoIterator<Item = &'edge EdgeId>,
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
               AND NOT EXISTS (\
                   SELECT 1 FROM json_each(?4) AS seen_edge \
                   WHERE seen_edge.value = edges.id\
               ) \
             ORDER BY id LIMIT ?5"
        );
        let kinds = serde_json::to_string(kinds).map_err(|_| backend_error())?;
        let excluded = serde_json::to_string(
            &excluded
                .into_iter()
                .map(|edge| edge.as_str())
                .collect::<Vec<_>>(),
        )
        .map_err(|_| backend_error())?;
        let mut statement = self
            .sidecar
            .connection()
            .prepare(&sql)
            .map_err(sqlite_query_error)?;
        let mut rows = statement
            .query(params![
                snapshot.as_str(),
                node.as_str(),
                kinds,
                excluded,
                i64::from(limit.saturating_add(1)),
            ])
            .map_err(sqlite_query_error)?;
        let mut found = Vec::new();
        while let Some(row) = rows.next().map_err(sqlite_query_error)? {
            found.push(decode_edge(row)?);
        }
        Ok(found)
    }

    fn context_seed_nodes(
        &self,
        snapshot: &SnapshotId,
        seed: &ContextSeed,
    ) -> Result<Vec<GraphNode>, QueryError> {
        if let ContextSeed::Node(id) = seed {
            return Ok(self.node(snapshot, id)?.into_iter().collect());
        }
        let (predicate, value) = match seed {
            ContextSeed::Symbol(key) => ("semantic_key = ?2", key.as_str()),
            ContextSeed::Path(path) => ("evidence_path = ?2", path.as_str()),
            ContextSeed::Node(_) => unreachable!("node seeds return above"),
        };
        let sql = format!(
            "SELECT {NODE_COLUMNS} FROM nodes WHERE snapshot_id = ?1 AND {predicate} \
             ORDER BY kind, COALESCE(semantic_key, ''), id LIMIT ?3"
        );
        let mut statement = self
            .sidecar
            .connection()
            .prepare(&sql)
            .map_err(sqlite_query_error)?;
        let mut rows = statement
            .query(params![
                snapshot.as_str(),
                value,
                i64::try_from(MAX_CONTEXT_CANDIDATES + 1).expect("context cap fits in i64"),
            ])
            .map_err(sqlite_query_error)?;
        let mut found = Vec::new();
        while let Some(row) = rows.next().map_err(sqlite_query_error)? {
            found.push(decode_node(row)?);
        }
        Ok(found)
    }

    fn assemble_context(
        &self,
        scope: &ResolvedScope,
        request: &ContextRequest,
        started: Instant,
    ) -> Result<ContextAssembly, QueryError> {
        let mut candidates = BTreeMap::<NodeId, ContextCandidate>::new();
        let mut frontier = Vec::new();
        let mut truncation = None;

        let mut seeds = request
            .seeds
            .iter()
            .map(|seed| Ok((context_seed_key(seed)?, seed)))
            .collect::<Result<Vec<_>, QueryError>>()?;
        seeds.sort_by(|left, right| left.0.cmp(&right.0));
        seeds.dedup_by(|left, right| left.0 == right.0);
        for (_, seed) in seeds {
            let mut nodes = match self.context_seed_nodes(&scope.snapshot.id, seed) {
                Ok(nodes) => nodes,
                Err(error) if error.code == QueryErrorCode::BudgetExceeded => {
                    set_context_truncation(&mut truncation, TruncationReason::Duration);
                    break;
                }
                Err(error) => return Err(error),
            };
            if nodes.is_empty() {
                return Err(invalid_request("context seed matched no nodes"));
            }
            if nodes.len() > MAX_CONTEXT_CANDIDATES {
                nodes.truncate(MAX_CONTEXT_CANDIDATES);
                set_context_truncation(&mut truncation, TruncationReason::Capability);
            }
            for node in nodes {
                let id = node.id.clone();
                if !candidates.contains_key(&id) && candidates.len() >= MAX_CONTEXT_CANDIDATES {
                    set_context_truncation(&mut truncation, TruncationReason::Capability);
                    break;
                }
                let is_new = insert_context_candidate(
                    &mut candidates,
                    node,
                    0,
                    ContextSelectionReason {
                        kind: ContextSelectionKind::ExactSeed,
                        via_node: None,
                        via_edge: None,
                    },
                );
                if is_new {
                    frontier.push(id);
                }
            }
        }

        frontier.sort();
        let mut frontier = frontier
            .into_iter()
            .map(|node| (node, 0_u32))
            .collect::<VecDeque<_>>();
        let mut seen_edges = BTreeSet::<EdgeId>::new();
        let mut explored_depth = 0_u32;

        'walk: while let Some((current, depth)) = frontier.pop_front() {
            explored_depth = explored_depth.max(depth);
            if started.elapsed().as_millis() >= u128::from(scope.budget.max_duration_ms) {
                set_context_truncation(&mut truncation, TruncationReason::Duration);
                break;
            }
            let remaining_edges = MAX_CONTEXT_CANDIDATES.saturating_sub(seen_edges.len());
            if remaining_edges == 0 {
                set_context_truncation(&mut truncation, TruncationReason::Capability);
                break;
            }
            let mut edges = match self.edges(
                &scope.snapshot.id,
                &current,
                request.policy.direction,
                &request.policy.edge_kinds,
                seen_edges.iter(),
                u32::try_from(remaining_edges).expect("context cap fits in u32"),
            ) {
                Ok(edges) => edges,
                Err(error) if error.code == QueryErrorCode::BudgetExceeded => {
                    set_context_truncation(&mut truncation, TruncationReason::Duration);
                    break;
                }
                Err(error) => return Err(error),
            };
            if edges.len() > remaining_edges {
                edges.truncate(remaining_edges);
                set_context_truncation(&mut truncation, TruncationReason::Capability);
            }
            if depth >= scope.budget.max_depth {
                if edges
                    .iter()
                    .any(|edge| context_edge_allowed(edge, request) && edge_targets_node(edge))
                {
                    set_context_truncation(&mut truncation, TruncationReason::Depth);
                }
                continue;
            }

            for edge in edges {
                seen_edges.insert(edge.id.clone());
                if !context_edge_allowed(&edge, request) {
                    continue;
                }
                let Some(next_id) = adjacent_node(&edge, &current, request.policy.direction) else {
                    continue;
                };
                let next = match self.node(&scope.snapshot.id, &next_id) {
                    Ok(Some(node)) => node,
                    Ok(None) => return Err(backend_error()),
                    Err(error) if error.code == QueryErrorCode::BudgetExceeded => {
                        set_context_truncation(&mut truncation, TruncationReason::Duration);
                        break 'walk;
                    }
                    Err(error) => return Err(error),
                };
                if !context_node_allowed(&next, request) {
                    continue;
                }
                let next_depth = depth.saturating_add(1);
                let reason = ContextSelectionReason {
                    kind: context_selection_kind(&edge, &current, &next),
                    via_node: Some(current.clone()),
                    via_edge: Some(edge.id.clone()),
                };
                if !candidates.contains_key(&next_id) && candidates.len() >= MAX_CONTEXT_CANDIDATES
                {
                    set_context_truncation(&mut truncation, TruncationReason::Capability);
                    break 'walk;
                }
                let is_new = insert_context_candidate(&mut candidates, next, next_depth, reason);
                if is_new {
                    frontier.push_back((next_id, next_depth));
                }
            }
        }

        let mut candidates = candidates
            .into_values()
            .filter(|candidate| candidate.node.provenance.evidence.is_some())
            .collect::<Vec<_>>();
        for candidate in &mut candidates {
            sort_context_reasons(&mut candidate.selection_reasons);
        }
        candidates.sort_by(context_candidate_order);
        Ok(ContextAssembly {
            candidates,
            explored_depth,
            truncation,
        })
    }
}

impl GraphQuery for SqliteGraphQuery<'_> {
    fn status(&self, request: &StatusRequest) -> Result<StatusResponse, QueryError> {
        self.status_at(request, Instant::now())
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
        let search_rows = self.search_rows(&scope, request, offset, started)?;
        let rows = search_rows.rows;
        let mut hits = Vec::new();
        let mut returned_bytes = 0_u64;
        let mut reason = search_rows
            .deadline_exceeded
            .then_some(TruncationReason::Duration);
        for (node, match_kind) in rows {
            if hits.len() >= scope.budget.max_results as usize {
                reason = Some(TruncationReason::Results);
                break;
            }
            if started.elapsed().as_millis() >= u128::from(scope.budget.max_duration_ms) {
                reason = Some(TruncationReason::Duration);
                break;
            }
            let hit = search_hit(node, match_kind, request.text.trim());
            let bytes = serialized_len(&hit)?;
            if returned_bytes.saturating_add(bytes) > scope.budget.max_bytes {
                reason = Some(TruncationReason::Bytes);
                break;
            }
            returned_bytes = returned_bytes.saturating_add(bytes);
            hits.push(hit);
        }
        let (diagnostics, diagnostics_timed_out) =
            self.diagnostics_with_deadline(&scope, started)?;
        if diagnostics_timed_out {
            reason = Some(TruncationReason::Duration);
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
            source_revision: source_revision(&scope.snapshot, &scope.source_revision_id),
            freshness: scope.freshness,
            diagnostics,
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
        let show_rows = self.show_rows(&scope, request, offset, started)?;
        if show_rows.rows.is_empty() && !show_rows.deadline_exceeded && offset == 0 {
            return Err(invalid_request("graph lookup matched no nodes"));
        }
        let mut nodes = Vec::new();
        let mut returned_bytes = 0_u64;
        let mut reason = show_rows
            .deadline_exceeded
            .then_some(TruncationReason::Duration);
        for node in show_rows.rows {
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
        let (diagnostics, diagnostics_timed_out) =
            self.diagnostics_with_deadline(&scope, started)?;
        if diagnostics_timed_out {
            reason = Some(TruncationReason::Duration);
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
            source_revision: source_revision(&scope.snapshot, &scope.source_revision_id),
            freshness: scope.freshness,
            diagnostics,
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
        let deadline = QueryDeadline::install(
            self.sidecar.connection(),
            started,
            Duration::from_millis(scope.budget.max_duration_ms),
        )?;

        let mut nodes = BTreeMap::new();
        let mut edges = BTreeMap::new();
        let mut frontier = VecDeque::new();
        let mut returned_bytes = 0_u64;
        let mut reason = None;
        for root in &request.roots {
            let node = match self.node(&scope.snapshot.id, root) {
                Ok(Some(node)) => node,
                Ok(None) => {
                    return Err(invalid_request("neighborhood root was not found"));
                }
                Err(error) if error.code == QueryErrorCode::BudgetExceeded => {
                    reason = Some(TruncationReason::Duration);
                    break;
                }
                Err(error) => return Err(error),
            };
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
                match self.edges(
                    &scope.snapshot.id,
                    &current,
                    request.direction,
                    &request.edge_kinds,
                    std::iter::empty::<&EdgeId>(),
                    1,
                ) {
                    Ok(edges) if !edges.is_empty() => {
                        reason = Some(TruncationReason::Depth);
                    }
                    Ok(_) => {}
                    Err(error) if error.code == QueryErrorCode::BudgetExceeded => {
                        reason = Some(TruncationReason::Duration);
                        break;
                    }
                    Err(error) => return Err(error),
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
            let next_edges = match self.edges(
                &scope.snapshot.id,
                &current,
                request.direction,
                &request.edge_kinds,
                edges.keys(),
                remaining,
            ) {
                Ok(edges) => edges,
                Err(error) if error.code == QueryErrorCode::BudgetExceeded => {
                    reason = Some(TruncationReason::Duration);
                    break;
                }
                Err(error) => return Err(error),
            };
            for edge in next_edges {
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
                let next = match self.node(&scope.snapshot.id, &next_id) {
                    Ok(Some(node)) => node,
                    Ok(None) => return Err(backend_error()),
                    Err(error) if error.code == QueryErrorCode::BudgetExceeded => {
                        reason = Some(TruncationReason::Duration);
                        break;
                    }
                    Err(error) => return Err(error),
                };
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

        drop(deadline);
        let (diagnostics, diagnostics_timed_out) =
            self.diagnostics_with_deadline(&scope, started)?;
        if diagnostics_timed_out {
            reason = Some(TruncationReason::Duration);
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
            source_revision: source_revision(&scope.snapshot, &scope.source_revision_id),
            freshness: scope.freshness,
            diagnostics,
            page,
            data: NeighborhoodData {
                nodes: nodes.into_values().collect(),
                edges: edges.into_values().collect(),
            },
        })
    }

    fn context(&self, request: &ContextRequest) -> Result<ContextResponse, QueryError> {
        let started = Instant::now();
        let scope = self.resolve_scope(&request.scope)?;
        if request.seeds.is_empty() || request.seeds.len() > MAX_FILTERS {
            return Err(invalid_request("context requires 1..=32 seeds"));
        }
        validate_filters(&request.policy.edge_kinds, 0)?;
        let fingerprint = context_cursor_fingerprint(request)?;
        let offset = decode_cursor(
            request.page.cursor.as_ref(),
            "context",
            &scope.snapshot.id,
            &fingerprint,
        )?;
        let deadline = QueryDeadline::install(
            self.sidecar.connection(),
            started,
            Duration::from_millis(scope.budget.max_duration_ms),
        )?;
        let assembly = self.assemble_context(&scope, request, started)?;
        drop(deadline);

        let offset = usize::try_from(offset).map_err(|_| stale_cursor_error())?;
        if offset > assembly.candidates.len() {
            return Err(stale_cursor_error());
        }
        let mut candidates = assembly.candidates.into_iter().skip(offset).peekable();
        let mut items = Vec::new();
        let mut returned_bytes = 0_u64;
        let mut reason = None;
        for candidate in candidates.by_ref() {
            if items.len() >= scope.budget.max_results as usize {
                reason = Some(TruncationReason::Results);
                break;
            }
            if started.elapsed().as_millis() >= u128::from(scope.budget.max_duration_ms) {
                reason = Some(TruncationReason::Duration);
                break;
            }
            let item = context_item(candidate);
            let bytes = serialized_len(&item)?;
            if returned_bytes.saturating_add(bytes) > scope.budget.max_bytes {
                reason = Some(TruncationReason::Bytes);
                break;
            }
            returned_bytes = returned_bytes.saturating_add(bytes);
            items.push(item);
        }
        let has_more_candidates = candidates.peek().is_some()
            || matches!(
                reason,
                Some(TruncationReason::Results | TruncationReason::Bytes)
            );
        if reason.is_none() {
            reason = assembly.truncation;
        }
        let (diagnostics, diagnostics_timed_out) =
            self.diagnostics_with_deadline(&scope, started)?;
        if diagnostics_timed_out {
            reason = Some(TruncationReason::Duration);
        }
        let cursor = (has_more_candidates
            && matches!(
                reason,
                Some(TruncationReason::Results | TruncationReason::Bytes)
            ))
        .then_some((
            "context",
            &scope.snapshot.id,
            fingerprint.as_str(),
            u64::try_from(offset + items.len()).unwrap_or(u64::MAX),
        ));
        let page = page_info(
            reason,
            items.len(),
            returned_bytes,
            assembly.explored_depth,
            cursor,
        )?;
        Ok(QueryResponse {
            wire_version: QUERY_WIRE_VERSION,
            repository: scope.repository,
            snapshot_id: scope.snapshot.id.clone(),
            source_revision: source_revision(&scope.snapshot, &scope.source_revision_id),
            freshness: scope.freshness,
            diagnostics,
            page,
            data: ContextData { items },
        })
    }
}

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

#[derive(Serialize)]
struct ContextCursorParameters<'a> {
    seeds: Vec<String>,
    direction: EdgeDirection,
    edge_kinds: Vec<&'a str>,
    include_unresolved: bool,
    include_external: bool,
}

fn context_cursor_fingerprint(request: &ContextRequest) -> Result<String, QueryError> {
    let mut seeds = request
        .seeds
        .iter()
        .map(context_seed_key)
        .collect::<Result<Vec<_>, _>>()?;
    seeds.sort_unstable();
    seeds.dedup();
    let mut edge_kinds = request
        .policy
        .edge_kinds
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    edge_kinds.sort_unstable();
    edge_kinds.dedup();
    cursor_fingerprint(
        "context",
        &ContextCursorParameters {
            seeds,
            direction: request.policy.direction,
            edge_kinds,
            include_unresolved: request.policy.include_unresolved,
            include_external: request.policy.include_external,
        },
    )
}

fn context_seed_key(seed: &ContextSeed) -> Result<String, QueryError> {
    serde_json::to_string(seed).map_err(|_| backend_error())
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

fn diagnostic_severity(value: &str) -> Result<DiagnosticSeverity, QueryError> {
    match value {
        "info" => Ok(DiagnosticSeverity::Info),
        "warning" => Ok(DiagnosticSeverity::Warning),
        "error" => Ok(DiagnosticSeverity::Error),
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
        recommended_action: None,
        details: BTreeMap::new(),
    }
}

fn backend_error() -> QueryError {
    QueryError {
        wire_version: QUERY_WIRE_VERSION,
        code: QueryErrorCode::BackendUnavailable,
        message: "repository graph storage is unavailable or inconsistent".to_string(),
        retryable: true,
        recommended_action: None,
        details: BTreeMap::new(),
    }
}

fn duration_budget_exceeded_error() -> QueryError {
    QueryError {
        wire_version: QUERY_WIRE_VERSION,
        code: QueryErrorCode::BudgetExceeded,
        message: "repository graph query exceeded the duration budget".to_string(),
        retryable: false,
        recommended_action: None,
        details: BTreeMap::new(),
    }
}

fn not_built_error() -> QueryError {
    QueryError {
        wire_version: QUERY_WIRE_VERSION,
        code: QueryErrorCode::NotBuilt,
        message: "repository graph is not built; run `ferrus graph index`".to_string(),
        retryable: false,
        recommended_action: Some(RetrievalAction::Index),
        details: BTreeMap::new(),
    }
}

fn index_building_error() -> QueryError {
    QueryError {
        wire_version: QUERY_WIRE_VERSION,
        code: QueryErrorCode::IndexBuilding,
        message: "repository graph is currently building; retry after indexing completes"
            .to_string(),
        retryable: true,
        recommended_action: Some(RetrievalAction::WaitForBuild),
        details: BTreeMap::new(),
    }
}

fn index_failed_error() -> QueryError {
    QueryError {
        wire_version: QUERY_WIRE_VERSION,
        code: QueryErrorCode::IndexFailed,
        message: "repository graph build failed; run `ferrus graph index` to retry".to_string(),
        retryable: false,
        recommended_action: Some(RetrievalAction::RetryIndex),
        details: BTreeMap::new(),
    }
}

fn snapshot_not_found_error() -> QueryError {
    QueryError {
        wire_version: QUERY_WIRE_VERSION,
        code: QueryErrorCode::SnapshotNotFound,
        message: "repository graph snapshot was not found".to_string(),
        retryable: false,
        recommended_action: None,
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
        recommended_action: None,
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
        NonZeroU32::new(limits.max_diagnostics)
            .ok_or_else(|| invalid_request("query max_diagnostics must be greater than zero"))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository_graph::{
        config::RepositoryGraphConfig,
        domain::{
            Availability, BuildId, BuildState, DiagnosticCode, DiagnosticSeverity, GraphBuild,
            GraphDiagnostic, PublishedViewName, RepositoryId, RepositoryNamespace, SnapshotId,
            SourceRevisionId,
        },
        index::{IndexCoordinator, IndexRequest, active_extractor_identities},
        source::{FilesystemRepositorySource, SourceDiscoveryContext},
        sqlite::{OpenSidecarResult, open_for_build_at},
        store::BuildFailure,
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
        FreshnessComparison,
    ) {
        indexed_fixture_with_extra_files(&[])
    }

    fn indexed_fixture_with_extra_files(
        extra_files: &[(&str, &str)],
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        Sidecar,
        RepositoryGraphConfig,
        FreshnessComparison,
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
        for (path, contents) in extra_files {
            let path = source_dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        let config = RepositoryGraphConfig::default();
        let identities = active_extractor_identities(&config).unwrap();
        let context =
            SourceDiscoveryContext::from_config(repository(), &config, &identities).unwrap();
        let source = FilesystemRepositorySource::discover(source_dir.path(), context).unwrap();
        let freshness_comparison = FreshnessComparison::from_manifest(source.manifest());
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
        (
            source_dir,
            sidecar_dir,
            sidecar,
            config,
            freshness_comparison,
        )
    }

    fn scope(config: &RepositoryGraphConfig) -> super::super::query::QueryScope {
        super::super::query::QueryScope::current(
            repository(),
            SnapshotSelector::Published(PublishedViewName::new("canonical").unwrap()),
            default_budget(&config.query_limits).unwrap(),
        )
    }

    fn context_request(config: &RepositoryGraphConfig, seed: ContextSeed) -> ContextRequest {
        ContextRequest {
            scope: scope(config),
            seeds: vec![seed],
            policy: super::super::query::ContextPolicy {
                direction: EdgeDirection::Both,
                edge_kinds: vec![],
                include_unresolved: false,
                include_external: false,
            },
            page: super::super::query::PageRequest { cursor: None },
        }
    }

    fn fixture_symbol(query: &SqliteGraphQuery<'_>, config: &RepositoryGraphConfig) -> SemanticKey {
        query
            .search(&SearchRequest {
                scope: scope(config),
                text: "RuntimeTaskContext".to_string(),
                node_kinds: vec!["struct".to_string()],
                paths: vec![],
                page: super::super::query::PageRequest { cursor: None },
            })
            .unwrap()
            .data
            .hits
            .into_iter()
            .find_map(|hit| hit.semantic_key)
            .unwrap()
    }

    #[test]
    fn context_resolves_seeds_ranks_deduplicates_and_preserves_evidence() {
        let (_source, _sidecar_dir, sidecar, config, comparison) = indexed_fixture();
        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), Some(comparison));
        let symbol = ContextSeed::Symbol(fixture_symbol(&query, &config));
        let request = context_request(&config, symbol.clone());

        let first = query.context(&request).unwrap();
        let second = query.context(&request).unwrap();

        assert!(!first.data.items.is_empty());
        assert_eq!(
            first.data.items[0].selection_reasons[0].kind,
            ContextSelectionKind::ExactSeed
        );
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            serde_json::to_value(&second).unwrap()
        );
        let mut node_ids = BTreeSet::new();
        let mut selection_kinds = BTreeSet::new();
        for item in &first.data.items {
            assert!(node_ids.insert(item.node_id.clone()));
            let evidence = item.provenance.evidence.as_ref().unwrap();
            assert_eq!(item.path, evidence.path);
            assert_eq!(item.span, evidence.span);
            assert_eq!(item.content_identity, evidence.content_identity);
            let mut reasons = item.selection_reasons.clone();
            sort_context_reasons(&mut reasons);
            reasons.dedup();
            assert_eq!(item.selection_reasons, reasons);
            selection_kinds.extend(
                item.selection_reasons
                    .iter()
                    .map(|reason| context_selection_rank(reason.kind)),
            );
        }
        assert!(
            selection_kinds.contains(&context_selection_rank(ContextSelectionKind::Containment))
        );
        assert!(
            selection_kinds.contains(&context_selection_rank(ContextSelectionKind::Declaration))
        );

        let mut ordered = context_request(&config, symbol.clone());
        ordered
            .seeds
            .push(ContextSeed::Path(RepoPath::new("src/lib.rs").unwrap()));
        let mut reversed = ordered.clone();
        reversed.seeds.reverse();
        assert_eq!(
            serde_json::to_value(query.context(&ordered).unwrap()).unwrap(),
            serde_json::to_value(query.context(&reversed).unwrap()).unwrap()
        );
    }

    #[test]
    fn context_pagination_is_snapshot_and_parameter_bound() {
        let (_source, _sidecar_dir, sidecar, mut config, _comparison) = indexed_fixture();
        config.query_limits.max_results = 2;
        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
        let mut request = context_request(
            &config,
            ContextSeed::Symbol(fixture_symbol(&query, &config)),
        );

        let first = query.context(&request).unwrap();
        assert_eq!(first.data.items.len(), 2);
        assert_eq!(
            first.page.truncation.as_ref().unwrap().reason,
            TruncationReason::Results
        );
        request.page.cursor = first.page.next_cursor.clone();
        let second = query.context(&request).unwrap();
        assert!(first.data.items.iter().all(|left| {
            second
                .data
                .items
                .iter()
                .all(|right| left.node_id != right.node_id)
        }));

        request.policy.direction = EdgeDirection::Outgoing;
        let error = query.context(&request).unwrap_err();
        assert_eq!(error.code, QueryErrorCode::StaleCursor);
    }

    #[test]
    fn context_depth_and_byte_budgets_return_terminal_truncation() {
        let (_source, _sidecar_dir, sidecar, mut config, _comparison) = indexed_fixture();
        config.query_limits.max_depth = 1;
        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
        let seed = ContextSeed::Symbol(fixture_symbol(&query, &config));

        let depth = query
            .context(&context_request(&config, seed.clone()))
            .unwrap();
        assert_eq!(
            depth.page.truncation.as_ref().unwrap().reason,
            TruncationReason::Depth
        );
        assert!(depth.page.next_cursor.is_none());

        config.query_limits.max_bytes = 1;
        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
        let bytes = query.context(&context_request(&config, seed)).unwrap();
        assert!(bytes.data.items.is_empty());
        assert_eq!(
            bytes.page.truncation.as_ref().unwrap().reason,
            TruncationReason::Bytes
        );
        assert!(bytes.page.next_cursor.is_none());
    }

    #[test]
    fn context_rejects_empty_and_missing_seeds() {
        let (_source, _sidecar_dir, sidecar, config, _comparison) = indexed_fixture();
        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
        let mut request = context_request(
            &config,
            ContextSeed::Node(NodeId::new("node:missing").unwrap()),
        );
        assert_eq!(
            query.context(&request).unwrap_err().code,
            QueryErrorCode::InvalidRequest
        );
        request.seeds.clear();
        assert_eq!(
            query.context(&request).unwrap_err().code,
            QueryErrorCode::InvalidRequest
        );
    }

    #[test]
    fn context_policy_controls_unresolved_and_external_candidates() {
        for (resolution, include_unresolved, include_external) in
            [("unresolved", true, false), ("external", false, true)]
        {
            let (_source, _sidecar_dir, sidecar, config, _comparison) = indexed_fixture();
            let (file, classified, edge): (String, String, String) = sidecar
                .connection()
                .query_row(
                    "SELECT edge.source_node_id, edge.target_node_id, edge.id \
                     FROM edges AS edge \
                     JOIN nodes AS source ON source.snapshot_id = edge.snapshot_id \
                                         AND source.id = edge.source_node_id \
                     JOIN nodes AS target ON target.snapshot_id = edge.snapshot_id \
                                         AND target.id = edge.target_node_id \
                     WHERE edge.kind = 'classified_as' \
                       AND source.kind = 'file' \
                       AND source.evidence_path = 'Cargo.toml' \
                       AND target.kind = 'configuration' \
                     LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            sidecar
                .connection()
                .execute(
                    "UPDATE nodes SET resolution_state = ?1 WHERE id = ?2",
                    params![resolution, classified],
                )
                .unwrap();
            sidecar
                .connection()
                .execute(
                    "UPDATE edges SET resolution_state = ?1 WHERE id = ?2",
                    params![resolution, edge],
                )
                .unwrap();
            let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
            let mut request =
                context_request(&config, ContextSeed::Node(NodeId::new(file).unwrap()));
            request.policy.direction = EdgeDirection::Outgoing;
            request.policy.edge_kinds = vec!["classified_as".to_string()];

            let excluded = query.context(&request).unwrap();
            assert!(
                excluded
                    .data
                    .items
                    .iter()
                    .all(|item| item.node_id.as_str() != classified)
            );

            request.policy.include_unresolved = include_unresolved;
            request.policy.include_external = include_external;
            let included = query.context(&request).unwrap();
            let classified_item = included
                .data
                .items
                .iter()
                .find(|item| item.node_id.as_str() == classified)
                .unwrap();
            assert!(
                classified_item
                    .selection_reasons
                    .iter()
                    .any(|reason| reason.kind == ContextSelectionKind::Configuration)
            );
        }
    }

    #[test]
    fn context_classifies_documentation_facts_ahead_of_generic_relationships() {
        let (_source, _sidecar_dir, sidecar, config, _comparison) =
            indexed_fixture_with_extra_files(&[("README.md", "# Fixture\n")]);
        let (file, document): (String, String) = sidecar
            .connection()
            .query_row(
                "SELECT edge.source_node_id, edge.target_node_id \
                 FROM edges AS edge \
                 JOIN nodes AS source ON source.snapshot_id = edge.snapshot_id \
                                     AND source.id = edge.source_node_id \
                 JOIN nodes AS target ON target.snapshot_id = edge.snapshot_id \
                                     AND target.id = edge.target_node_id \
                 WHERE edge.kind = 'classified_as' \
                   AND source.evidence_path = 'README.md' \
                   AND target.kind = 'document' \
                 LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
        let mut request = context_request(&config, ContextSeed::Node(NodeId::new(file).unwrap()));
        request.policy.direction = EdgeDirection::Outgoing;
        request.policy.edge_kinds = vec!["classified_as".to_string()];

        let response = query.context(&request).unwrap();
        let item = response
            .data
            .items
            .iter()
            .find(|item| item.node_id.as_str() == document)
            .unwrap();

        assert!(
            item.selection_reasons
                .iter()
                .any(|reason| reason.kind == ContextSelectionKind::Documentation)
        );
    }

    #[test]
    fn context_labels_resolved_import_targets_as_dependencies() {
        let (_source, _sidecar_dir, sidecar, config, _comparison) =
            indexed_fixture_with_extra_files(&[
                (
                    "src/lib.rs",
                    "pub mod api;\nuse crate::api::Api;\npub fn make() -> Api { Api }\n",
                ),
                ("src/api.rs", "pub struct Api;\n"),
            ]);
        let (source, target): (String, String) = sidecar
            .connection()
            .query_row(
                "SELECT source_node_id, target_node_id FROM edges \
                 WHERE kind = 'imports' AND target_node_id IS NOT NULL \
                 ORDER BY id LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
        let mut request = context_request(&config, ContextSeed::Node(NodeId::new(source).unwrap()));
        request.policy.direction = EdgeDirection::Outgoing;
        request.policy.edge_kinds = vec!["imports".to_string()];

        let response = query.context(&request).unwrap();
        let target = response
            .data
            .items
            .iter()
            .find(|item| item.node_id.as_str() == target)
            .unwrap();

        assert!(
            target
                .selection_reasons
                .iter()
                .any(|reason| reason.kind == ContextSelectionKind::ResolvedDependency)
        );
        assert_eq!(target.provenance.resolution, ResolutionState::Resolved);
    }

    #[test]
    fn context_assembly_observes_an_expired_deadline() {
        let (_source, _sidecar_dir, sidecar, config, _comparison) = indexed_fixture();
        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
        let request = context_request(
            &config,
            ContextSeed::Symbol(fixture_symbol(&query, &config)),
        );
        let resolved = query.resolve_scope(&request.scope).unwrap();
        let started = Instant::now()
            - Duration::from_millis(config.query_limits.max_duration_ms.saturating_add(1));
        let deadline = QueryDeadline::install(
            sidecar.connection(),
            started,
            Duration::from_millis(config.query_limits.max_duration_ms),
        )
        .unwrap();

        let assembly = query
            .assemble_context(&resolved, &request, started)
            .unwrap();
        drop(deadline);

        assert_eq!(assembly.truncation, Some(TruncationReason::Duration));
    }

    #[test]
    fn context_returns_bounded_snapshot_diagnostics() {
        let (_source, _sidecar_dir, mut sidecar, mut config, _comparison) = indexed_fixture();
        let snapshot = sidecar
            .published_snapshot(&repository(), &PublishedViewName::new("canonical").unwrap())
            .unwrap()
            .unwrap();
        for code in ["context.a", "context.b"] {
            sidecar
                .record_diagnostic(&GraphDiagnostic {
                    build_id: BuildId::new("build-query").unwrap(),
                    snapshot_id: Some(snapshot.id.clone()),
                    severity: DiagnosticSeverity::Warning,
                    code: DiagnosticCode::new(code).unwrap(),
                    location: None,
                    metrics: BTreeMap::new(),
                })
                .unwrap();
        }
        config.query_limits.max_diagnostics = 1;
        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
        let request = context_request(
            &config,
            ContextSeed::Symbol(fixture_symbol(&query, &config)),
        );

        let response = query.context(&request).unwrap();

        assert!(response.diagnostics.summary.warning >= 2);
        assert_eq!(response.diagnostics.items.len(), 1);
        assert!(response.diagnostics.truncated);
    }

    #[test]
    fn search_show_and_neighborhood_return_evidence_and_provenance() {
        let (_source, _sidecar_dir, sidecar, config, freshness_comparison) = indexed_fixture();
        let query = SqliteGraphQuery::new(
            &sidecar,
            config.query_limits.clone(),
            Some(freshness_comparison.clone()),
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
        assert_eq!(
            search.source_revision.manifest_digest,
            freshness_comparison.source_manifest_digest
        );
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
    fn search_classifies_exact_matches_and_is_snapshot_deterministic() {
        let (_source, _sidecar_dir, sidecar, config, comparison) = indexed_fixture();
        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), Some(comparison));

        for (text, expected) in [
            ("RuntimeTaskContext", SearchMatchKind::ExactNormalizedName),
            ("src/lib.rs", SearchMatchKind::ExactPath),
        ] {
            let request = SearchRequest {
                scope: scope(&config),
                text: text.to_string(),
                node_kinds: vec![],
                paths: vec![],
                page: super::super::query::PageRequest { cursor: None },
            };
            let first = query.search(&request).unwrap();
            let second = query.search(&request).unwrap();

            assert_eq!(first.data.hits.first().unwrap().match_kind, expected);
            assert_eq!(
                serde_json::to_value(first).unwrap(),
                serde_json::to_value(second).unwrap()
            );
        }

        let symbol = query
            .search(&SearchRequest {
                scope: scope(&config),
                text: "RuntimeTaskContext".to_string(),
                node_kinds: vec!["struct".to_string()],
                paths: vec![],
                page: super::super::query::PageRequest { cursor: None },
            })
            .unwrap()
            .data
            .hits
            .into_iter()
            .find_map(|hit| hit.semantic_key)
            .unwrap();
        let exact_symbol = query
            .search(&SearchRequest {
                scope: scope(&config),
                text: symbol.as_str().to_string(),
                node_kinds: vec![],
                paths: vec![],
                page: super::super::query::PageRequest { cursor: None },
            })
            .unwrap();
        assert_eq!(
            exact_symbol.data.hits.first().unwrap().match_kind,
            SearchMatchKind::ExactSemanticKey
        );
    }

    #[test]
    fn previous_wire_versions_are_rejected_explicitly() {
        let (_source, _sidecar_dir, sidecar, config, _comparison) = indexed_fixture();
        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
        let mut request_scope = scope(&config);
        request_scope.wire_version = QUERY_WIRE_VERSION - 1;

        let error = query
            .status(&StatusRequest {
                scope: request_scope,
            })
            .unwrap_err();

        assert_eq!(error.code, QueryErrorCode::UnsupportedWireVersion);
        assert_eq!(error.wire_version, QUERY_WIRE_VERSION);
    }

    #[test]
    fn unbuilt_status_and_queries_distinguish_building_and_failed_attempts() {
        let sidecar_dir = tempfile::tempdir().unwrap();
        let OpenSidecarResult::Ready(mut sidecar) =
            open_for_build_at(&sidecar_dir.path().join("repo-graph.db")).unwrap()
        else {
            panic!("new sidecar unexpectedly requires rebuild");
        };
        let config = RepositoryGraphConfig::default();
        let build = GraphBuild {
            id: BuildId::new("build-in-progress").unwrap(),
            repository: repository(),
            source_revision_id: SourceRevisionId::new("revision-next").unwrap(),
            prospective_snapshot_id: SnapshotId::new("snapshot-next").unwrap(),
            state: BuildState::Building,
        };
        sidecar.start_build(&build).unwrap();

        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
        let status = query
            .status(&StatusRequest {
                scope: scope(&config),
            })
            .unwrap();
        assert_eq!(status.data.availability, Availability::NotBuilt);
        assert_eq!(status.data.build_state, Some(BuildState::Building));
        assert_eq!(
            status.data.recommended_action,
            Some(RetrievalAction::WaitForBuild)
        );
        let error = query
            .search(&SearchRequest {
                scope: scope(&config),
                text: "anything".to_string(),
                node_kinds: vec![],
                paths: vec![],
                page: super::super::query::PageRequest { cursor: None },
            })
            .unwrap_err();
        assert_eq!(error.code, QueryErrorCode::IndexBuilding);

        drop(query);
        sidecar
            .fail_build(&BuildFailure {
                build_id: build.id,
                code: DiagnosticCode::new("index.failed").unwrap(),
            })
            .unwrap();
        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
        let status = query
            .status(&StatusRequest {
                scope: scope(&config),
            })
            .unwrap();
        assert_eq!(status.data.build_state, Some(BuildState::Failed));
        assert_eq!(
            status.data.recommended_action,
            Some(RetrievalAction::RetryIndex)
        );
        let error = query
            .search(&SearchRequest {
                scope: scope(&config),
                text: "anything".to_string(),
                node_kinds: vec![],
                paths: vec![],
                page: super::super::query::PageRequest { cursor: None },
            })
            .unwrap_err();
        assert_eq!(error.code, QueryErrorCode::IndexFailed);
    }

    #[test]
    fn diagnostics_are_deterministic_and_capped_independently() {
        let (_source, _sidecar_dir, mut sidecar, mut config, _comparison) = indexed_fixture();
        let snapshot = sidecar
            .published_snapshot(&repository(), &PublishedViewName::new("canonical").unwrap())
            .unwrap()
            .unwrap();
        for code in ["query.z", "query.a", "query.m"] {
            sidecar
                .record_diagnostic(&GraphDiagnostic {
                    build_id: BuildId::new("build-query").unwrap(),
                    snapshot_id: Some(snapshot.id.clone()),
                    severity: DiagnosticSeverity::Warning,
                    code: DiagnosticCode::new(code).unwrap(),
                    location: None,
                    metrics: BTreeMap::new(),
                })
                .unwrap();
        }
        config.query_limits.max_diagnostics = 2;
        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
        let response = query
            .search(&SearchRequest {
                scope: scope(&config),
                text: "RuntimeTaskContext".to_string(),
                node_kinds: vec![],
                paths: vec![],
                page: super::super::query::PageRequest { cursor: None },
            })
            .unwrap();

        assert!(response.diagnostics.truncated);
        assert_eq!(response.diagnostics.items.len(), 2);
        assert!(response.diagnostics.summary.warning >= 3);
        assert!(
            response
                .diagnostics
                .items
                .windows(2)
                .all(|items| { items[0].code.as_str() <= items[1].code.as_str() })
        );
    }

    #[test]
    fn path_filters_treat_like_metacharacters_as_literals() {
        let (_source, _sidecar_dir, sidecar, config, _comparison) =
            indexed_fixture_with_extra_files(&[
                ("src_a/lib.rs", "pub struct PathScopedMarker;\n"),
                ("srcXa/lib.rs", "pub struct PathScopedMarker;\n"),
                ("src%a/lib.rs", "pub struct PathScopedMarker;\n"),
            ]);
        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);

        for prefix in ["src_a", "src%a"] {
            let response = query
                .search(&SearchRequest {
                    scope: scope(&config),
                    text: "PathScopedMarker".to_string(),
                    node_kinds: vec!["struct".to_string()],
                    paths: vec![RepoPath::new(prefix).unwrap()],
                    page: super::super::query::PageRequest { cursor: None },
                })
                .unwrap();
            let expected_path = format!("{prefix}/lib.rs");

            assert!(!response.data.hits.is_empty());
            assert!(response.data.hits.iter().all(|hit| {
                hit.path
                    .as_ref()
                    .is_some_and(|path| path.as_str() == expected_path)
            }));
        }
    }

    #[test]
    fn service_limits_cap_results_and_cursors_are_query_bound() {
        let (_source, _sidecar_dir, sidecar, mut config, _comparison) = indexed_fixture();
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
    fn oversized_first_search_hit_returns_terminal_byte_truncation() {
        let (_source, _sidecar_dir, sidecar, mut config, _comparison) = indexed_fixture();
        config.query_limits.max_bytes = 1;
        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);

        let response = query
            .search(&SearchRequest {
                scope: scope(&config),
                text: "RuntimeTaskContext".to_string(),
                node_kinds: vec![],
                paths: vec![],
                page: super::super::query::PageRequest { cursor: None },
            })
            .unwrap();

        assert!(response.data.hits.is_empty());
        assert_eq!(
            response.page.truncation.as_ref().unwrap().reason,
            TruncationReason::Bytes
        );
        assert!(response.page.next_cursor.is_none());
    }

    #[test]
    fn edge_limit_is_applied_after_seen_edges_are_excluded() {
        let (_source, _sidecar_dir, sidecar, config, _comparison) = indexed_fixture();
        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
        let (snapshot, node): (String, String) = sidecar
            .connection()
            .query_row(
                "SELECT edges.snapshot_id, nodes.id \
                 FROM nodes \
                 JOIN edges ON edges.snapshot_id = nodes.snapshot_id \
                   AND (edges.source_node_id = nodes.id OR edges.target_node_id = nodes.id) \
                 GROUP BY edges.snapshot_id, nodes.id \
                 HAVING COUNT(*) >= 2 \
                 ORDER BY nodes.id \
                 LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let snapshot = SnapshotId::new(snapshot).unwrap();
        let node = NodeId::new(node).unwrap();
        let all = query
            .edges(
                &snapshot,
                &node,
                EdgeDirection::Both,
                &[],
                std::iter::empty::<&EdgeId>(),
                16,
            )
            .unwrap();
        assert!(all.len() >= 2);
        let seen = [all[0].id.clone()];

        let unseen = query
            .edges(&snapshot, &node, EdgeDirection::Both, &[], seen.iter(), 1)
            .unwrap();

        assert_eq!(unseen.first().map(|edge| &edge.id), Some(&all[1].id));
        assert!(unseen.iter().all(|edge| edge.id != seen[0]));
    }

    #[test]
    fn sqlite_search_execution_observes_an_expired_deadline() {
        let (_source, _sidecar_dir, sidecar, config, _comparison) = indexed_fixture();
        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
        let requested_scope = scope(&config);
        let resolved_scope = query.resolve_scope(&requested_scope).unwrap();
        let request = SearchRequest {
            scope: requested_scope,
            text: "missing-low-selectivity-term".to_string(),
            node_kinds: vec![],
            paths: vec![],
            page: super::super::query::PageRequest { cursor: None },
        };
        let started = Instant::now()
            - Duration::from_millis(config.query_limits.max_duration_ms.saturating_add(1));

        let rows = query
            .search_rows(&resolved_scope, &request, 0, started)
            .unwrap();

        assert!(rows.deadline_exceeded);
        assert!(rows.rows.is_empty());
    }

    #[test]
    fn sqlite_show_execution_observes_an_expired_deadline() {
        let source = (0..300)
            .map(|index| format!("pub struct Type{index};\n"))
            .collect::<String>();
        let extra_files = [("src/many.rs", source.as_str())];
        let (_source, _sidecar_dir, sidecar, config, _comparison) =
            indexed_fixture_with_extra_files(&extra_files);
        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
        let requested_scope = scope(&config);
        let resolved_scope = query.resolve_scope(&requested_scope).unwrap();
        let request = ShowRequest {
            scope: requested_scope,
            lookup: ShowLookup::Path(RepoPath::new("src/many.rs").unwrap()),
            page: super::super::query::PageRequest { cursor: None },
        };
        let started = Instant::now()
            - Duration::from_millis(config.query_limits.max_duration_ms.saturating_add(1));

        let rows = query
            .show_rows(&resolved_scope, &request, 0, started)
            .unwrap();

        assert!(rows.deadline_exceeded);
        assert!(rows.rows.is_empty());
    }

    #[test]
    fn status_observes_an_expired_deadline() {
        let (_source, _sidecar_dir, sidecar, config, _comparison) = indexed_fixture();
        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
        let started = Instant::now()
            - Duration::from_millis(config.query_limits.max_duration_ms.saturating_add(1));

        let response = query
            .status_at(
                &StatusRequest {
                    scope: scope(&config),
                },
                started,
            )
            .unwrap();

        assert_eq!(
            response.page.truncation.unwrap().reason,
            TruncationReason::Duration
        );
        assert!(response.data.statistics.is_none());
    }

    #[test]
    fn status_reports_counts_and_missing_show_is_actionable() {
        let (_source, _sidecar_dir, sidecar, config, _comparison) = indexed_fixture();
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

    #[test]
    fn analysis_and_extractor_changes_mark_the_snapshot_stale() {
        let (_source, _sidecar_dir, sidecar, config, current) = indexed_fixture();
        let snapshot = sidecar
            .published_snapshot(&repository(), &PublishedViewName::new("canonical").unwrap())
            .unwrap()
            .unwrap();
        for (comparison, reason) in [
            (
                FreshnessComparison {
                    analysis_config_digest: Digest::new("sha256", "aa").unwrap(),
                    ..current.clone()
                },
                "analysis_config_changed",
            ),
            (
                FreshnessComparison {
                    extractor_set_digest: Digest::new("sha256", "bb").unwrap(),
                    ..current.clone()
                },
                "extractor_set_changed",
            ),
        ] {
            let status =
                SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), Some(comparison))
                    .status(&StatusRequest {
                        scope: scope(&config),
                    })
                    .unwrap();

            assert_eq!(status.freshness.freshness, Freshness::Stale);
            assert_eq!(
                status.freshness.compared_manifest.as_ref(),
                Some(&snapshot.source_manifest_digest)
            );
            assert_eq!(status.freshness.reason_codes, vec![reason]);
        }
    }
}
