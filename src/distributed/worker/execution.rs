//! Leased stateless worker execution.

use super::*;

impl StatelessIndexWorker {
    pub fn new(limits: WorkerLimits) -> Self {
        Self {
            limits,
            #[cfg(test)]
            elapsed_ms: None,
        }
    }

    pub fn execute<C, O, F>(
        &self,
        request: &ExecuteIndexJobRequest,
        coordinator: &C,
        objects: &O,
        facts: &mut F,
    ) -> Result<WorkerExecutionOutcome, WorkerError>
    where
        C: IndexJobCoordinator,
        O: TenantObjectStore,
        F: FactBatchStore,
    {
        self.execute_with_repository_snapshots(request, coordinator, objects, facts, None, &[])
    }

    /// Execute a memory job with the exact current and task/run origin graph
    /// snapshots named by its immutable job input. A pinned job fails closed
    /// unless the caller supplies the complete selected snapshot set.
    pub fn execute_with_repository_snapshots<C, O, F>(
        &self,
        request: &ExecuteIndexJobRequest,
        coordinator: &C,
        objects: &O,
        facts: &mut F,
        repository_snapshot: Option<&StoredRemoteGraphSnapshot>,
        repository_origin_snapshots: &[StoredRemoteGraphSnapshot],
    ) -> Result<WorkerExecutionOutcome, WorkerError>
    where
        C: IndexJobCoordinator,
        O: TenantObjectStore,
        F: FactBatchStore,
    {
        let started = Instant::now();
        self.validate_request(request)?;
        let deadline = WorkerDeadline {
            started,
            limit_ms: effective_worker_duration_ms(
                self.limits.max_job_duration_ms.get(),
                Utc::now(),
                request.job.deadline_at,
            )?,
        };
        let object_protection = objects.protection();
        if !object_protection.authenticated_transport || !object_protection.encrypted_at_rest {
            return Err(WorkerError::InvalidSandbox);
        }

        require_fact_protection(facts)?;
        self.authorize(request, coordinator, deadline)?;

        let (target, payloads) = match &request.job.spec.input {
            IndexInputRef::Repository(reference) => {
                let bytes =
                    self.read_object_bounded(objects, &reference.manifest_object, deadline)?;
                let body: RepositorySourceManifestBody =
                    serde_json::from_slice(&bytes).map_err(|_| WorkerError::InvalidInput)?;
                let diagnostics =
                    checked_sum(body.summary.source_diagnostic_codes.values().copied())?;
                self.check_manifest_limits(
                    body.files.len().saturating_add(1),
                    body.summary
                        .total_bytes
                        .checked_add(bytes.len() as u64)
                        .ok_or(WorkerError::InputLimitExceeded)?,
                    diagnostics,
                )?;
                let manifest = RepositorySourceManifest {
                    reference: reference.clone(),
                    body,
                };
                manifest
                    .validate::<(), ()>()
                    .map_err(|_| WorkerError::InvalidInput)?;
                let (target, fragment) =
                    self.extract_repository(request, coordinator, objects, &manifest, deadline)?;
                (
                    target,
                    graph_payloads(fragment, self.limits.max_facts_per_batch)?,
                )
            }
            IndexInputRef::Memory(reference) => {
                let bytes =
                    self.read_object_bounded(objects, &reference.manifest_object, deadline)?;
                let body: MemorySourceManifestBody =
                    serde_json::from_slice(&bytes).map_err(|_| WorkerError::InvalidInput)?;
                self.check_manifest_limits(
                    body.sources.len().saturating_add(1),
                    body.summary
                        .total_bytes
                        .checked_add(bytes.len() as u64)
                        .ok_or(WorkerError::InputLimitExceeded)?,
                    0,
                )?;
                let manifest = MemorySourceManifest {
                    reference: reference.clone(),
                    body,
                };
                manifest
                    .validate::<(), ()>()
                    .map_err(|_| WorkerError::InvalidInput)?;
                let extracted = self.extract_memory(
                    request,
                    coordinator,
                    objects,
                    &manifest,
                    MemoryRepositorySnapshots {
                        current: repository_snapshot,
                        origins: repository_origin_snapshots,
                    },
                    deadline,
                )?;
                (
                    extracted.target,
                    memory_payloads(
                        extracted.entities,
                        extracted.relationships,
                        extracted.diagnostics,
                        self.limits.max_facts_per_batch,
                    )?,
                )
            }
        };

        let shard = FactShardId::new(match request.job.job.kind {
            super::super::protocol::IndexJobKind::RepositoryGraph => "repository-all",
            super::super::protocol::IndexJobKind::ProjectMemory => "memory-all",
        })
        .map_err(|_| WorkerError::InvalidInput)?;
        let payloads =
            self.size_payloads_for_batches(request, &target, &shard, payloads, deadline)?;
        let emitted_facts = payloads
            .iter()
            .map(payload_fact_count)
            .try_fold(0u64, |total, count| total.checked_add(count))
            .ok_or(WorkerError::OutputLimitExceeded)?;
        if emitted_facts > self.limits.max_total_facts.get() {
            return Err(WorkerError::OutputLimitExceeded);
        }

        let mut stored_batches = 0u64;
        let mut reused_batches = 0u64;
        let mut output_bytes = 0u64;
        let write_authority = FactBatchWriteAuthority {
            worker_id: request.worker_id.clone(),
            lease_generation: request.lease_generation,
        };
        let write_deadline = deadline
            .started
            .checked_add(Duration::from_millis(deadline.limit_ms))
            .ok_or(WorkerError::DeadlineExceeded)?;
        let last = payloads.len().saturating_sub(1);
        for (index, payload) in payloads.into_iter().enumerate() {
            self.check_deadline(deadline)?;
            self.authorize(request, coordinator, deadline)?;
            let sequence = u32::try_from(index).map_err(|_| WorkerError::OutputLimitExceeded)?;
            let batch = FactBatch::new(
                request.job.job.clone(),
                target.clone(),
                shard.clone(),
                sequence,
                request.job.spec.semantics.extractor_set_digest.clone(),
                index == last,
                payload,
            )
            .map_err(|_| WorkerError::FactConflict)?;
            let byte_len = serde_json::to_vec(&batch)
                .map_err(|_| WorkerError::FactConflict)?
                .len() as u64;
            if byte_len > self.limits.max_batch_bytes.get() {
                return Err(WorkerError::OutputLimitExceeded);
            }

            output_bytes = output_bytes
                .checked_add(byte_len)
                .ok_or(WorkerError::OutputLimitExceeded)?;
            if output_bytes > self.limits.max_output_bytes.get() {
                return Err(WorkerError::OutputLimitExceeded);
            }

            let put = facts
                .put(&batch, &write_authority, write_deadline)
                .map_err(|_| WorkerError::FactStore)?;
            self.check_deadline(deadline)?;
            match put {
                PutFactBatchOutcome::Stored => stored_batches += 1,
                PutFactBatchOutcome::Reused => reused_batches += 1,
                PutFactBatchOutcome::DeadlineExceeded => {
                    return Err(WorkerError::DeadlineExceeded);
                }
            }
        }

        let progress = match facts
            .progress(&request.job.job, write_deadline)
            .map_err(|_| WorkerError::FactStore)?
        {
            FactBatchProgressOutcome::Available(progress) => progress,
            FactBatchProgressOutcome::DeadlineExceeded => {
                return Err(WorkerError::DeadlineExceeded);
            }
        };
        if !progress_is_complete(&progress) {
            return Err(WorkerError::FactStore);
        }

        Ok(WorkerExecutionOutcome {
            target,
            progress,
            stored_batches,
            reused_batches,
            emitted_facts,
        })
    }

