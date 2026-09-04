//! Implement GraphQuery with service limits, snapshot-bound pagination, and freshness reporting.

use super::*;

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
            task_view: None,
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
            task_view: None,
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
            task_view: None,
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
        let fingerprint = context_cursor_fingerprint(request, scope.budget.max_depth)?;
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
            task_view: None,
            freshness: scope.freshness,
            diagnostics,
            page,
            data: ContextData {
                items,
                snippets: vec![],
            },
        })
    }
}
