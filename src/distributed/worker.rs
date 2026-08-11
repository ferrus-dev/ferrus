//! Stateless, resource-bounded extraction workers for immutable remote input.

use std::{
    collections::BTreeMap,
    num::{NonZeroU32, NonZeroU64},
    time::Instant,
};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    project_memory::{
        MEMORY_MODEL_VERSION,
        diagnostics::{MemoryDiagnostic, MemoryDiagnosticSeverity},
        domain::{
            AuthorizedSourceDescriptor, AuthorizedSourceManifest, MemoryBuildId, MemoryEntity,
            MemoryEntityId, MemoryFragment, MemoryRelationship, MemoryRelationshipId,
            MemoryRevision,
        },
        extractors::{built_in_extractor_set_digest, built_in_extractors},
        ports::{MemoryExtractionContext, MemoryExtractionInput},
    },
    repository_graph::{
        GRAPH_MODEL_VERSION,
        domain::{BuildId, DiagnosticSeverity, GraphDiagnostic, GraphEdge, GraphNode},
        extractors::{
            builtin_extractor_identities, cargo::CargoExtractor, generic::GenericExtractor,
            rust::RustSyntaxExtractor,
        },
        index::snapshot_identity,
        ports::{
            CrossFileResolutionInput, CrossFileResolver, DynExtractor, ExtractionContext,
            FileExtractionInput, GraphFragment, ResolutionBudget, SourceDiagnostic,
            SourceDiscoveryMetrics, SourceFileDescriptor, SourceManifest,
        },
        resolution::ConservativeResolver,
        source::extractor_set_digest,
    },
};

