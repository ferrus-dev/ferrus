//! Vendor-neutral immutable remote storage and independent publication ports.

use std::num::NonZeroU64;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    project_memory::{
        diagnostics::MemoryDiagnostic,
        domain::{
            MemoryBuildId, MemoryEntity, MemoryRelationship, MemoryRevisionId, MemoryViewName,
        },
    },
    repository_graph::domain::{
        BuildId, Digest, GraphDiagnostic, GraphEdge, GraphNode, PublishedViewName, SnapshotId,
    },
};

use super::{
    identity::{
        FederatedViewRef, RemoteGraphSnapshotRef, RemoteMemoryRevisionRef, RemoteProjectRef,
        RemoteRepositoryRef,
    },
    protocol::{FactBatch, IndexJobRef, PublishGraphRequest, PublishMemoryRequest},
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteFactCounts {
    pub primary: u64,
    pub relationships: u64,
    pub diagnostics: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteGraphSnapshotRecord {
    pub snapshot: RemoteGraphSnapshotRef,
    pub job: IndexJobRef,
    pub build_id: BuildId,
    pub extractor_set_digest: Digest,
    pub fact_set_digest: Digest,
    pub counts: RemoteFactCounts,
    pub completed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteMemoryRevisionRecord {
    pub revision: RemoteMemoryRevisionRef,
    pub job: IndexJobRef,
    pub build_id: MemoryBuildId,
    pub extractor_set_digest: Digest,
    pub fact_set_digest: Digest,
    pub counts: RemoteFactCounts,
    pub completed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredRemoteGraphSnapshot {
    pub record: RemoteGraphSnapshotRecord,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub diagnostics: Vec<GraphDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredRemoteMemoryRevision {
    pub record: RemoteMemoryRevisionRecord,
    pub entities: Vec<MemoryEntity>,
    pub relationships: Vec<MemoryRelationship>,
    pub diagnostics: Vec<MemoryDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedRemoteGraphView {
    pub repository: RemoteRepositoryRef,
    pub view_name: PublishedViewName,
    pub snapshot_id: SnapshotId,
    pub job: IndexJobRef,
    pub generation: NonZeroU64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedRemoteMemoryView {
    pub project: RemoteProjectRef,
    pub view_name: MemoryViewName,
    pub revision_id: MemoryRevisionId,
    pub job: IndexJobRef,
    pub generation: NonZeroU64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphPublicationOutcome {
    Published {
        view: PublishedRemoteGraphView,
        reused_snapshot: bool,
    },
    Superseded {
        current: Option<PublishedRemoteGraphView>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryPublicationOutcome {
    Published {
        view: PublishedRemoteMemoryView,
        reused_revision: bool,
    },
    Superseded {
        current: Option<PublishedRemoteMemoryView>,
    },
}

/// Internal storage capability. Authorization must happen before these scoped
/// reads are exposed through a remote query API in RG5.5.
pub trait RemotePublicationStore {
    type Error;

    fn publish_graph(
        &mut self,
        request: &PublishGraphRequest,
        batches: &[FactBatch],
        now: DateTime<Utc>,
    ) -> Result<GraphPublicationOutcome, Self::Error>;
    fn publish_memory(
        &mut self,
        request: &PublishMemoryRequest,
        batches: &[FactBatch],
        now: DateTime<Utc>,
    ) -> Result<MemoryPublicationOutcome, Self::Error>;
    fn graph_snapshot(
        &self,
        snapshot: &RemoteGraphSnapshotRef,
    ) -> Result<Option<StoredRemoteGraphSnapshot>, Self::Error>;
    fn memory_revision(
        &self,
        revision: &RemoteMemoryRevisionRef,
    ) -> Result<Option<StoredRemoteMemoryRevision>, Self::Error>;
    fn graph_view(
        &self,
        repository: &RemoteRepositoryRef,
        view_name: &PublishedViewName,
    ) -> Result<Option<PublishedRemoteGraphView>, Self::Error>;
    fn memory_view(
        &self,
        project: &RemoteProjectRef,
        view_name: &MemoryViewName,
    ) -> Result<Option<PublishedRemoteMemoryView>, Self::Error>;
    fn federated_view(
        &self,
        repository: &RemoteRepositoryRef,
        graph_view: &PublishedViewName,
        memory_view: &MemoryViewName,
    ) -> Result<Option<FederatedViewRef>, Self::Error>;
}