    pub(super) fn validate_request(
        &self,
        request: &ExecuteIndexJobRequest,
    ) -> Result<(), WorkerError> {
        if request.protocol_version != DISTRIBUTED_WORKER_PROTOCOL_VERSION
            || request.job.validate().is_err()
            || request.job.state != IndexJobState::Running
            || request.job.cancellation_requested
            || request.job.lease.as_ref().is_none_or(|lease| {
                lease.worker_id != request.worker_id
                    || lease.generation != request.lease_generation
                    || lease.expires_at <= Utc::now()
            })
            || request.job.deadline_at <= Utc::now()
        {
            return Err(WorkerError::InvalidInput);
        }

        Ok(())
    }

    pub(super) fn authorize<C: IndexJobCoordinator>(
        &self,
        request: &ExecuteIndexJobRequest,
        coordinator: &C,
        deadline: WorkerDeadline,
    ) -> Result<(), WorkerError> {
        let authorization = InspectIndexJobRequest {
            protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            request_id: RequestId::new(format!("worker-auth-{}", request.job.job.job_id))
                .map_err(|_| WorkerError::InvalidInput)?,
            job: request.job.job.clone(),
        };
        let inspect_deadline = deadline
            .started
            .checked_add(Duration::from_millis(deadline.limit_ms))
            .ok_or(WorkerError::DeadlineExceeded)?;
        let inspected = coordinator.inspect_bounded(&authorization, inspect_deadline);
        self.check_deadline(deadline)?;
        let current = match inspected.map_err(|_| WorkerError::AuthorityLost)? {
            BoundedJobInspection::Found(record) => *record,
            BoundedJobInspection::NotFound => return Err(WorkerError::AuthorityLost),
            BoundedJobInspection::DeadlineExceeded => {
                return Err(WorkerError::DeadlineExceeded);
            }
        };
        let now = Utc::now();
        let authorized = current.spec == request.job.spec
            && current.state == IndexJobState::Running
            && !current.cancellation_requested
            && current.deadline_at > now
            && current.lease.as_ref().is_some_and(|lease| {
                lease.worker_id == request.worker_id
                    && lease.generation == request.lease_generation
                    && lease.expires_at > now
            });
        authorized.then_some(()).ok_or(WorkerError::AuthorityLost)
    }

