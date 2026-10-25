//! Stateless, resource-bounded extraction workers for immutable remote input.

use std::{
    collections::{BTreeMap, VecDeque},
    num::{NonZeroU32, NonZeroU64},
    time::{Duration, Instant},
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
        links::{
            RepositoryLinkSnapshot, RepositoryLinkSnapshotSet, SnapshotLinkResolutionError,
            resolve_repository_links_for_snapshot,
        },
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
    coordinator::{BoundedJobInspection, IndexJobCoordinator},
    fact_store::{
        FactBatchProgress, FactBatchProgressOutcome, FactBatchStore, FactBatchWriteAuthority,
        PutFactBatchOutcome,
    },
    identity::{
        FactShardId, IndexJobFailureCode, RemoteGraphSnapshotRef, RemoteMemoryRevisionRef,
        RequestId, TenantObjectRef, WorkerId,
    },
    object_store::{BoundedObjectRead, TenantObjectStore},
    protocol::{
        FactBatch, FactBatchPayload, FactTarget, IndexInputRef, IndexJobRecord, IndexJobState,
        InspectIndexJobRequest, RemoteMemoryLinkSetTarget,
    },
    publication::StoredRemoteGraphSnapshot,
    source::{
        MemorySourceManifest, MemorySourceManifestBody, RepositorySourceManifest,
        RepositorySourceManifestBody, verify_sanitized_memory_source,
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
        match self {
            Self::InvalidInput => {
                const { IndexJobFailureCode::from_static("worker.invalid_input") }
            }
            Self::InvalidSandbox => {
                const { IndexJobFailureCode::from_static("worker.invalid_sandbox") }
            }
            Self::AuthorityLost => {
                const { IndexJobFailureCode::from_static("worker.authority_lost") }
            }
            Self::SourceUnavailable => {
                const { IndexJobFailureCode::from_static("worker.source_unavailable") }
            }
            Self::IncompatibleSemantics => {
                const { IndexJobFailureCode::from_static("worker.incompatible_semantics") }
            }
            Self::InputLimitExceeded => {
                const { IndexJobFailureCode::from_static("worker.input_limit") }
            }
            Self::ExtractionFailed => {
                const { IndexJobFailureCode::from_static("worker.extraction_failed") }
            }
            Self::FactConflict => {
                const { IndexJobFailureCode::from_static("worker.fact_conflict") }
            }
            Self::OutputLimitExceeded => {
                const { IndexJobFailureCode::from_static("worker.output_limit") }
            }
            Self::FactStore => const { IndexJobFailureCode::from_static("worker.fact_store") },
            Self::DeadlineExceeded => const { IndexJobFailureCode::from_static("worker.deadline") },
        }
    }
}

pub struct StatelessIndexWorker {
    limits: WorkerLimits,
    #[cfg(test)]
    elapsed_ms: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
}

#[derive(Clone, Copy)]
struct WorkerDeadline {
    started: Instant,
    limit_ms: u64,
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

#[derive(Clone, Copy)]
struct MemoryRepositorySnapshots<'a> {
    current: Option<&'a StoredRemoteGraphSnapshot>,
    origins: &'a [StoredRemoteGraphSnapshot],
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

fn extraction_context_for_attempt(
    context: &ExtractionContext,
    max_diagnostics: u64,
    remaining_job_duration_ms: u64,
) -> ExtractionContext {
    ExtractionContext {
        max_diagnostics,
        max_parser_duration_ms: context
            .max_parser_duration_ms
            .min(remaining_job_duration_ms),
        ..context.clone()
    }
}

fn remaining_duration_ms(limit_ms: u64, elapsed_ms: u64) -> Result<u64, WorkerError> {
    limit_ms
        .checked_sub(elapsed_ms)
        .filter(|remaining| *remaining > 0)
        .ok_or(WorkerError::DeadlineExceeded)
}

fn effective_worker_duration_ms(
    local_limit_ms: u64,
    now: chrono::DateTime<Utc>,
    durable_deadline: chrono::DateTime<Utc>,
) -> Result<u64, WorkerError> {
    let durable_remaining_ms = u64::try_from(
        durable_deadline
            .signed_duration_since(now)
            .num_milliseconds(),
    )
    .map_err(|_| WorkerError::DeadlineExceeded)?;
    let effective = local_limit_ms.min(durable_remaining_ms);
    (effective > 0)
        .then_some(effective)
        .ok_or(WorkerError::DeadlineExceeded)
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
        if extractor_set_digest(&identities).ok()? != *requested {
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

    fn finish(mut self, context: &ExtractionContext) -> Result<GraphFragment, WorkerError> {
        self.diagnostics = crate::json_size::sort_by_json_key(self.diagnostics, serde_json::to_vec)
            .map_err(|_| WorkerError::ExtractionFailed)?;
        self.diagnostics.dedup();
        self.diagnostics
            .truncate(usize::try_from(context.max_diagnostics).unwrap_or(usize::MAX));
        Ok(GraphFragment {
            nodes: self.nodes.into_values().collect(),
            edges: self.edges.into_values().collect(),
            diagnostics: self.diagnostics,
        })
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
    fragment.diagnostics =
        crate::json_size::sort_by_json_key(fragment.diagnostics, serde_json::to_vec)
            .map_err(|_| WorkerError::ExtractionFailed)?;
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
    diagnostics = crate::json_size::sort_by_json_key(diagnostics, serde_json::to_vec)
        .map_err(|_| WorkerError::ExtractionFailed)?;
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

fn split_payload(
    payload: FactBatchPayload,
) -> Result<(FactBatchPayload, FactBatchPayload), WorkerError> {
    let count = usize::try_from(payload_fact_count(&payload))
        .map_err(|_| WorkerError::OutputLimitExceeded)?;
    if count < 2 {
        return Err(WorkerError::OutputLimitExceeded);
    }

    let mut remaining = count / 2;
    let split = match payload {
        FactBatchPayload::RepositoryGraph {
            mut nodes,
            mut edges,
            mut diagnostics,
        } => {
            let first = FactBatchPayload::RepositoryGraph {
                nodes: take_prefix(&mut nodes, &mut remaining),
                edges: take_prefix(&mut edges, &mut remaining),
                diagnostics: take_prefix(&mut diagnostics, &mut remaining),
            };
            let second = FactBatchPayload::RepositoryGraph {
                nodes,
                edges,
                diagnostics,
            };
            (first, second)
        }
        FactBatchPayload::ProjectMemory {
            mut entities,
            mut relationships,
            mut diagnostics,
        } => {
            let first = FactBatchPayload::ProjectMemory {
                entities: take_prefix(&mut entities, &mut remaining),
                relationships: take_prefix(&mut relationships, &mut remaining),
                diagnostics: take_prefix(&mut diagnostics, &mut remaining),
            };
            let second = FactBatchPayload::ProjectMemory {
                entities,
                relationships,
                diagnostics,
            };
            (first, second)
        }
    };
    Ok(split)
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

mod execution;

#[cfg(test)]
#[path = "worker_tests.rs"]
mod tests;
