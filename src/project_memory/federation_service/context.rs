//! Federated context retrieval and immutable cross-domain traversal.

use super::*;

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
        let backend_has_more = match &request.scope.target {
            FederatedTarget::Repository { repository: target } => {
                let (state, domain_results, domain_has_more) =
                    self.repository_search(&request, target, budget.remaining(started)?, desired)?;
                repository = Some(state);
                results.extend(domain_results);
                domain_has_more
            }
            FederatedTarget::Memory { memory: selector } => {
                let (state, domain_results, domain_has_more) =
                    self.memory_search(&request, selector, budget.remaining(started)?, desired)?;
                memory = Some(state);
                results.extend(domain_results);
                domain_has_more
            }
            FederatedTarget::All {
                repository: target,
                memory: selector,
            } => {
                let (repository_state, domain_results, repository_has_more) =
                    self.repository_search(&request, target, budget.remaining(started)?, desired)?;
                repository = Some(repository_state);
                results.extend(domain_results);
                if started.elapsed().as_millis() >= u128::from(budget.max_duration_ms) {
                    return Err(MemoryQueryError::BudgetExceeded(
                        MemoryTruncationReason::Duration,
                    ));
                }

                let (memory_state, domain_results, memory_has_more) =
                    self.memory_search(&request, selector, budget.remaining(started)?, desired)?;
                memory = Some(memory_state);
                results.extend(domain_results);
                repository_has_more || memory_has_more
            }
        };
        results.sort_by(federated_search_order);
        results.dedup_by(|left, right| search_result_key(left) == search_result_key(right));
        let revision_key = revision_key(repository.as_ref(), memory.as_ref())?;
        validate_cursor_revision(pending_cursor.as_ref(), &revision_key)?;
        if offset > results.len() {
            return Err(MemoryQueryError::StaleCursor);
        }

        let total = results.len();
        let mut candidates = results.into_iter().skip(offset).peekable();
        let mut returned = Vec::new();
        let candidate_cap_truncated = candidate_cap_truncated(desired, backend_has_more);
        let mut reason = candidate_cap_truncated.then_some(MemoryTruncationReason::Results);
        for result in candidates.by_ref() {
            if returned.len() >= budget.max_results {
                reason = Some(MemoryTruncationReason::Results);
                break;
            }

            if started.elapsed().as_millis() >= u128::from(budget.max_duration_ms) {
                reason = Some(MemoryTruncationReason::Duration);
                break;
            }

            returned.push(result);
        }

        let has_more = offset + returned.len() < total || candidates.peek().is_some();
        let page = federated_page(
            reason,
            returned.len(),
            0,
            0,
            has_more,
            "search",
            &fingerprint,
            &revision_key,
            offset + returned.len(),
        )?;
        let mut response = FederatedSearchResponse {
            wire_version: FEDERATION_WIRE_VERSION,
            project: request.scope.project,
            requested_domain: request.scope.target.domain(),
            repository,
            memory,
            federation_diagnostics: vec![],
            page,
            results: returned,
        };
        fit_federated_search_response(
            &mut response,
            budget.max_bytes,
            reason,
            offset,
            total,
            &fingerprint,
            &revision_key,
            started,
            budget.max_duration_ms,
        )?;
        Ok(response)
    }

    fn context(
        &self,
        request: FederatedContextRequest,
    ) -> Result<FederatedContextResponse, MemoryQueryError> {
        self.validate_scope(request.scope.wire_version)?;
        if request.seeds.is_empty() || request.seeds.len() > 32 {
            return Err(backend_error(
                const {
                    crate::project_memory::diagnostics::MemoryDiagnosticCode::from_static(
                        "context.seeds",
                    )
                },
            ));
        }

        let started = Instant::now();
        let budget = EffectiveBudget::new(&request.scope.budget, &self.limits);
        let fingerprint = request_fingerprint("context", &ContextCursorShape::try_from(&request)?)?;
        let pending_cursor = decode_cursor(request.cursor.as_ref(), "context", &fingerprint)?;
        let offset = usize::try_from(pending_cursor.as_ref().map_or(0, |cursor| cursor.offset))
            .map_err(|_| MemoryQueryError::StaleCursor)?;
        let desired = offset
            .saturating_add(budget.max_results)
            .saturating_add(1)
            .min(MAX_FEDERATION_CANDIDATES);
        let mut repository_state = None;
        let mut memory_state = None;
        let mut repository_responses: Vec<(ContextResponse, u32)> = Vec::new();
        let mut memory_responses: Vec<(MemoryContextResponse, u32)> = Vec::new();
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
                    .ok_or_else(|| backend_error(const { crate::project_memory::diagnostics::MemoryDiagnosticCode::from_static("context.seeds") }))?;
                repository_state = Some(repository_state_from_context(&response));
                repository_responses.push((response, 0));
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
                    .ok_or_else(|| backend_error(const { crate::project_memory::diagnostics::MemoryDiagnosticCode::from_static("context.seeds") }))?;
                memory_state = Some(memory_state_from_context(&response));
                memory_responses.push((response, 0));
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
                let (repository_seeds, memory_seeds) = split_all_seeds(&request.seeds);
                let mut cross_memory_seeds = Vec::new();
                if matches!(
                    request.memory_policy.direction,
                    EdgeDirection::Incoming | EdgeDirection::Both
                ) {
                    for seed in &repository_seeds {
                        for relationship in links.iter().filter(|relationship| {
                            relationship.provenance.resolution == MemoryResolutionState::Resolved
                                && cross_link_kind_allowed(&request.memory_policy, relationship)
                                && repository_seed_matches(seed, &relationship.target, snapshot_id)
                        }) {
                            cross_memory_seeds
                                .push(MemoryContextSeed::Entity(relationship.source.clone()));
                            cross_links.push(relationship.clone());
                        }
                    }
                }

                if let Some(response) = self.memory_context(
                    &request.scope.project,
                    selector,
                    &request,
                    memory_seeds,
                    budget.remaining(started)?,
                    desired,
                )? {
                    memory_responses.push((response, 0));
                }

                if let Some(response) = self.memory_context(
                    &request.scope.project,
                    selector,
                    &request,
                    cross_memory_seeds,
                    budget
                        .remaining(started)?
                        .with_max_depth(budget.max_depth.saturating_sub(1)),
                    desired,
                )? {
                    memory_responses.push((response, 1));
                }

                let mut cross_repository_seeds = Vec::new();
                if !memory_responses.is_empty() {
                    let selected = memory_responses
                        .iter()
                        .flat_map(|(response, _)| response.items.iter())
                        .filter(|item| {
                            item.selection_reasons
                                .iter()
                                .any(|reason| reason.as_str() == "context.seed")
                        })
                        .map(|item| item.entity.id.clone())
                        .collect::<BTreeSet<_>>();
                    if matches!(
                        request.memory_policy.direction,
                        EdgeDirection::Outgoing | EdgeDirection::Both
                    ) {
                        for relationship in links.iter().filter(|relationship| {
                            relationship.provenance.resolution == MemoryResolutionState::Resolved
                                && cross_link_kind_allowed(&request.memory_policy, relationship)
                                && selected.contains(&relationship.source)
                        }) {
                            if let Some(seed) =
                                repository_seed_from_target(&relationship.target, snapshot_id)
                            {
                                cross_repository_seeds.push(seed);
                                cross_links.push(relationship.clone());
                            }
                        }
                    }
                }

                cross_links.sort_by(|left, right| left.id.cmp(&right.id));
                cross_links.dedup_by(|left, right| left.id == right.id);
                if let Some(response) = self.repository_context(
                    target,
                    &request,
                    repository_seeds,
                    budget.remaining(started)?,
                    desired,
                )? {
                    repository_responses.push((response, 0));
                }

                if let Some(response) = self.repository_context(
                    target,
                    &request,
                    cross_repository_seeds,
                    budget
                        .remaining(started)?
                        .with_max_depth(budget.max_depth.saturating_sub(1)),
                    desired,
                )? {
                    repository_responses.push((response, 1));
                }

                repository_state = Some(repository_responses.first().map_or_else(
                    || repository_state_from_status(&repository_status),
                    |(response, _)| repository_state_from_context(response),
                ));
                memory_state = Some(memory_responses.first().map_or_else(
                    || memory_state_from_status(&memory_status),
                    |(response, _)| memory_state_from_context(response),
                ));
            }
        }

        let revision_key = revision_key(repository_state.as_ref(), memory_state.as_ref())?;
        validate_cursor_revision(pending_cursor.as_ref(), &revision_key)?;
        let mut items = Vec::new();
        for (response, _) in &repository_responses {
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

        for (response, _) in &memory_responses {
            items.extend(response.items.iter().cloned().map(|mut item| {
                if cross_links
                    .iter()
                    .any(|relationship| relationship.source == item.entity.id)
                {
                    item.selection_reasons.push(diagnostic_code(
                        const {
                            crate::project_memory::diagnostics::MemoryDiagnosticCode::from_static(
                                "context.repositorylink",
                            )
                        },
                    ));
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
        let backend_has_more = repository_responses
            .iter()
            .any(|(response, _)| response.page.next_cursor.is_some())
            || memory_responses
                .iter()
                .any(|(response, _)| response.page.next_cursor.is_some());
        let candidate_cap_truncated = candidate_cap_truncated(desired, backend_has_more);
        let domain_truncation = repository_responses
            .iter()
            .filter_map(|(response, _)| {
                response
                    .page
                    .truncation
                    .as_ref()
                    .map(|truncation| repository_truncation_reason(truncation.reason))
            })
            .chain(memory_responses.iter().filter_map(|(response, _)| {
                response
                    .page
                    .truncation
                    .as_ref()
                    .map(|truncation| truncation.reason)
            }))
            .fold(None, |current, reason| {
                stronger_truncation(current, Some(reason))
            });
        let mut reason = if link_duration_exceeded {
            Some(MemoryTruncationReason::Duration)
        } else if link_results_truncated || candidate_cap_truncated {
            Some(MemoryTruncationReason::Results)
        } else {
            domain_truncation
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
        let explored_depth = repository_responses
            .iter()
            .map(|(response, domain_hops)| {
                (*domain_hops).saturating_add(
                    response
                        .page
                        .truncation
                        .as_ref()
                        .map_or(0, |truncation| truncation.explored_depth),
                )
            })
            .chain(memory_responses.iter().map(|(response, domain_hops)| {
                (*domain_hops).saturating_add(
                    response
                        .page
                        .truncation
                        .as_ref()
                        .map_or(0, |truncation| truncation.explored_depth),
                )
            }))
            .max()
            .unwrap_or(0);
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
        let mut memory_relationships = memory_responses
            .iter()
            .flat_map(|(response, _)| response.relationships.iter().cloned())
            .collect::<Vec<_>>();
        memory_relationships.retain(|relationship| {
            returned_memory_ids.contains(&relationship.source)
                || matches!(
                    &relationship.target,
                    MemoryRelationshipTarget::MemoryEntity { entity_id }
                        if returned_memory_ids.contains(entity_id)
                )
        });
        memory_relationships.sort_by(|left, right| left.id.cmp(&right.id));
        memory_relationships.dedup_by(|left, right| left.id == right.id);
        cross_links.retain(|relationship| returned_memory_ids.contains(&relationship.source));
        let mut repository_snippets = repository_responses
            .into_iter()
            .flat_map(|(response, _)| response.data.snippets)
            .collect::<Vec<_>>();
        repository_snippets.retain(|snippet| returned_repository_paths.contains(&snippet.path));
        repository_snippets.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| snippet_span_key(left).cmp(&snippet_span_key(right)))
        });
        repository_snippets.dedup_by(|left, right| {
            left.path == right.path && snippet_span_key(left) == snippet_span_key(right)
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
            has_more,
            "context",
            &fingerprint,
            &revision_key,
            offset + returned.len(),
        )?;
        let mut response = FederatedContextResponse {
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
        };
        fit_federated_context_response(
            &mut response,
            budget.max_bytes,
            reason,
            explored_depth,
            offset,
            total,
            &fingerprint,
            &revision_key,
            started,
            budget.max_duration_ms,
        )?;
        Ok(response)
    }
}