    pub(super) fn extract_repository<C, O>(
        &self,
        request: &ExecuteIndexJobRequest,
        coordinator: &C,
        objects: &O,
        remote: &RepositorySourceManifest,
        deadline: WorkerDeadline,
    ) -> Result<(FactTarget, GraphFragment), WorkerError>
    where
        C: IndexJobCoordinator,
        O: TenantObjectStore,
    {
        let semantics = &request.job.spec.semantics;
        let active = repository_extractor_selection(&semantics.extractor_set_digest)
            .ok_or(WorkerError::IncompatibleSemantics)?;
        if remote.body.extractor_set_digest != semantics.extractor_set_digest
            || remote.body.source_revision.analysis_config_digest
                != semantics.semantic_config_digest
            || semantics.model_version.get() != GRAPH_MODEL_VERSION
        {
            return Err(WorkerError::IncompatibleSemantics);
        }

        let diagnostic_count = checked_sum(
            remote
                .body
                .summary
                .source_diagnostic_codes
                .values()
                .copied(),
        )?;
        let manifest = SourceManifest {
            revision: remote.body.source_revision.clone(),
            extractor_set_digest: remote.body.extractor_set_digest.clone(),
            files: remote
                .body
                .files
                .iter()
                .map(|file| SourceFileDescriptor {
                    path: file.path.clone(),
                    content_identity: file.content_identity.clone(),
                    byte_len: file.byte_len,
                    file_mode: file.file_mode,
                })
                .collect(),
            diagnostics: remote
                .body
                .summary
                .source_diagnostic_codes
                .iter()
                .flat_map(|(code, count)| {
                    std::iter::repeat_n(
                        SourceDiagnostic {
                            code: code.clone(),
                            path: None,
                        },
                        usize::try_from(*count).unwrap_or(usize::MAX),
                    )
                })
                .take(usize::try_from(self.limits.max_diagnostics.get()).unwrap_or(usize::MAX))
                .collect(),
            metrics: SourceDiscoveryMetrics {
                included: remote.body.files.len() as u64,
                skipped: diagnostic_count,
                total_bytes: remote.body.summary.total_bytes,
                ..SourceDiscoveryMetrics::default()
            },
        };
        let snapshot_id = snapshot_identity(&manifest).map_err(|_| WorkerError::InvalidInput)?;
        if snapshot_id != remote.reference.expected_snapshot_id {
            return Err(WorkerError::InvalidInput);
        }

        let build_id = BuildId::new(format!("remote-build:{}", request.job.job.job_id))
            .map_err(|_| WorkerError::InvalidInput)?;
        let context = ExtractionContext {
            snapshot_id: snapshot_id.clone(),
            build_id: build_id.clone(),
            repository: manifest.revision.repository.clone(),
            max_facts_per_file: self.limits.max_facts_per_source.get(),
            max_parser_duration_ms: self.limits.max_parser_duration_ms.get(),
            max_diagnostics: self.limits.max_diagnostics.get(),
        };
        let mut merger = GraphMerger::default();
        let mut extracted_facts = 0u64;
        let mut extracted_diagnostics = 0u64;
        if active.generic_enabled {
            let remaining_job_duration_ms = self.remaining_job_duration_ms(deadline)?;
            let extraction_context = extraction_context_for_attempt(
                &context,
                remaining_diagnostics(&self.limits, extracted_diagnostics),
                remaining_job_duration_ms,
            );
            let fragment =
                GenericExtractor::new().repository_fragment(&extraction_context, &manifest);
            self.check_deadline(deadline)?;
            reserve_extracted_diagnostics(
                &mut extracted_diagnostics,
                fragment.diagnostics.len(),
                self.limits.max_diagnostics.get(),
            )?;
            reserve_extracted_facts(
                &mut extracted_facts,
                graph_fragment_fact_count(&fragment)?,
                self.limits.max_total_facts.get(),
            )?;
            merger.merge(fragment)?;
        }

        let source_diagnostics = GraphFragment {
            diagnostics: manifest
                .diagnostics
                .iter()
                .take(
                    usize::try_from(remaining_diagnostics(&self.limits, extracted_diagnostics))
                        .unwrap_or(usize::MAX),
                )
                .map(|diagnostic| GraphDiagnostic {
                    build_id: build_id.clone(),
                    snapshot_id: Some(snapshot_id.clone()),
                    severity: DiagnosticSeverity::Warning,
                    code: diagnostic.code.clone(),
                    location: None,
                    metrics: BTreeMap::new(),
                })
                .collect(),
            ..GraphFragment::default()
        };
        reserve_extracted_diagnostics(
            &mut extracted_diagnostics,
            source_diagnostics.diagnostics.len(),
            self.limits.max_diagnostics.get(),
        )?;
        reserve_extracted_facts(
            &mut extracted_facts,
            graph_fragment_fact_count(&source_diagnostics)?,
            self.limits.max_total_facts.get(),
        )?;
        merger.merge(source_diagnostics)?;

        let generic = GenericExtractor::new();
        // Remote workers already run inside the declared hard sandbox and
        // cannot spawn the local extractor's parser subprocess.
        let cargo = CargoExtractor::new_in_process_sandboxed();
        let rust = RustSyntaxExtractor::new();
        let extractors: [&dyn DynExtractor; 3] = [&generic, &cargo, &rust];
        for (remote_file, file) in remote.body.files.iter().zip(&manifest.files) {
            let mut source_facts = 0u64;
            self.check_deadline(deadline)?;
            self.authorize(request, coordinator, deadline)?;
            if remote_file.byte_len > self.limits.max_object_bytes.get() {
                return Err(WorkerError::InputLimitExceeded);
            }

            let content = self.read_object_bounded(objects, &remote_file.object, deadline)?;
            self.check_deadline(deadline)?;
            if content.len() as u64 != file.byte_len {
                return Err(WorkerError::SourceUnavailable);
            }

            for extractor in extractors
                .iter()
                .copied()
                .filter(|item| active.file_ids.contains(item.identity().id.as_str()))
                .filter(|item| item.supports(file))
            {
                let remaining_job_duration_ms = self.remaining_job_duration_ms(deadline)?;
                let extraction_context = extraction_context_for_attempt(
                    &context,
                    remaining_diagnostics(&self.limits, extracted_diagnostics),
                    remaining_job_duration_ms,
                );
                let fragment = extractor
                    .extract(FileExtractionInput {
                        context: &extraction_context,
                        file,
                        content: &content,
                    })
                    .map_err(|_| WorkerError::ExtractionFailed)?;
                self.check_deadline(deadline)?;
                reserve_extracted_diagnostics(
                    &mut extracted_diagnostics,
                    fragment.diagnostics.len(),
                    self.limits.max_diagnostics.get(),
                )?;
                source_facts = source_facts
                    .checked_add(graph_fragment_fact_count(&fragment)?)
                    .ok_or(WorkerError::OutputLimitExceeded)?;
                if source_facts > self.limits.max_facts_per_source.get() {
                    return Err(WorkerError::OutputLimitExceeded);
                }

                reserve_extracted_facts(
                    &mut extracted_facts,
                    graph_fragment_fact_count(&fragment)?,
                    self.limits.max_total_facts.get(),
                )?;
                merger.merge(fragment)?;
            }
        }

        let unresolved = merger.finish(&context)?;
        let unresolved_facts = graph_fragment_fact_count(&unresolved)?;
        if unresolved_facts > self.limits.max_total_facts.get() {
            return Err(WorkerError::OutputLimitExceeded);
        }

        let remaining_output_facts = self
            .limits
            .max_total_facts
            .get()
            .saturating_sub(unresolved_facts);
        let graph = if active.resolver_enabled {
            let remaining_job_duration_ms = self.remaining_job_duration_ms(deadline)?;
            ConservativeResolver::new()
                .resolve(CrossFileResolutionInput {
                    context: &context,
                    manifest: &manifest,
                    fragment: unresolved,
                    budget: ResolutionBudget {
                        max_relationships: self.limits.max_total_facts.get(),
                        max_added_relationships: remaining_output_facts,
                        max_duration_ms: self
                            .limits
                            .max_resolver_duration_ms
                            .get()
                            .min(remaining_job_duration_ms),
                        max_diagnostics: remaining_diagnostics(&self.limits, extracted_diagnostics),
                    },
                })
                .map_err(|_| WorkerError::ExtractionFailed)?
        } else {
            unresolved
        };
        if graph_fragment_fact_count(&graph)? > self.limits.max_total_facts.get() {
            return Err(WorkerError::OutputLimitExceeded);
        }

        if graph.diagnostics.len() as u64 > self.limits.max_diagnostics.get() {
            return Err(WorkerError::OutputLimitExceeded);
        }

        self.check_deadline(deadline)?;
        Ok((
            FactTarget::RepositoryGraph {
                snapshot: RemoteGraphSnapshotRef {
                    repository: remote.body.repository.clone(),
                    snapshot_id,
                },
                repository_identity: remote.body.source_revision.repository.clone(),
                build_id,
            },
            graph,
        ))
    }

