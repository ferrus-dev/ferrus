use super::*;

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

    pub(super) fn resolve_scope(
        &self,
        scope: &super::super::query::QueryScope,
    ) -> Result<ResolvedScope, QueryError> {
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

    pub(super) fn latest_build(
        &self,
        repository: &RepositoryRef,
    ) -> Result<Option<GraphBuild>, QueryError> {
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

    pub(super) fn unpublished_index_error(&self, repository: &RepositoryRef) -> QueryError {
        match self.latest_build(repository) {
            Ok(Some(build)) if build.state == BuildState::Building => index_building_error(),
            Ok(Some(build)) if build.state == BuildState::Failed => index_failed_error(),
            _ => not_built_error(),
        }
    }

    pub(super) fn status_at(
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
                    task_view: None,
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
                        task_view_status: None,
                        fallback: None,
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

    pub(super) fn diagnostics(
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

    pub(super) fn diagnostics_with_deadline(
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

    pub(super) fn statistics(
        &self,
        snapshot: &SnapshotId,
    ) -> Result<SnapshotStatistics, QueryError> {
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

    pub(super) fn search_rows(
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

    pub(super) fn show_rows(
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

    pub(super) fn node(
        &self,
        snapshot: &SnapshotId,
        id: &NodeId,
    ) -> Result<Option<GraphNode>, QueryError> {
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

    pub(super) fn edges<'edge>(
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

    pub(super) fn context_seed_nodes(
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

    pub(super) fn assemble_context(
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
