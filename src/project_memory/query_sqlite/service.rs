//! Read-only project memory query service.

use super::*;

impl MemoryQuery for SqliteMemoryQuery<'_> {
    fn status(
        &self,
        request: MemoryStatusRequest,
    ) -> Result<MemoryStatusResponse, MemoryQueryError> {
        let budget = EffectiveBudget::new(&request.scope.budget, &self.limits);
        let started = Instant::now();
        let resolved = self.resolve_scope_bounded(&request.scope, started, budget);
        let scope = match resolved {
            Ok(scope) => scope,
            Err(MemoryQueryError::Unavailable)
                if matches!(
                    request.scope.revision,
                    super::super::query::MemoryRevisionSelector::Published(_)
                ) =>
            {
                let deadline =
                    QueryDeadline::install(self.sidecar.connection(), started, budget.duration())?;
                let latest = self.latest_build(&request.scope.project)?;
                let retention = self.retention_statistics(&request.scope.project)?;
                drop(deadline);
                if started.elapsed() >= budget.duration() {
                    return Err(MemoryQueryError::BudgetExceeded(
                        MemoryTruncationReason::Duration,
                    ));
                }

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
                        retention: Some(retention),
                        recommended_action: Some(MemoryRetrievalAction::Build),
                        source_policy: source_policy_status(),
                    },
                });
            }
            Err(error) => return Err(error),
        };
        let deadline =
            QueryDeadline::install(self.sidecar.connection(), started, budget.duration())?;
        let latest = self.latest_build(&scope.revision.project)?;
        let statistics = self.statistics(&scope.revision.id)?;
        let retention = self.retention_statistics(&scope.revision.project)?;
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
                retention: Some(retention),
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
            return Err(invalid_request(
                const {
                    crate::project_memory::diagnostics::MemoryDiagnosticCode::from_static(
                        "query.filters",
                    )
                },
            ));
        }

        let started = Instant::now();
        let budget = EffectiveBudget::new(&request.scope.budget, &self.limits);
        let scope = self.resolve_scope_bounded(&request.scope, started, budget)?;
        let fingerprint = search_fingerprint(&request)?;
        let offset = decode_cursor(
            request.page.cursor.as_ref(),
            "search",
            &scope.revision.id,
            &fingerprint,
        )?;
        let deadline =
            QueryDeadline::install(self.sidecar.connection(), started, scope.budget.duration())?;
        let offset = usize::try_from(offset).map_err(|_| MemoryQueryError::StaleCursor)?;
        let keep = offset
            .saturating_add(scope.budget.max_results as usize)
            .saturating_add(1);
        let (hits, total, query_timed_out) = match self.search_hits(&scope, &request, keep, started)
        {
            Ok(result) => result,
            Err(MemoryQueryError::BudgetExceeded(MemoryTruncationReason::Duration)) => {
                (vec![], 0, true)
            }
            Err(error) => return Err(error),
        };
        drop(deadline);
        if offset > total {
            return Err(MemoryQueryError::StaleCursor);
        }

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
            return Err(invalid_request(
                const {
                    crate::project_memory::diagnostics::MemoryDiagnosticCode::from_static(
                        "context.seeds",
                    )
                },
            ));
        }

        if request.policy.relationship_kinds.len() > MAX_FILTERS {
            return Err(invalid_request(
                const {
                    crate::project_memory::diagnostics::MemoryDiagnosticCode::from_static(
                        "context.filters",
                    )
                },
            ));
        }

        let started = Instant::now();
        let budget = EffectiveBudget::new(&request.scope.budget, &self.limits);
        let scope = self.resolve_scope_bounded(&request.scope, started, budget)?;
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
                    truncation: Some(MemoryTruncationReason::Duration),
                }
            }
            Err(error) => return Err(error),
        };
        drop(deadline);
        let offset = usize::try_from(offset).map_err(|_| MemoryQueryError::StaleCursor)?;
        let entries = assembly
            .candidates
            .into_iter()
            .map(ContextPageEntry::Entity)
            .chain(
                assembly
                    .relationships
                    .into_iter()
                    .map(ContextPageEntry::Relationship),
            )
            .collect::<Vec<_>>();
        if offset > entries.len() {
            return Err(MemoryQueryError::StaleCursor);
        }

        let mut entries = entries.into_iter().skip(offset).peekable();
        let mut items = Vec::new();
        let mut relationships = Vec::new();
        let mut returned_bytes = 0_u64;
        let mut snippet_bytes = 0_u64;
        let mut reason = assembly.truncation;
        if request.policy.include_snippets && self.content.is_none() && reason.is_none() {
            reason = Some(MemoryTruncationReason::Capability);
        }

        let mut omitted_entry = false;
        loop {
            if items.len().saturating_add(relationships.len()) >= scope.budget.max_results as usize
            {
                if entries.peek().is_some() {
                    reason = Some(MemoryTruncationReason::Results);
                }

                break;
            }

            if started.elapsed() >= scope.budget.duration() {
                reason = Some(MemoryTruncationReason::Duration);
                break;
            }

            let Some(entry) = entries.next() else {
                break;
            };
            let (bytes, item, relationship) = match entry {
                ContextPageEntry::Entity(candidate) => {
                    let mut item = MemoryContextItem {
                        entity: candidate.entity,
                        snippet: None,
                        selection_reasons: candidate.selection_reasons,
                    };
                    let base_bytes = serialized_len(&item)?;
                    let remaining_response_bytes =
                        scope.budget.max_bytes.saturating_sub(returned_bytes);
                    if request.policy.include_snippets {
                        let remaining_snippet_bytes =
                            scope.budget.max_snippet_bytes.saturating_sub(snippet_bytes);
                        let response_snippet_bytes =
                            remaining_response_bytes.saturating_sub(base_bytes);
                        let snippet_limit = remaining_snippet_bytes.min(response_snippet_bytes);
                        item.snippet =
                            match self.attach_snippet(&scope, &item.entity, snippet_limit, started)
                            {
                                Ok(snippet) => snippet,
                                Err(MemoryQueryError::BudgetExceeded(
                                    MemoryTruncationReason::Duration,
                                )) => {
                                    reason = Some(MemoryTruncationReason::Duration);
                                    break;
                                }
                                Err(error) => return Err(error),
                            };
                        if self.content.is_some() && snippet_limit < remaining_snippet_bytes {
                            reason = Some(MemoryTruncationReason::Bytes);
                        }
                    }

                    let mut bytes = serialized_len(&item)?;
                    if bytes > remaining_response_bytes && item.snippet.take().is_some() {
                        reason = Some(MemoryTruncationReason::Bytes);
                        bytes = base_bytes;
                    }

                    (bytes, Some(item), None)
                }
                ContextPageEntry::Relationship(relationship) => {
                    (serialized_len(&relationship)?, None, Some(relationship))
                }
            };
            if returned_bytes.saturating_add(bytes) > scope.budget.max_bytes {
                if items.is_empty() && relationships.is_empty() {
                    return Err(MemoryQueryError::BudgetExceeded(
                        MemoryTruncationReason::Bytes,
                    ));
                }

                omitted_entry = true;
                reason = Some(MemoryTruncationReason::Bytes);
                break;
            }

            returned_bytes = returned_bytes.saturating_add(bytes);
            if let Some(item) = item {
                if let Some(snippet) = &item.snippet {
                    snippet_bytes = snippet_bytes.saturating_add(snippet.text.len() as u64);
                }

                items.push(item);
            } else if let Some(relationship) = relationship {
                relationships.push(relationship);
            }
        }

        let returned_results = items.len().saturating_add(relationships.len());
        let has_more = omitted_entry || entries.peek().is_some();
        let (diagnostics, diagnostics_timed_out) =
            self.diagnostics_with_deadline(&scope, started)?;
        if diagnostics_timed_out {
            reason = Some(MemoryTruncationReason::Duration);
        }

        let page = memory_page(
            reason,
            returned_results,
            returned_bytes,
            assembly.explored_depth,
            has_more,
            "context",
            &scope.revision.id,
            &fingerprint,
            offset + returned_results,
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