    pub(super) fn extract_memory<C, O>(
        &self,
        request: &ExecuteIndexJobRequest,
        coordinator: &C,
        objects: &O,
        remote: &MemorySourceManifest,
        repository_snapshots: MemoryRepositorySnapshots<'_>,
        deadline: WorkerDeadline,
    ) -> Result<MemoryExtractionOutput, WorkerError>
    where
        C: IndexJobCoordinator,
        O: TenantObjectStore,
    {
        let semantics = &request.job.spec.semantics;
        if remote.body.extractor_set_digest != semantics.extractor_set_digest
            || semantics.extractor_set_digest
                != built_in_extractor_set_digest().map_err(|_| WorkerError::InvalidInput)?
            || semantics.semantic_config_digest != remote.body.memory_policy_digest
            || semantics.model_version.get() != MEMORY_MODEL_VERSION
        {
            return Err(WorkerError::IncompatibleSemantics);
        }

        let unique_sources = remote
            .body
            .sources
            .iter()
            .map(|source| serde_json::to_vec(&(source.category, &source.locator)))
            .collect::<Result<std::collections::BTreeSet<_>, _>>()
            .map_err(|_| WorkerError::InvalidInput)?;
        if unique_sources.len() != remote.body.sources.len() {
            return Err(WorkerError::InvalidInput);
        }

        let manifest = AuthorizedSourceManifest {
            project: remote.body.project_identity.clone(),
            policy_digest: remote.body.memory_policy_digest.clone(),
            source_set_digest: remote.body.source_set_digest.clone(),
            extractor_set_digest: remote.body.extractor_set_digest.clone(),
            sources: remote
                .body
                .sources
                .iter()
                .map(|source| AuthorizedSourceDescriptor {
                    project: remote.body.project_identity.clone(),
                    category: source.category,
                    locator: source.locator.clone(),
                    fingerprint: source.source_fingerprint.clone(),
                    byte_len: source.sanitized_byte_len,
                })
                .collect(),
        };
        manifest.validate().map_err(|_| WorkerError::InvalidInput)?;
        let build_id =
            MemoryBuildId::new(format!("remote-memory-build:{}", request.job.job.job_id))
                .map_err(|_| WorkerError::InvalidInput)?;
        let revision = MemoryRevision::from_manifest(&manifest, build_id.clone())
            .map_err(|_| WorkerError::InvalidInput)?;
        if revision.id != remote.reference.expected_revision_id {
            return Err(WorkerError::InvalidInput);
        }

        let context = MemoryExtractionContext {
            project: manifest.project.clone(),
            revision_id: revision.id.clone(),
            build_id: build_id.clone(),
            indexed_at: request.job.created_at,
            max_entities_per_source: self.limits.max_facts_per_source.get(),
            max_relationships_per_source: self.limits.max_facts_per_source.get(),
            max_parser_duration_ms: self.limits.max_parser_duration_ms.get(),
            max_diagnostics: self.limits.max_diagnostics.get(),
        };
        let extractors = built_in_extractors();
        let mut entities = BTreeMap::<MemoryEntityId, MemoryEntity>::new();
        let mut relationships = BTreeMap::<MemoryRelationshipId, MemoryRelationship>::new();
        let mut diagnostics = Vec::new();
        let mut extracted_facts = 0u64;
        for (remote_source, source) in remote.body.sources.iter().zip(&manifest.sources) {
            self.check_deadline(deadline)?;
            self.authorize(request, coordinator, deadline)?;
            if remote_source.sanitized_byte_len > self.limits.max_object_bytes.get() {
                return Err(WorkerError::InputLimitExceeded);
            }

            let content = self.read_object_bounded(objects, &remote_source.object, deadline)?;
            self.check_deadline(deadline)?;
            if content.len() as u64 != remote_source.sanitized_byte_len {
                return Err(WorkerError::SourceUnavailable);
            }

            if !verify_sanitized_memory_source(remote_source, &content) {
                return Err(WorkerError::InvalidInput);
            }

            let Some(extractor) = extractors
                .iter()
                .find(|extractor| extractor.supports(source.category))
            else {
                return Err(WorkerError::IncompatibleSemantics);
            };
            let mut extraction_context = context.clone();
            extraction_context.max_parser_duration_ms = extraction_context
                .max_parser_duration_ms
                .min(self.remaining_job_duration_ms(deadline)?);
            match extractor.extract(MemoryExtractionInput {
                context: &extraction_context,
                source,
                content: &content,
            }) {
                Ok(fragment) => {
                    self.check_deadline(deadline)?;
                    let source_facts = memory_fragment_fact_count(&fragment)?;
                    if source_facts > self.limits.max_facts_per_source.get() {
                        return Err(WorkerError::OutputLimitExceeded);
                    }

                    reserve_extracted_facts(
                        &mut extracted_facts,
                        source_facts,
                        self.limits.max_total_facts.get(),
                    )?;
                    merge_memory_fragment(&mut entities, &mut relationships, fragment)?;
                }
                Err(failure) => {
                    self.check_deadline(deadline)?;
                    if diagnostics.len() as u64 >= self.limits.max_diagnostics.get() {
                        return Err(WorkerError::OutputLimitExceeded);
                    }

                    reserve_extracted_facts(
                        &mut extracted_facts,
                        1,
                        self.limits.max_total_facts.get(),
                    )?;
                    diagnostics.push(MemoryDiagnostic {
                        build_id: build_id.clone(),
                        revision_id: revision.id.clone(),
                        severity: MemoryDiagnosticSeverity::Warning,
                        code: failure.code,
                        source_category: Some(source.category),
                        entity_id: None,
                        relationship_id: None,
                        metrics: BTreeMap::new(),
                    });
                }
            }
        }

        self.check_deadline(deadline)?;
        let mut supplied_origin_refs = repository_snapshots
            .origins
            .iter()
            .map(|snapshot| snapshot.record.snapshot.clone())
            .collect::<Vec<_>>();
        supplied_origin_refs.sort();
        if supplied_origin_refs != remote.reference.repository_origin_snapshots {
            return Err(WorkerError::InvalidInput);
        }

        let repository_links = match (
            remote.reference.repository_snapshot.as_ref(),
            repository_snapshots.current,
        ) {
            (None, None) => None,
            (Some(expected), Some(snapshot)) if &snapshot.record.snapshot == expected => {
                if repository_snapshots.origins.iter().any(|origin| {
                    origin.record.snapshot.repository != expected.repository
                        || origin.record.repository_identity != snapshot.record.repository_identity
                }) {
                    return Err(WorkerError::InvalidInput);
                }

                let origin_snapshots = repository_snapshots
                    .origins
                    .iter()
                    .map(|origin| RepositoryLinkSnapshot {
                        snapshot_id: &origin.record.snapshot.snapshot_id,
                        nodes: &origin.nodes,
                    })
                    .collect::<Vec<_>>();
                let commit = resolve_repository_links_for_snapshot(
                    &revision,
                    &entities.values().cloned().collect::<Vec<_>>(),
                    RepositoryLinkSnapshotSet {
                        repository: &snapshot.record.repository_identity,
                        current: RepositoryLinkSnapshot {
                            snapshot_id: &expected.snapshot_id,
                            nodes: &snapshot.nodes,
                        },
                        origins: &origin_snapshots,
                    },
                    request.job.created_at,
                    || self.check_deadline(deadline).is_err(),
                )
                .map_err(|error| match error {
                    SnapshotLinkResolutionError::InvalidSnapshot => WorkerError::ExtractionFailed,
                    SnapshotLinkResolutionError::DeadlineExceeded => WorkerError::DeadlineExceeded,
                })?;
                let link_facts = checked_sum([
                    commit.relationships.len() as u64,
                    commit.diagnostics.len() as u64,
                ])?;
                reserve_extracted_facts(
                    &mut extracted_facts,
                    link_facts,
                    self.limits.max_total_facts.get(),
                )?;
                if diagnostics.len().saturating_add(commit.diagnostics.len()) as u64
                    > self.limits.max_diagnostics.get()
                {
                    return Err(WorkerError::OutputLimitExceeded);
                }

                for relationship in commit.relationships {
                    if relationships
                        .insert(relationship.id.clone(), relationship)
                        .is_some()
                    {
                        return Err(WorkerError::FactConflict);
                    }
                }

                diagnostics.extend(commit.diagnostics);
                Some(Box::new(RemoteMemoryLinkSetTarget {
                    graph: expected.clone(),
                    origin_graphs: remote.reference.repository_origin_snapshots.clone(),
                    link_set: commit.link_set,
                }))
            }
            _ => return Err(WorkerError::InvalidInput),
        };
        self.check_deadline(deadline)?;
        Ok(MemoryExtractionOutput {
            target: FactTarget::ProjectMemory {
                revision: RemoteMemoryRevisionRef {
                    project: remote.body.project.clone(),
                    revision_id: revision.id,
                },
                project_identity: remote.body.project_identity.clone(),
                build_id,
                repository_links,
            },
            entities: entities.into_values().collect(),
            relationships: relationships.into_values().collect(),
            diagnostics,
        })
    }

