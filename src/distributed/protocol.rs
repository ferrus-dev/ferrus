//! Versioned control, fact, publication, and query envelopes.

use std::num::{NonZeroU32, NonZeroU64};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::{
    project_memory::{
        diagnostics::MemoryDiagnostic,
        domain::{
            MemoryBuildId, MemoryEntity, MemoryRelationship, MemoryRepositoryLinkSet,
            MemoryRevisionId, MemoryViewName,
        },
    },
    repository_graph::domain::{
        BuildId, Digest, GraphDiagnostic, GraphEdge, GraphNode, PublishedViewName, SnapshotId,
    },
};

use super::{
    DISTRIBUTED_CONTROL_PROTOCOL_VERSION, DISTRIBUTED_FACT_PROTOCOL_VERSION,
    DISTRIBUTED_QUERY_PROTOCOL_VERSION,
    identity::{
        FactBatchId, FactShardId, FederatedViewRef, IndexJobFailureCode, IndexJobId,
        MemoryManifestRef, RemoteGraphSnapshotRef, RemoteMemoryRevisionRef, RemoteProjectRef,
        RemoteRepositoryRef, RepositoryManifestRef, RequestId, WorkerId,
    },
};

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DistributedProtocolError {
    #[error("unsupported distributed protocol version")]
    UnsupportedVersion,
    #[error("index job kind does not match its immutable input")]
    JobInputMismatch,
    #[error("index job idempotency key does not match its semantic inputs")]
    IdempotencyMismatch,
    #[error("index job state transition is invalid")]
    InvalidJobTransition,
    #[error("fact batch kind, target, or fact identity is inconsistent")]
    FactBatchMismatch,
    #[error("fact batch content digest or deterministic identity is invalid")]
    FactBatchIdentityMismatch,
    #[error("publication request scope, job kind, or expected version is inconsistent")]
    PublicationMismatch,
    #[error("query target does not belong to the request project")]
    QueryScopeMismatch,
    #[error("deletion scope, coverage, or idempotency identity is inconsistent")]
    DeletionMismatch,
    #[error("distributed contract serialization failed")]
    Serialization,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexJobKind {
    RepositoryGraph,
    ProjectMemory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum IndexInputRef {
    Repository(RepositoryManifestRef),
    Memory(MemoryManifestRef),
}

impl IndexInputRef {
    pub fn project(&self) -> &RemoteProjectRef {
        match self {
            Self::Repository(manifest) => &manifest.repository.project,
            Self::Memory(manifest) => &manifest.project,
        }
    }

    pub fn kind(&self) -> IndexJobKind {
        match self {
            Self::Repository(_) => IndexJobKind::RepositoryGraph,
            Self::Memory(_) => IndexJobKind::ProjectMemory,
        }
    }

    fn manifest_digest(&self) -> &Digest {
        match self {
            Self::Repository(manifest) => &manifest.manifest_digest,
            Self::Memory(manifest) => &manifest.manifest_digest,
        }
    }

    fn policy_digest(&self) -> &Digest {
        match self {
            Self::Repository(manifest) => &manifest.source_policy_digest,
            Self::Memory(manifest) => &manifest.memory_policy_digest,
        }
    }

    fn expected_target_identity(&self) -> &str {
        match self {
            Self::Repository(manifest) => manifest.expected_snapshot_id.as_str(),
            Self::Memory(manifest) => manifest.expected_revision_id.as_str(),
        }
    }

    fn repository_snapshot(&self) -> Option<&RemoteGraphSnapshotRef> {
        match self {
            Self::Repository(_) => None,
            Self::Memory(manifest) => manifest.repository_snapshot.as_ref(),
        }
    }

    fn repository_origin_snapshots(&self) -> &[RemoteGraphSnapshotRef] {
        match self {
            Self::Repository(_) => &[],
            Self::Memory(manifest) => &manifest.repository_origin_snapshots,
        }
    }

    fn repository_identity(&self) -> Option<&crate::repository_graph::domain::RepositoryRef> {
        match self {
            Self::Repository(manifest) => Some(&manifest.repository_identity),
            Self::Memory(_) => None,
        }
    }

    fn project_identity(&self) -> Option<&crate::project_memory::domain::ProjectRef> {
        match self {
            Self::Repository(_) => None,
            Self::Memory(manifest) => Some(&manifest.project_identity),
        }
    }

    fn validate(&self) -> Result<(), DistributedProtocolError> {
        let result = match self {
            Self::Repository(manifest) => manifest.validate(),
            Self::Memory(manifest) => manifest.validate(),
        };
        result.map_err(|_| DistributedProtocolError::JobInputMismatch)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexSemantics {
    pub semantic_config_digest: Digest,
    pub model_version: NonZeroU32,
    pub extractor_set_digest: Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexJobSpec {
    pub protocol_version: u32,
    pub kind: IndexJobKind,
    pub input: IndexInputRef,
    pub semantics: IndexSemantics,
    pub idempotency_key: Digest,
}

#[derive(Serialize)]
struct IdempotencyMaterial<'a> {
    protocol_version: u32,
    project: &'a RemoteProjectRef,
    kind: IndexJobKind,
    manifest_digest: &'a Digest,
    policy_digest: &'a Digest,
    expected_target_identity: &'a str,
    repository_snapshot: Option<&'a RemoteGraphSnapshotRef>,
    repository_origin_snapshots: &'a [RemoteGraphSnapshotRef],
    repository_identity: Option<&'a crate::repository_graph::domain::RepositoryRef>,
    project_identity: Option<&'a crate::project_memory::domain::ProjectRef>,
    semantics: &'a IndexSemantics,
}

impl IndexJobSpec {
    pub fn new(
        kind: IndexJobKind,
        input: IndexInputRef,
        semantics: IndexSemantics,
    ) -> Result<Self, DistributedProtocolError> {
        if kind != input.kind() || input.validate().is_err() {
            return Err(DistributedProtocolError::JobInputMismatch);
        }
        let idempotency_key = idempotency_key(kind, &input, &semantics)?;
        Ok(Self {
            protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            kind,
            input,
            semantics,
            idempotency_key,
        })
    }

    pub fn validate(&self) -> Result<(), DistributedProtocolError> {
        validate_control_version(self.protocol_version)?;
        self.input.validate()?;
        if self.kind != self.input.kind() {
            return Err(DistributedProtocolError::JobInputMismatch);
        }
        if self.idempotency_key != idempotency_key(self.kind, &self.input, &self.semantics)? {
            return Err(DistributedProtocolError::IdempotencyMismatch);
        }
        Ok(())
    }

    pub fn project(&self) -> &RemoteProjectRef {
        self.input.project()
    }
}

fn idempotency_key(
    kind: IndexJobKind,
    input: &IndexInputRef,
    semantics: &IndexSemantics,
) -> Result<Digest, DistributedProtocolError> {
    hash(
        b"ferrus.distributed.index-job.v1\0",
        &IdempotencyMaterial {
            protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            project: input.project(),
            kind,
            manifest_digest: input.manifest_digest(),
            policy_digest: input.policy_digest(),
            expected_target_identity: input.expected_target_identity(),
            repository_snapshot: input.repository_snapshot(),
            repository_origin_snapshots: input.repository_origin_snapshots(),
            repository_identity: input.repository_identity(),
            project_identity: input.project_identity(),
            semantics,
        },
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexJobState {
    Queued,
    Leased,
    Running,
    Publishing,
    Complete,
    Failed,
    Cancelled,
}

impl IndexJobState {
    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Queued, Self::Leased | Self::Cancelled | Self::Failed)
                | (
                    Self::Leased,
                    Self::Running | Self::Queued | Self::Cancelled | Self::Failed
                )
                | (
                    Self::Running,
                    Self::Publishing | Self::Queued | Self::Cancelled | Self::Failed
                )
                | (
                    Self::Publishing,
                    Self::Complete | Self::Cancelled | Self::Failed
                )
        )
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexJobRef {
    pub project: RemoteProjectRef,
    pub job_id: IndexJobId,
    pub kind: IndexJobKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobLease {
    pub worker_id: WorkerId,
    pub generation: NonZeroU64,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexJobRecord {
    pub job: IndexJobRef,
    pub spec: IndexJobSpec,
    pub state: IndexJobState,
    pub attempt: NonZeroU32,
    pub max_attempts: NonZeroU32,
    pub lease: Option<JobLease>,
    pub cancellation_requested: bool,
    pub failure_code: Option<IndexJobFailureCode>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deadline_at: DateTime<Utc>,
}

impl IndexJobRecord {
    pub fn validate(&self) -> Result<(), DistributedProtocolError> {
        self.spec.validate()?;
        if self.job.project != *self.spec.project()
            || self.job.kind != self.spec.kind
            || self.attempt > self.max_attempts
            || (matches!(
                self.state,
                IndexJobState::Leased | IndexJobState::Running | IndexJobState::Publishing
            ) != self.lease.is_some())
            || (self.cancellation_requested && self.state == IndexJobState::Complete)
            || (self.state == IndexJobState::Failed && self.failure_code.is_none())
            || (matches!(
                self.state,
                IndexJobState::Complete | IndexJobState::Cancelled
            ) && self.failure_code.is_some())
            || self.deadline_at < self.created_at
        {
            return Err(DistributedProtocolError::JobInputMismatch);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitIndexJobRequest {
    pub protocol_version: u32,
    pub request_id: RequestId,
    pub project: RemoteProjectRef,
    pub job: IndexJobSpec,
}

impl SubmitIndexJobRequest {
    pub fn validate(&self) -> Result<(), DistributedProtocolError> {
        validate_control_version(self.protocol_version)?;
        self.job.validate()?;
        if self.project != *self.job.project() {
            return Err(DistributedProtocolError::JobInputMismatch);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectIndexJobRequest {
    pub protocol_version: u32,
    pub request_id: RequestId,
    pub job: IndexJobRef,
}

impl InspectIndexJobRequest {
    pub fn validate(&self) -> Result<(), DistributedProtocolError> {
        validate_control_version(self.protocol_version)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelIndexJobRequest {
    pub protocol_version: u32,
    pub request_id: RequestId,
    pub job: IndexJobRef,
    pub expected_state: Option<IndexJobState>,
}

impl CancelIndexJobRequest {
    pub fn validate(&self) -> Result<(), DistributedProtocolError> {
        validate_control_version(self.protocol_version)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatJobRequest {
    pub protocol_version: u32,
    pub request_id: RequestId,
    pub job: IndexJobRef,
    pub worker_id: WorkerId,
    pub lease_generation: NonZeroU64,
}

impl HeartbeatJobRequest {
    pub fn validate(&self) -> Result<(), DistributedProtocolError> {
        validate_control_version(self.protocol_version)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum FactTarget {
    RepositoryGraph {
        snapshot: RemoteGraphSnapshotRef,
        repository_identity: crate::repository_graph::domain::RepositoryRef,
        build_id: BuildId,
    },
    ProjectMemory {
        revision: RemoteMemoryRevisionRef,
        project_identity: crate::project_memory::domain::ProjectRef,
        build_id: MemoryBuildId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repository_links: Option<Box<RemoteMemoryLinkSetTarget>>,
    },
}

/// Immutable repository-link resolution produced alongside a memory build.
/// The graph target is part of the job and fact-batch identity, but not the
/// semantic memory revision identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteMemoryLinkSetTarget {
    pub graph: RemoteGraphSnapshotRef,
    pub link_set: MemoryRepositoryLinkSet,
}

impl FactTarget {
    pub fn project(&self) -> &RemoteProjectRef {
        match self {
            Self::RepositoryGraph { snapshot, .. } => &snapshot.repository.project,
            Self::ProjectMemory { revision, .. } => &revision.project,
        }
    }

    pub fn kind(&self) -> IndexJobKind {
        match self {
            Self::RepositoryGraph { .. } => IndexJobKind::RepositoryGraph,
            Self::ProjectMemory { .. } => IndexJobKind::ProjectMemory,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum FactBatchPayload {
    RepositoryGraph {
        nodes: Vec<GraphNode>,
        edges: Vec<GraphEdge>,
        diagnostics: Vec<GraphDiagnostic>,
    },
    ProjectMemory {
        entities: Vec<MemoryEntity>,
        relationships: Vec<MemoryRelationship>,
        diagnostics: Vec<MemoryDiagnostic>,
    },
}

impl FactBatchPayload {
    fn kind(&self) -> IndexJobKind {
        match self {
            Self::RepositoryGraph { .. } => IndexJobKind::RepositoryGraph,
            Self::ProjectMemory { .. } => IndexJobKind::ProjectMemory,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactBatchHeader {
    pub protocol_version: u32,
    pub job: IndexJobRef,
    pub target: FactTarget,
    pub batch_id: FactBatchId,
    pub shard_id: FactShardId,
    pub sequence: u32,
    pub payload_digest: Digest,
    pub extractor_set_digest: Digest,
    pub final_batch: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactBatch {
    pub header: FactBatchHeader,
    pub payload: FactBatchPayload,
}

#[derive(Serialize)]
struct FactBatchIdentityMaterial<'a> {
    job: &'a IndexJobRef,
    target: &'a FactTarget,
    shard_id: &'a FactShardId,
    sequence: u32,
    payload_digest: &'a Digest,
    extractor_set_digest: &'a Digest,
}

impl FactBatch {
    pub fn new(
        job: IndexJobRef,
        target: FactTarget,
        shard_id: FactShardId,
        sequence: u32,
        extractor_set_digest: Digest,
        final_batch: bool,
        payload: FactBatchPayload,
    ) -> Result<Self, DistributedProtocolError> {
        let payload_digest = hash(b"ferrus.distributed.fact-payload.v1\0", &payload)?;
        let batch_id = fact_batch_id(
            &job,
            &target,
            &shard_id,
            sequence,
            &payload_digest,
            &extractor_set_digest,
        )?;
        let batch = Self {
            header: FactBatchHeader {
                protocol_version: DISTRIBUTED_FACT_PROTOCOL_VERSION,
                job,
                target,
                batch_id,
                shard_id,
                sequence,
                payload_digest,
                extractor_set_digest,
                final_batch,
            },
            payload,
        };
        batch.validate()?;
        Ok(batch)
    }

    pub fn validate(&self) -> Result<(), DistributedProtocolError> {
        if self.header.protocol_version != DISTRIBUTED_FACT_PROTOCOL_VERSION {
            return Err(DistributedProtocolError::UnsupportedVersion);
        }
        if self.header.job.kind != self.header.target.kind()
            || self.header.job.kind != self.payload.kind()
            || self.header.job.project != *self.header.target.project()
        {
            return Err(DistributedProtocolError::FactBatchMismatch);
        }
        validate_fact_targets(&self.header.target, &self.payload)?;
        let payload_digest = hash(b"ferrus.distributed.fact-payload.v1\0", &self.payload)?;
        let batch_id = fact_batch_id(
            &self.header.job,
            &self.header.target,
            &self.header.shard_id,
            self.header.sequence,
            &payload_digest,
            &self.header.extractor_set_digest,
        )?;
        if payload_digest != self.header.payload_digest || batch_id != self.header.batch_id {
            return Err(DistributedProtocolError::FactBatchIdentityMismatch);
        }
        Ok(())
    }
}

fn validate_fact_targets(
    target: &FactTarget,
    payload: &FactBatchPayload,
) -> Result<(), DistributedProtocolError> {
    let valid = match (target, payload) {
        (
            FactTarget::RepositoryGraph {
                snapshot,
                repository_identity: _,
                build_id,
            },
            FactBatchPayload::RepositoryGraph {
                nodes,
                edges,
                diagnostics,
            },
        ) => {
            nodes
                .iter()
                .all(|node| node.snapshot_id == snapshot.snapshot_id)
                && edges
                    .iter()
                    .all(|edge| edge.snapshot_id == snapshot.snapshot_id)
                && diagnostics.iter().all(|diagnostic| {
                    diagnostic.build_id == *build_id
                        && diagnostic
                            .snapshot_id
                            .as_ref()
                            .is_none_or(|id| id == &snapshot.snapshot_id)
                })
        }
        (
            FactTarget::ProjectMemory {
                revision,
                project_identity,
                build_id,
                repository_links,
            },
            FactBatchPayload::ProjectMemory {
                entities,
                relationships,
                diagnostics,
            },
        ) => {
            let link_target_is_valid = repository_links.as_ref().is_none_or(|links| {
                links.graph.repository.project == revision.project
                    && links.link_set.project == *project_identity
                    && links.link_set.memory_revision_id == revision.revision_id
                    && links.link_set.repository_snapshot_id.as_ref()
                        == Some(&links.graph.snapshot_id)
            });
            link_target_is_valid
                && entities.iter().all(|entity| {
                    entity.project == *project_identity
                        && entity.memory_revision_id == revision.revision_id
                })
                && relationships.iter().all(|relationship| {
                    relationship.project == *project_identity
                        && relationship.memory_revision_id == revision.revision_id
                        && memory_relationship_matches_link_target(
                            relationship,
                            repository_links.as_deref(),
                        )
                })
                && diagnostics.iter().all(|diagnostic| {
                    diagnostic.build_id == *build_id
                        && diagnostic.revision_id == revision.revision_id
                })
        }
        _ => false,
    };
    valid
        .then_some(())
        .ok_or(DistributedProtocolError::FactBatchMismatch)
}

fn memory_relationship_matches_link_target(
    relationship: &MemoryRelationship,
    target: Option<&RemoteMemoryLinkSetTarget>,
) -> bool {
    let (repository, snapshot_id) = match &relationship.target {
        crate::project_memory::domain::MemoryRelationshipTarget::RepositoryNode {
            repository,
            snapshot_id,
            ..
        } => (repository, Some(snapshot_id)),
        crate::project_memory::domain::MemoryRelationshipTarget::RepositoryPath {
            repository,
            snapshot_id,
            ..
        }
        | crate::project_memory::domain::MemoryRelationshipTarget::RepositorySymbol {
            repository,
            snapshot_id,
            ..
        } => (repository, snapshot_id.as_ref()),
        _ => return true,
    };
    target.is_some_and(|target| {
        repository == &target.link_set.repository
            && snapshot_id.is_none_or(|snapshot| snapshot == &target.graph.snapshot_id)
    })
}

fn fact_batch_id(
    job: &IndexJobRef,
    target: &FactTarget,
    shard_id: &FactShardId,
    sequence: u32,
    payload_digest: &Digest,
    extractor_set_digest: &Digest,
) -> Result<FactBatchId, DistributedProtocolError> {
    let digest = hash(
        b"ferrus.distributed.fact-batch.v1\0",
        &FactBatchIdentityMaterial {
            job,
            target,
            shard_id,
            sequence,
            payload_digest,
            extractor_set_digest,
        },
    )?;
    FactBatchId::new(digest.value()).map_err(|_| DistributedProtocolError::Serialization)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphPublicationVersion {
    pub snapshot_id: SnapshotId,
    pub generation: NonZeroU64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishGraphRequest {
    pub protocol_version: u32,
    pub request_id: RequestId,
    pub job: IndexJobRef,
    pub worker_id: WorkerId,
    pub lease_generation: NonZeroU64,
    pub repository: RemoteRepositoryRef,
    pub view_name: PublishedViewName,
    pub snapshot_id: SnapshotId,
    pub expected: Option<GraphPublicationVersion>,
}

impl PublishGraphRequest {
    pub fn validate(&self) -> Result<(), DistributedProtocolError> {
        validate_control_version(self.protocol_version)?;
        if self.job.kind != IndexJobKind::RepositoryGraph
            || self.job.project != self.repository.project
        {
            return Err(DistributedProtocolError::PublicationMismatch);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryPublicationVersion {
    pub revision_id: MemoryRevisionId,
    pub generation: NonZeroU64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishMemoryRequest {
    pub protocol_version: u32,
    pub request_id: RequestId,
    pub job: IndexJobRef,
    pub worker_id: WorkerId,
    pub lease_generation: NonZeroU64,
    pub project: RemoteProjectRef,
    pub view_name: MemoryViewName,
    pub revision_id: MemoryRevisionId,
    pub expected: Option<MemoryPublicationVersion>,
}

impl PublishMemoryRequest {
    pub fn validate(&self) -> Result<(), DistributedProtocolError> {
        validate_control_version(self.protocol_version)?;
        if self.job.kind != IndexJobKind::ProjectMemory || self.job.project != self.project {
            return Err(DistributedProtocolError::PublicationMismatch);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RemoteQueryTarget {
    Repository(RemoteGraphSnapshotRef),
    Memory(RemoteMemoryRevisionRef),
    Federated(FederatedViewRef),
    RepositoryView {
        repository: RemoteRepositoryRef,
        view_name: PublishedViewName,
    },
    MemoryView {
        project: RemoteProjectRef,
        view_name: MemoryViewName,
    },
    FederatedView {
        repository: RemoteRepositoryRef,
        graph_view: PublishedViewName,
        memory_view: MemoryViewName,
    },
}

impl RemoteQueryTarget {
    pub fn project(&self) -> &RemoteProjectRef {
        match self {
            Self::Repository(snapshot) => &snapshot.repository.project,
            Self::Memory(revision) => &revision.project,
            Self::Federated(view) => view.project(),
            Self::RepositoryView { repository, .. } | Self::FederatedView { repository, .. } => {
                &repository.project
            }
            Self::MemoryView { project, .. } => project,
        }
    }
}

/// Remote query envelope. `body` is one existing bounded graph, memory, or
/// federation request DTO, selected by the adapter for `target`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteQueryRequest<T> {
    pub protocol_version: u32,
    pub request_id: RequestId,
    pub project: RemoteProjectRef,
    pub target: RemoteQueryTarget,
    pub body: T,
}

impl<T> RemoteQueryRequest<T> {
    pub fn validate(&self) -> Result<(), DistributedProtocolError> {
        if self.protocol_version != DISTRIBUTED_QUERY_PROTOCOL_VERSION {
            return Err(DistributedProtocolError::UnsupportedVersion);
        }
        if let RemoteQueryTarget::Federated(view) = &self.target {
            view.validate()
                .map_err(|_| DistributedProtocolError::QueryScopeMismatch)?;
        }
        if self.project != *self.target.project() {
            return Err(DistributedProtocolError::QueryScopeMismatch);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteQueryResponse<T> {
    pub protocol_version: u32,
    pub request_id: RequestId,
    pub project: RemoteProjectRef,
    pub resolved_target: RemoteQueryTarget,
    pub body: T,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteErrorCode {
    Unauthorized,
    NotFound,
    UnsupportedVersion,
    InvalidRequest,
    Conflict,
    Cancelled,
    AttemptLimit,
    BudgetExceeded,
    StaleCursor,
    TemporarilyUnavailable,
    Internal,
}

/// Privacy-safe wire error. It has no free-form message or backend detail field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteError {
    pub protocol_version: u32,
    pub request_id: RequestId,
    pub code: RemoteErrorCode,
    pub retryable: bool,
}

fn validate_control_version(version: u32) -> Result<(), DistributedProtocolError> {
    (version == DISTRIBUTED_CONTROL_PROTOCOL_VERSION)
        .then_some(())
        .ok_or(DistributedProtocolError::UnsupportedVersion)
}

fn hash<T: Serialize>(domain: &[u8], value: &T) -> Result<Digest, DistributedProtocolError> {
    let encoded = serde_json::to_vec(value).map_err(|_| DistributedProtocolError::Serialization)?;
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(encoded);
    let value = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Digest::new("sha256", value).map_err(|_| DistributedProtocolError::Serialization)
}

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod tests;