use super::{
    DISTRIBUTED_CONTROL_PROTOCOL_VERSION, DISTRIBUTED_WORKER_PROTOCOL_VERSION,
    coordinator::IndexJobCoordinator,
    fact_store::{FactBatchProgress, FactBatchStore, PutFactBatchOutcome},
    identity::{
        FactShardId, IndexJobFailureCode, RemoteGraphSnapshotRef, RemoteMemoryRevisionRef,
        RequestId, WorkerId,
    },
    object_store::TenantObjectStore,
    protocol::{
        FactBatch, FactBatchPayload, FactTarget, IndexInputRef, IndexJobRecord, IndexJobState,
        InspectIndexJobRequest,
    },
    source::{
        MemorySourceManifest, MemorySourceManifestBody, RepositorySourceManifest,
        RepositorySourceManifestBody,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryExecutionPolicy {
    Denied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerEgressPolicy {
    ControlAndObjectStoreOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerFilesystemPolicy {
    EphemeralNoHostMounts,
}

/// Controls that the process/container adapter must enforce before calling the
/// pure worker. There is intentionally no permissive policy variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerSandbox {
    pub repository_execution: RepositoryExecutionPolicy,
    pub egress: WorkerEgressPolicy,
    pub filesystem: WorkerFilesystemPolicy,
    pub memory_limit_bytes: NonZeroU64,
    pub cpu_time_limit_ms: NonZeroU64,
    pub max_concurrency: NonZeroU32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerLimits {
    pub max_input_objects: NonZeroU64,
    pub max_input_bytes: NonZeroU64,
    pub max_object_bytes: NonZeroU64,
    pub max_facts_per_source: NonZeroU64,
    pub max_total_facts: NonZeroU64,
    pub max_diagnostics: NonZeroU64,
    pub max_parser_duration_ms: NonZeroU64,
    pub max_resolver_duration_ms: NonZeroU64,
    pub max_job_duration_ms: NonZeroU64,
    pub max_facts_per_batch: NonZeroU64,
    pub max_batch_bytes: NonZeroU64,
    pub max_output_bytes: NonZeroU64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecuteIndexJobRequest {
    pub protocol_version: u32,
    pub job: IndexJobRecord,
    pub worker_id: WorkerId,
    pub lease_generation: NonZeroU64,
    pub sandbox: WorkerSandbox,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerExecutionOutcome {
    pub target: FactTarget,
    pub progress: FactBatchProgress,
    pub stored_batches: u64,
    pub reused_batches: u64,
    pub emitted_facts: u64,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum WorkerError {
    #[error("worker request or immutable manifest is invalid")]
    InvalidInput,
    #[error("worker sandbox contract is incompatible")]
    InvalidSandbox,
    #[error("worker no longer holds authority for this job")]
    AuthorityLost,
    #[error("tenant source object is unavailable or failed verification")]
    SourceUnavailable,
    #[error("worker semantic or extractor version is incompatible")]
    IncompatibleSemantics,
    #[error("worker input budget exceeded")]
    InputLimitExceeded,
    #[error("worker parser or extraction failed")]
    ExtractionFailed,
    #[error("worker fact identities conflict")]
    FactConflict,
    #[error("worker output budget exceeded")]
    OutputLimitExceeded,
    #[error("durable fact progress could not be recorded")]
    FactStore,
    #[error("worker total duration budget exceeded")]
    DeadlineExceeded,
}

impl WorkerError {
    pub fn failure_code(self) -> IndexJobFailureCode {
        let code = match self {
            Self::InvalidInput => "worker.invalid_input",
            Self::InvalidSandbox => "worker.invalid_sandbox",
            Self::AuthorityLost => "worker.authority_lost",
            Self::SourceUnavailable => "worker.source_unavailable",
            Self::IncompatibleSemantics => "worker.incompatible_semantics",
            Self::InputLimitExceeded => "worker.input_limit",
            Self::ExtractionFailed => "worker.extraction_failed",
            Self::FactConflict => "worker.fact_conflict",
            Self::OutputLimitExceeded => "worker.output_limit",
            Self::FactStore => "worker.fact_store",
            Self::DeadlineExceeded => "worker.deadline",
        };
        IndexJobFailureCode::new(code).expect("static worker failure code is canonical")
    }
}

pub struct StatelessIndexWorker {
    limits: WorkerLimits,
}

struct MemoryExtractionOutput {
    target: FactTarget,
    entities: Vec<MemoryEntity>,
    relationships: Vec<MemoryRelationship>,
    diagnostics: Vec<MemoryDiagnostic>,
}

struct RepositoryExtractorSelection {
    file_ids: std::collections::BTreeSet<String>,
    generic_enabled: bool,
    resolver_enabled: bool,
}

impl StatelessIndexWorker {
    pub fn new(limits: WorkerLimits) -> Self {
        Self { limits }
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
        let started = Instant::now();
        self.validate_request(request)?;
        let object_protection = objects.protection();
        if !object_protection.authenticated_transport || !object_protection.encrypted_at_rest {
            return Err(WorkerError::InvalidSandbox);
        }
        require_fact_protection(facts)?;
        self.authorize(request, coordinator)?;

        let (target, payloads) = match &request.job.spec.input {
            IndexInputRef::Repository(reference) => {
                let bytes = objects
                    .read_verified(&reference.manifest_object)
                    .map_err(|_| WorkerError::SourceUnavailable)?;
                self.check_object_bytes(bytes.len(), started)?;
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
                    self.extract_repository(request, coordinator, objects, &manifest, started)?;
                (
                    target,
                    graph_payloads(fragment, self.limits.max_facts_per_batch)?,
                )
            }
            IndexInputRef::Memory(reference) => {
                let bytes = objects
                    .read_verified(&reference.manifest_object)
                    .map_err(|_| WorkerError::SourceUnavailable)?;
                self.check_object_bytes(bytes.len(), started)?;
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
                let extracted =
                    self.extract_memory(request, coordinator, objects, &manifest, started)?;
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

        let emitted_facts = payloads
            .iter()
            .map(payload_fact_count)
            .try_fold(0u64, |total, count| total.checked_add(count))
            .ok_or(WorkerError::OutputLimitExceeded)?;
        if emitted_facts > self.limits.max_total_facts.get() {
            return Err(WorkerError::OutputLimitExceeded);
        }

        let shard = FactShardId::new(match request.job.job.kind {
            super::protocol::IndexJobKind::RepositoryGraph => "repository-all",
            super::protocol::IndexJobKind::ProjectMemory => "memory-all",
        })
        .map_err(|_| WorkerError::InvalidInput)?;
        let mut stored_batches = 0u64;
        let mut reused_batches = 0u64;
        let mut output_bytes = 0u64;
        let last = payloads.len().saturating_sub(1);
        for (index, payload) in payloads.into_iter().enumerate() {
            self.check_deadline(started)?;
            self.authorize(request, coordinator)?;
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
            match facts.put(&batch).map_err(|_| WorkerError::FactStore)? {
                PutFactBatchOutcome::Stored => stored_batches += 1,
                PutFactBatchOutcome::Reused => reused_batches += 1,
            }
        }
        let progress = facts
            .progress(&request.job.job)
            .map_err(|_| WorkerError::FactStore)?;
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

    fn validate_request(&self, request: &ExecuteIndexJobRequest) -> Result<(), WorkerError> {
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

    fn authorize<C: IndexJobCoordinator>(
        &self,
        request: &ExecuteIndexJobRequest,
        coordinator: &C,
    ) -> Result<(), WorkerError> {
        let authorization = InspectIndexJobRequest {
            protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            request_id: RequestId::new(format!("worker-auth-{}", request.job.job.job_id))
                .map_err(|_| WorkerError::InvalidInput)?,
            job: request.job.job.clone(),
        };
        let current = coordinator
            .inspect(&authorization)
            .map_err(|_| WorkerError::AuthorityLost)?
            .ok_or(WorkerError::AuthorityLost)?;
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

    fn extract_repository<C, O>(
        &self,
        request: &ExecuteIndexJobRequest,
        coordinator: &C,
        objects: &O,
        remote: &RepositorySourceManifest,
        started: Instant,
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
        let snapshot_id = snapshot_identity(&manifest);
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
            let extraction_context = extraction_context_with_diagnostic_limit(
                &context,
                remaining_diagnostics(&self.limits, extracted_diagnostics),
            );
            let fragment =
                GenericExtractor::new().repository_fragment(&extraction_context, &manifest);
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
        let cargo = CargoExtractor::new();
        let rust = RustSyntaxExtractor::new();
        let extractors: [&dyn DynExtractor; 3] = [&generic, &cargo, &rust];
        for (remote_file, file) in remote.body.files.iter().zip(&manifest.files) {
            let mut source_facts = 0u64;
            self.check_deadline(started)?;
            self.authorize(request, coordinator)?;
            if remote_file.byte_len > self.limits.max_object_bytes.get() {
                return Err(WorkerError::InputLimitExceeded);
            }
            let content = objects
                .read_verified(&remote_file.object)
                .map_err(|_| WorkerError::SourceUnavailable)?;
            if content.len() as u64 != file.byte_len {
                return Err(WorkerError::SourceUnavailable);
            }
            for extractor in extractors
                .iter()
                .copied()
                .filter(|item| active.file_ids.contains(item.identity().id.as_str()))
                .filter(|item| item.supports(file))
            {
                let extraction_context = extraction_context_with_diagnostic_limit(
                    &context,
                    remaining_diagnostics(&self.limits, extracted_diagnostics),
                );
                let fragment = extractor
                    .extract(FileExtractionInput {
                        context: &extraction_context,
                        file,
                        content: &content,
                    })
                    .map_err(|_| WorkerError::ExtractionFailed)?;
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
        let unresolved = merger.finish(&context);
        if graph_fragment_fact_count(&unresolved)? > self.limits.max_total_facts.get() {
            return Err(WorkerError::OutputLimitExceeded);
        }
        let graph = if active.resolver_enabled {
            ConservativeResolver::new()
                .resolve(CrossFileResolutionInput {
                    context: &context,
                    manifest: &manifest,
                    fragment: unresolved,
                    budget: ResolutionBudget {
                        max_relationships: self.limits.max_total_facts.get(),
                        max_duration_ms: self.limits.max_resolver_duration_ms.get(),
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
        self.check_deadline(started)?;
        Ok((
            FactTarget::RepositoryGraph {
                snapshot: RemoteGraphSnapshotRef {
                    repository: remote.body.repository.clone(),
                    snapshot_id,
                },
                build_id,
            },
            graph,
        ))
    }

    fn extract_memory<C, O>(
        &self,
        request: &ExecuteIndexJobRequest,
        coordinator: &C,
        objects: &O,
        remote: &MemorySourceManifest,
        started: Instant,
    ) -> Result<MemoryExtractionOutput, WorkerError>
    where
        C: IndexJobCoordinator,
        O: TenantObjectStore,
    {
        let semantics = &request.job.spec.semantics;
        if remote.body.extractor_set_digest != semantics.extractor_set_digest
            || semantics.extractor_set_digest != built_in_extractor_set_digest()
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
            self.check_deadline(started)?;
            self.authorize(request, coordinator)?;
            if remote_source.sanitized_byte_len > self.limits.max_object_bytes.get() {
                return Err(WorkerError::InputLimitExceeded);
            }
            let content = objects
                .read_verified(&remote_source.object)
                .map_err(|_| WorkerError::SourceUnavailable)?;
            if content.len() as u64 != remote_source.sanitized_byte_len {
                return Err(WorkerError::SourceUnavailable);
            }
            let Some(extractor) = extractors
                .iter()
                .find(|extractor| extractor.supports(source.category))
            else {
                return Err(WorkerError::IncompatibleSemantics);
            };
            match extractor.extract(MemoryExtractionInput {
                context: &context,
                source,
                content: &content,
            }) {
                Ok(fragment) => {
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
        self.check_deadline(started)?;
        Ok(MemoryExtractionOutput {
            target: FactTarget::ProjectMemory {
                revision: RemoteMemoryRevisionRef {
                    project: remote.body.project.clone(),
                    revision_id: revision.id,
                },
                build_id,
            },
            entities: entities.into_values().collect(),
            relationships: relationships.into_values().collect(),
            diagnostics,
        })
    }

    fn check_manifest_limits(
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

    fn check_object_bytes(&self, bytes: usize, started: Instant) -> Result<(), WorkerError> {
        if bytes as u64 > self.limits.max_object_bytes.get() {
            return Err(WorkerError::InputLimitExceeded);
        }
        self.check_deadline(started)
    }

    fn check_deadline(&self, started: Instant) -> Result<(), WorkerError> {
        if started.elapsed().as_millis() as u64 > self.limits.max_job_duration_ms.get() {
            return Err(WorkerError::DeadlineExceeded);
        }
        Ok(())
    }
}

fn graph_fragment_fact_count(fragment: &GraphFragment) -> Result<u64, WorkerError> {
    fragment
        .nodes
        .len()
        .checked_add(fragment.edges.len())
        .and_then(|count| count.checked_add(fragment.diagnostics.len()))
        .and_then(|count| u64::try_from(count).ok())
        .ok_or(WorkerError::OutputLimitExceeded)
}

fn memory_fragment_fact_count(fragment: &MemoryFragment) -> Result<u64, WorkerError> {
    fragment
        .entities
        .len()
        .checked_add(fragment.relationships.len())
        .and_then(|count| u64::try_from(count).ok())
        .ok_or(WorkerError::OutputLimitExceeded)
}

fn reserve_extracted_facts(
    total: &mut u64,
    additional: u64,
    limit: u64,
) -> Result<(), WorkerError> {
    let next = total
        .checked_add(additional)
        .ok_or(WorkerError::OutputLimitExceeded)?;
    if next > limit {
        return Err(WorkerError::OutputLimitExceeded);
    }
    *total = next;
    Ok(())
}

fn extraction_context_with_diagnostic_limit(
    context: &ExtractionContext,
    max_diagnostics: u64,
) -> ExtractionContext {
    ExtractionContext {
        max_diagnostics,
        ..context.clone()
    }
}

fn remaining_diagnostics(limits: &WorkerLimits, used: u64) -> u64 {
    limits.max_diagnostics.get().saturating_sub(used)
}

fn reserve_extracted_diagnostics(
    total: &mut u64,
    additional: usize,
    limit: u64,
) -> Result<(), WorkerError> {
    let additional = u64::try_from(additional).map_err(|_| WorkerError::OutputLimitExceeded)?;
    reserve_extracted_facts(total, additional, limit)
}

fn require_fact_protection<F: FactBatchStore>(facts: &F) -> Result<(), WorkerError> {
    let protection = facts.protection();
    if !protection.authenticated_transport || !protection.encrypted_at_rest {
        return Err(WorkerError::InvalidSandbox);
    }
    Ok(())
}

fn repository_extractor_selection(
    requested: &crate::repository_graph::domain::Digest,
) -> Option<RepositoryExtractorSelection> {
    let builtins = builtin_extractor_identities();
    let variants = 1usize.checked_shl(u32::try_from(builtins.len()).ok()?)?;
    for mask in 1usize..variants {
        let identities = builtins
            .iter()
            .enumerate()
            .filter(|(index, _)| mask & (1usize << index) != 0)
            .map(|(_, identity)| identity.clone())
            .collect::<Vec<_>>();
        if extractor_set_digest(&identities) != *requested {
            continue;
        }
        let file_ids = identities
            .iter()
            .filter(|identity| identity.id.as_str() != "builtin.rust-cargo-resolver")
            .map(|identity| identity.id.as_str().to_string())
            .collect::<std::collections::BTreeSet<_>>();
        return Some(RepositoryExtractorSelection {
            generic_enabled: file_ids.contains("builtin.generic-structure"),
            resolver_enabled: identities
                .iter()
                .any(|identity| identity.id.as_str() == "builtin.rust-cargo-resolver"),
            file_ids,
        });
    }
    None
}

#[derive(Default)]
struct GraphMerger {
    nodes: BTreeMap<crate::repository_graph::domain::NodeId, GraphNode>,
    edges: BTreeMap<crate::repository_graph::domain::EdgeId, GraphEdge>,
    diagnostics: Vec<GraphDiagnostic>,
}

impl GraphMerger {
    fn merge(&mut self, fragment: GraphFragment) -> Result<(), WorkerError> {
        for node in fragment.nodes {
            if self
                .nodes
                .get(&node.id)
                .is_some_and(|existing| existing != &node)
            {
                return Err(WorkerError::FactConflict);
            }
            self.nodes.insert(node.id.clone(), node);
        }
        for edge in fragment.edges {
            if self
                .edges
                .get(&edge.id)
                .is_some_and(|existing| existing != &edge)
            {
                return Err(WorkerError::FactConflict);
            }
            self.edges.insert(edge.id.clone(), edge);
        }
        self.diagnostics.extend(fragment.diagnostics);
        Ok(())
    }

    fn finish(mut self, context: &ExtractionContext) -> GraphFragment {
        self.diagnostics.sort_by(|left, right| {
            serde_json::to_vec(left)
                .expect("graph diagnostics serialize")
                .cmp(&serde_json::to_vec(right).expect("graph diagnostics serialize"))
        });
        self.diagnostics.dedup();
        self.diagnostics
            .truncate(usize::try_from(context.max_diagnostics).unwrap_or(usize::MAX));
        GraphFragment {
            nodes: self.nodes.into_values().collect(),
            edges: self.edges.into_values().collect(),
            diagnostics: self.diagnostics,
        }
    }
}

fn merge_memory_fragment(
    entities: &mut BTreeMap<MemoryEntityId, MemoryEntity>,
    relationships: &mut BTreeMap<MemoryRelationshipId, MemoryRelationship>,
    fragment: MemoryFragment,
) -> Result<(), WorkerError> {
    for entity in fragment.entities {
        if entities
            .get(&entity.id)
            .is_some_and(|existing| existing != &entity)
        {
            return Err(WorkerError::FactConflict);
        }
        entities.insert(entity.id.clone(), entity);
    }
    for relationship in fragment.relationships {
        if relationships
            .get(&relationship.id)
            .is_some_and(|existing| existing != &relationship)
        {
            return Err(WorkerError::FactConflict);
        }
        relationships.insert(relationship.id.clone(), relationship);
    }
    Ok(())
}

fn graph_payloads(
    mut fragment: GraphFragment,
    max_facts: NonZeroU64,
) -> Result<Vec<FactBatchPayload>, WorkerError> {
    fragment.nodes.sort_by(|left, right| left.id.cmp(&right.id));
    fragment.edges.sort_by(|left, right| left.id.cmp(&right.id));
    fragment.diagnostics.sort_by(|left, right| {
        serde_json::to_vec(left)
            .expect("graph diagnostics serialize")
            .cmp(&serde_json::to_vec(right).expect("graph diagnostics serialize"))
    });
    let limit = usize::try_from(max_facts.get()).unwrap_or(usize::MAX);
    let mut payloads = Vec::new();
    while !fragment.nodes.is_empty()
        || !fragment.edges.is_empty()
        || !fragment.diagnostics.is_empty()
    {
        let mut remaining = limit;
        let nodes = take_prefix(&mut fragment.nodes, &mut remaining);
        let edges = take_prefix(&mut fragment.edges, &mut remaining);
        let diagnostics = take_prefix(&mut fragment.diagnostics, &mut remaining);
        payloads.push(FactBatchPayload::RepositoryGraph {
            nodes,
            edges,
            diagnostics,
        });
    }
    if payloads.is_empty() {
        payloads.push(FactBatchPayload::RepositoryGraph {
            nodes: Vec::new(),
            edges: Vec::new(),
            diagnostics: Vec::new(),
        });
    }
    Ok(payloads)
}

fn memory_payloads(
    mut entities: Vec<MemoryEntity>,
    mut relationships: Vec<MemoryRelationship>,
    mut diagnostics: Vec<MemoryDiagnostic>,
    max_facts: NonZeroU64,
) -> Result<Vec<FactBatchPayload>, WorkerError> {
    entities.sort_by(|left, right| left.id.cmp(&right.id));
    relationships.sort_by(|left, right| left.id.cmp(&right.id));
    diagnostics.sort_by(|left, right| {
        serde_json::to_vec(left)
            .expect("memory diagnostics serialize")
            .cmp(&serde_json::to_vec(right).expect("memory diagnostics serialize"))
    });
    let limit = usize::try_from(max_facts.get()).unwrap_or(usize::MAX);
    let mut payloads = Vec::new();
    while !entities.is_empty() || !relationships.is_empty() || !diagnostics.is_empty() {
        let mut remaining = limit;
        let entities = take_prefix(&mut entities, &mut remaining);
        let relationships = take_prefix(&mut relationships, &mut remaining);
        let diagnostics = take_prefix(&mut diagnostics, &mut remaining);
        payloads.push(FactBatchPayload::ProjectMemory {
            entities,
            relationships,
            diagnostics,
        });
    }
    if payloads.is_empty() {
        payloads.push(FactBatchPayload::ProjectMemory {
            entities: Vec::new(),
            relationships: Vec::new(),
            diagnostics: Vec::new(),
        });
    }
    Ok(payloads)
}

fn take_prefix<T>(items: &mut Vec<T>, remaining: &mut usize) -> Vec<T> {
    let count = items.len().min(*remaining);
    *remaining -= count;
    items.drain(..count).collect()
}

fn payload_fact_count(payload: &FactBatchPayload) -> u64 {
    match payload {
        FactBatchPayload::RepositoryGraph {
            nodes,
            edges,
            diagnostics,
        } => (nodes.len() + edges.len() + diagnostics.len()) as u64,
        FactBatchPayload::ProjectMemory {
            entities,
            relationships,
            diagnostics,
        } => (entities.len() + relationships.len() + diagnostics.len()) as u64,
    }
}

fn checked_sum(values: impl IntoIterator<Item = u64>) -> Result<u64, WorkerError> {
    values
        .into_iter()
        .try_fold(0u64, |total, value| total.checked_add(value))
        .ok_or(WorkerError::InputLimitExceeded)
}

fn progress_is_complete(progress: &FactBatchProgress) -> bool {
    !progress.batches.is_empty()
        && progress.batches.iter().enumerate().all(|(index, batch)| {
            usize::try_from(batch.sequence).ok() == Some(index)
                && batch.final_batch == (index + 1 == progress.batches.len())
                && batch.shard_id == progress.batches[0].shard_id
        })
}

#[cfg(test)]
#[path = "worker_tests.rs"]
mod tests;