    pub(super) fn check_manifest_limits(
        &self,
        objects: usize,
        bytes: u64,
        diagnostics: u64,
    ) -> Result<(), WorkerError> {
        if objects as u64 > self.limits.max_input_objects.get()
            || bytes > self.limits.max_input_bytes.get()
            || diagnostics > self.limits.max_diagnostics.get()
        {
            return Err(WorkerError::InputLimitExceeded);
        }

        Ok(())
    }

    pub(super) fn read_object_bounded<O: TenantObjectStore>(
        &self,
        objects: &O,
        object: &TenantObjectRef,
        deadline: WorkerDeadline,
    ) -> Result<Vec<u8>, WorkerError> {
        let read_deadline = deadline
            .started
            .checked_add(Duration::from_millis(deadline.limit_ms))
            .ok_or(WorkerError::DeadlineExceeded)?;
        match objects
            .read_verified_bounded(object, self.limits.max_object_bytes.get(), read_deadline)
            .map_err(|_| WorkerError::SourceUnavailable)?
        {
            BoundedObjectRead::Content(bytes) => {
                self.check_object_bytes(bytes.len(), deadline)?;
                Ok(bytes)
            }
            BoundedObjectRead::DeadlineExceeded => Err(WorkerError::DeadlineExceeded),
            BoundedObjectRead::SizeExceeded => Err(WorkerError::InputLimitExceeded),
        }
    }

    pub(super) fn check_object_bytes(
        &self,
        bytes: usize,
        deadline: WorkerDeadline,
    ) -> Result<(), WorkerError> {
        if bytes as u64 > self.limits.max_object_bytes.get() {
            return Err(WorkerError::InputLimitExceeded);
        }

        self.check_deadline(deadline)
    }

    pub(super) fn size_payloads_for_batches(
        &self,
        request: &ExecuteIndexJobRequest,
        target: &FactTarget,
        shard: &FactShardId,
        payloads: Vec<FactBatchPayload>,
        deadline: WorkerDeadline,
    ) -> Result<Vec<FactBatchPayload>, WorkerError> {
        let mut pending = VecDeque::from(payloads);
        let mut sized = Vec::new();
        while let Some(payload) = pending.pop_front() {
            self.check_deadline(deadline)?;
            let sequence =
                u32::try_from(sized.len()).map_err(|_| WorkerError::OutputLimitExceeded)?;
            let final_batch = pending.is_empty();
            let candidate = FactBatch::new(
                request.job.job.clone(),
                target.clone(),
                shard.clone(),
                sequence,
                request.job.spec.semantics.extractor_set_digest.clone(),
                final_batch,
                payload.clone(),
            )
            .map_err(|_| WorkerError::FactConflict)?;
            let byte_len = u64::try_from(
                serde_json::to_vec(&candidate)
                    .map_err(|_| WorkerError::FactConflict)?
                    .len(),
            )
            .map_err(|_| WorkerError::OutputLimitExceeded)?;
            if byte_len <= self.limits.max_batch_bytes.get() {
                sized.push(payload);
                continue;
            }

            let (first, second) = split_payload(payload)?;
            pending.push_front(second);
            pending.push_front(first);
        }

        Ok(sized)
    }

    pub(super) fn check_deadline(&self, deadline: WorkerDeadline) -> Result<(), WorkerError> {
        self.remaining_job_duration_ms(deadline).map(|_| ())
    }

    pub(super) fn remaining_job_duration_ms(
        &self,
        deadline: WorkerDeadline,
    ) -> Result<u64, WorkerError> {
        #[cfg(test)]
        if let Some(elapsed_ms) = &self.elapsed_ms {
            return remaining_duration_ms(
                deadline.limit_ms,
                elapsed_ms.load(std::sync::atomic::Ordering::Relaxed),
            );
        }

        let elapsed_ms = u64::try_from(deadline.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        remaining_duration_ms(deadline.limit_ms, elapsed_ms)
    }
}
