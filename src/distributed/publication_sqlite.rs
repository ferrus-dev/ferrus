//! Durable tenant-scoped remote fact storage and atomic publication prototype.

use std::{
    collections::BTreeMap,
    num::NonZeroU64,
    path::Path,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use ring::{
    aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey},
    rand::{SecureRandom, SystemRandom},
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::{
    project_memory::{
        diagnostics::MemoryDiagnostic,
        domain::{
            MemoryBuildId, MemoryEntity, MemoryEntityId, MemoryRelationship, MemoryRelationshipId,
            MemoryRelationshipTarget, MemoryRevisionId, MemoryViewName,
        },
    },
    repository_graph::domain::{
        BuildId, Digest, EdgeId, EdgeTarget, GraphDiagnostic, GraphEdge, GraphNode, NodeId,
        PublishedViewName, SnapshotId,
    },
};

use super::{
    DISTRIBUTED_STORAGE_PROTOCOL_VERSION,
    coordinator_sqlite::COORDINATOR_SCHEMA_VERSION,
    identity::{
        FederatedViewRef, IndexJobId, RemoteGraphSnapshotRef, RemoteMemoryRevisionRef,
        RemoteProjectRef, RemoteRepositoryRef, WorkerId,
    },
    protocol::{
        FactBatch, FactBatchPayload, FactTarget, GraphPublicationVersion, IndexInputRef,
        IndexJobKind, IndexJobRef, IndexJobSpec, MemoryPublicationVersion, PublishGraphRequest,
        PublishMemoryRequest,
    },
    publication::{
        GraphPublicationOutcome, MemoryPublicationOutcome, PublishedRemoteGraphView,
        PublishedRemoteMemoryView, RemoteFactCounts, RemoteGraphSnapshotRecord,
        RemoteMemoryRevisionRecord, RemotePublicationStore, StoredRemoteGraphSnapshot,
        StoredRemoteMemoryRevision,
    },
};

pub(super) const STORAGE_SCHEMA_VERSION: u32 = 2;
const NONCE_BYTES: usize = 12;
const SQLITE_PROGRESS_OPS: i32 = 100;

struct ReadDeadline<'connection> {
    connection: &'connection Connection,
}

impl<'connection> ReadDeadline<'connection> {
    fn install(
        connection: &'connection Connection,
        started: Instant,
        duration: Duration,
    ) -> Result<Self, RemoteStoreError> {
        connection
            .progress_handler(
                SQLITE_PROGRESS_OPS,
                Some(move || started.elapsed() >= duration),
            )
            .map_err(RemoteStoreError::Database)?;
        Ok(Self { connection })
    }
}

impl Drop for ReadDeadline<'_> {
    fn drop(&mut self) {
        let _ = self.connection.progress_handler(0, None::<fn() -> bool>);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteStoreLimits {
    pub max_snapshots_per_project: NonZeroU64,
    pub max_facts_per_project: NonZeroU64,
    pub max_bytes_per_project: NonZeroU64,
    pub max_facts_per_snapshot: NonZeroU64,
    pub max_fact_bytes: NonZeroU64,
}

#[derive(Debug, Error)]
pub enum RemoteStoreError {
    #[error("remote fact storage requires authenticated transport and encryption at rest")]
    InsecureProtection,
    #[error("remote publication request or fact stream is invalid")]
    InvalidInput,
    #[error("remote publication job is unavailable, cancelled, expired, or lost its lease")]
    AuthorityLost,
    #[error("remote immutable target conflicts with existing facts")]
    ImmutableConflict,
    #[error("remote fact identities conflict or relationships reference missing facts")]
    FactConflict,
    #[error("remote storage quota exceeded")]
    QuotaExceeded,
    #[error("remote storage read exceeded its duration budget")]
    ReadBudgetExceeded,
    #[error("remote fact ciphertext failed authentication or validation")]
    IntegrityFailure,
    #[error("remote storage schema is incompatible")]
    IncompatibleSchema,
    #[error("remote storage requires the shared durable coordinator schema")]
    MissingCoordinatorSchema,
    #[error("remote storage database operation failed")]
    Database(#[source] rusqlite::Error),
    #[error("remote fact serialization failed")]
    Serialization,
    #[error("remote fact encryption failed")]
    Encryption,
}

impl From<rusqlite::Error> for RemoteStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

struct PreparedGraph {
    record: RemoteGraphSnapshotRecord,
    facts: Vec<PlainFact>,
}

struct PreparedMemory {
    record: RemoteMemoryRevisionRecord,
    facts: Vec<PlainFact>,
}

struct PlainFact {
    kind: &'static str,
    id: String,
    encoded: Vec<u8>,
}

struct EncryptedFact {
    kind: &'static str,
    id: String,
    byte_len: u64,
    nonce: [u8; NONCE_BYTES],
    ciphertext: Vec<u8>,
}

pub struct SqliteRemotePublicationStore {
    connection: Connection,
    key: LessSafeKey,
    limits: RemoteStoreLimits,
}

impl SqliteRemotePublicationStore {
    /// `path` must be the same prototype control-plane database used by
    /// `SqliteIndexJobCoordinator`. Sharing one SQLite transaction is what
    /// serializes cancellation, immutable ingestion, pointer CAS, and job
    /// completion in this adapter.
    pub fn open(
        path: impl AsRef<Path>,
        encryption_key: [u8; 32],
        limits: RemoteStoreLimits,
        authenticated_transport: bool,
    ) -> Result<Self, RemoteStoreError> {
        if !authenticated_transport {
            return Err(RemoteStoreError::InsecureProtection);
        }
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        initialize_schema(&connection)?;
        let key = LessSafeKey::new(
            UnboundKey::new(&AES_256_GCM, &encryption_key)
                .map_err(|_| RemoteStoreError::Encryption)?,
        );
        Ok(Self {
            connection,
            key,
            limits,
        })
    }

    pub fn published_graph_snapshot_bounded(
        &self,
        snapshot: &RemoteGraphSnapshotRef,
        started: Instant,
        duration: Duration,
    ) -> Result<Option<StoredRemoteGraphSnapshot>, RemoteStoreError> {
        let deadline = ReadDeadline::install(&self.connection, started, duration)?;
        let result = self.load_graph_snapshot(snapshot, Some((started, duration)), true);
        drop(deadline);
        result
    }

    pub fn published_memory_revision_bounded(
        &self,
        revision: &RemoteMemoryRevisionRef,
        started: Instant,
        duration: Duration,
    ) -> Result<Option<StoredRemoteMemoryRevision>, RemoteStoreError> {
        let deadline = ReadDeadline::install(&self.connection, started, duration)?;
        let result = self.load_memory_revision(revision, Some((started, duration)), true);
        drop(deadline);
        result
    }

    fn load_graph_snapshot(
        &self,
        snapshot: &RemoteGraphSnapshotRef,
        deadline: Option<(Instant, Duration)>,
        published_only: bool,
    ) -> Result<Option<StoredRemoteGraphSnapshot>, RemoteStoreError> {
        ensure_read_budget(deadline)?;
        if published_only
            && !target_was_published(
                &self.connection,
                &snapshot.repository.project,
                "repository_graph",
                snapshot.repository.repository_id.as_str(),
                snapshot.snapshot_id.as_str(),
            )?
        {
            return Ok(None);
        }
        let Some(record) = load_graph_record(&self.connection, snapshot)? else {
            return Ok(None);
        };
        let facts = load_facts(
            &self.connection,
            &self.key,
            &record.job,
            "repository_graph",
            record.snapshot.snapshot_id.as_str(),
            record.snapshot.repository.repository_id.as_str(),
            deadline,
        )?;
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        let mut diagnostics = Vec::new();
        for (kind, encoded) in facts {
            ensure_read_budget(deadline)?;
            match kind.as_str() {
                "node" => nodes.push(decode(&encoded)?),
                "edge" => edges.push(decode(&encoded)?),
                "diagnostic" => diagnostics.push(decode(&encoded)?),
                _ => return Err(RemoteStoreError::IntegrityFailure),
            }
        }
        if u64::try_from(nodes.len()).ok() != Some(record.counts.primary)
            || u64::try_from(edges.len()).ok() != Some(record.counts.relationships)
            || u64::try_from(diagnostics.len()).ok() != Some(record.counts.diagnostics)
        {
            return Err(RemoteStoreError::IntegrityFailure);
        }
        Ok(Some(StoredRemoteGraphSnapshot {
            record,
            nodes,
            edges,
            diagnostics,
        }))
    }

    fn load_memory_revision(
        &self,
        revision: &RemoteMemoryRevisionRef,
        deadline: Option<(Instant, Duration)>,
        published_only: bool,
    ) -> Result<Option<StoredRemoteMemoryRevision>, RemoteStoreError> {
        ensure_read_budget(deadline)?;
        if published_only
            && !target_was_published(
                &self.connection,
                &revision.project,
                "project_memory",
                "",
                revision.revision_id.as_str(),
            )?
        {
            return Ok(None);
        }
        let Some(record) = load_memory_record(&self.connection, revision)? else {
            return Ok(None);
        };
        let facts = load_facts(
            &self.connection,
            &self.key,
            &record.job,
            "project_memory",
            record.revision.revision_id.as_str(),
            "",
            deadline,
        )?;
        let mut entities = Vec::new();
        let mut relationships = Vec::new();
        let mut diagnostics = Vec::new();
        for (kind, encoded) in facts {
            ensure_read_budget(deadline)?;
            match kind.as_str() {
                "entity" => entities.push(decode(&encoded)?),
                "relationship" => relationships.push(decode(&encoded)?),
                "diagnostic" => diagnostics.push(decode(&encoded)?),
                _ => return Err(RemoteStoreError::IntegrityFailure),
            }
        }
        if u64::try_from(entities.len()).ok() != Some(record.counts.primary)
            || u64::try_from(relationships.len()).ok() != Some(record.counts.relationships)
            || u64::try_from(diagnostics.len()).ok() != Some(record.counts.diagnostics)
        {
            return Err(RemoteStoreError::IntegrityFailure);
        }
        Ok(Some(StoredRemoteMemoryRevision {
            record,
            entities,
            relationships,
            diagnostics,
        }))
    }

    fn publish_graph_prepared(
        &mut self,
        request: &PublishGraphRequest,
        prepared: PreparedGraph,
        now: DateTime<Utc>,
    ) -> Result<GraphPublicationOutcome, RemoteStoreError> {
        let encrypted = encrypt_facts(
            &self.key,
            &request.job,
            "repository_graph",
            request.snapshot_id.as_str(),
            prepared.facts,
            self.limits.max_fact_bytes,
        )?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let job = require_publication_authority(
            &transaction,
            &request.job,
            &request.worker_id,
            request.lease_generation,
            now,
        )?;
        if job.spec.kind != IndexJobKind::RepositoryGraph
            || job.spec.semantics.extractor_set_digest != prepared.record.extractor_set_digest
            || !matches!(
                &job.spec.input,
                IndexInputRef::Repository(manifest) if manifest.repository == request.repository
            )
        {
            return Err(RemoteStoreError::InvalidInput);
        }
        let reused_snapshot =
            insert_graph_snapshot(&transaction, &prepared.record, &encrypted, self.limits)?;
        let actual = load_graph_view(&transaction, &request.repository, &request.view_name)?;
        let expected_matches = graph_expected_matches(request.expected.as_ref(), actual.as_ref());
        let outcome = if !expected_matches {
            GraphPublicationOutcome::Superseded { current: actual }
        } else if let Some(current) = actual
            .as_ref()
            .filter(|view| view.snapshot_id == request.snapshot_id)
            .cloned()
        {
            GraphPublicationOutcome::Published {
                view: current,
                reused_snapshot: true,
            }
        } else {
            let generation = next_generation(actual.as_ref().map(|view| view.generation))?;
            let view = PublishedRemoteGraphView {
                repository: request.repository.clone(),
                view_name: request.view_name.clone(),
                snapshot_id: request.snapshot_id.clone(),
                job: request.job.clone(),
                generation,
            };
            upsert_graph_view(&transaction, &view)?;
            GraphPublicationOutcome::Published {
                view,
                reused_snapshot,
            }
        };
        if matches!(&outcome, GraphPublicationOutcome::Published { .. }) {
            mark_published_target(
                &transaction,
                &request.repository.project,
                "repository_graph",
                request.repository.repository_id.as_str(),
                request.snapshot_id.as_str(),
                now,
            )?;
        }
        complete_job(&transaction, request, now)?;
        transaction.commit()?;
        Ok(outcome)
    }

    fn publish_memory_prepared(
        &mut self,
        request: &PublishMemoryRequest,
        prepared: PreparedMemory,
        now: DateTime<Utc>,
    ) -> Result<MemoryPublicationOutcome, RemoteStoreError> {
        let encrypted = encrypt_facts(
            &self.key,
            &request.job,
            "project_memory",
            request.revision_id.as_str(),
            prepared.facts,
            self.limits.max_fact_bytes,
        )?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let job = require_publication_authority(
            &transaction,
            &request.job,
            &request.worker_id,
            request.lease_generation,
            now,
        )?;
        if job.spec.kind != IndexJobKind::ProjectMemory
            || job.spec.semantics.extractor_set_digest != prepared.record.extractor_set_digest
            || !matches!(
                &job.spec.input,
                IndexInputRef::Memory(manifest) if manifest.project == request.project
            )
        {
            return Err(RemoteStoreError::InvalidInput);
        }
        let reused_revision =
            insert_memory_revision(&transaction, &prepared.record, &encrypted, self.limits)?;
        let actual = load_memory_view(&transaction, &request.project, &request.view_name)?;
        let expected_matches = memory_expected_matches(request.expected.as_ref(), actual.as_ref());
        let outcome = if !expected_matches {
            MemoryPublicationOutcome::Superseded { current: actual }
        } else if let Some(current) = actual
            .as_ref()
            .filter(|view| view.revision_id == request.revision_id)
            .cloned()
        {
            MemoryPublicationOutcome::Published {
                view: current,
                reused_revision: true,
            }
        } else {
            let generation = next_generation(actual.as_ref().map(|view| view.generation))?;
            let view = PublishedRemoteMemoryView {
                project: request.project.clone(),
                view_name: request.view_name.clone(),
                revision_id: request.revision_id.clone(),
                job: request.job.clone(),
                generation,
            };
            upsert_memory_view(&transaction, &view)?;
            MemoryPublicationOutcome::Published {
                view,
                reused_revision,
            }
        };
        if matches!(&outcome, MemoryPublicationOutcome::Published { .. }) {
            mark_published_target(
                &transaction,
                &request.project,
                "project_memory",
                "",
                request.revision_id.as_str(),
                now,
            )?;
        }
        complete_job(&transaction, request, now)?;
        transaction.commit()?;
        Ok(outcome)
    }
}

impl RemotePublicationStore for SqliteRemotePublicationStore {
    type Error = RemoteStoreError;

    fn publish_graph(
        &mut self,
        request: &PublishGraphRequest,
        batches: &[FactBatch],
        now: DateTime<Utc>,
    ) -> Result<GraphPublicationOutcome, Self::Error> {
        request
            .validate()
            .map_err(|_| RemoteStoreError::InvalidInput)?;
        let prepared = prepare_graph(request, batches, now, self.limits.max_facts_per_snapshot)?;
        self.publish_graph_prepared(request, prepared, now)
    }

    fn publish_memory(
        &mut self,
        request: &PublishMemoryRequest,
        batches: &[FactBatch],
        now: DateTime<Utc>,
    ) -> Result<MemoryPublicationOutcome, Self::Error> {
        request
            .validate()
            .map_err(|_| RemoteStoreError::InvalidInput)?;
        let prepared = prepare_memory(request, batches, now, self.limits.max_facts_per_snapshot)?;
        self.publish_memory_prepared(request, prepared, now)
    }

    fn graph_snapshot(
        &self,
        snapshot: &RemoteGraphSnapshotRef,
    ) -> Result<Option<StoredRemoteGraphSnapshot>, Self::Error> {
        self.load_graph_snapshot(snapshot, None, false)
    }

    fn memory_revision(
        &self,
        revision: &RemoteMemoryRevisionRef,
    ) -> Result<Option<StoredRemoteMemoryRevision>, Self::Error> {
        self.load_memory_revision(revision, None, false)
    }

    fn graph_view(
        &self,
        repository: &RemoteRepositoryRef,
        view_name: &PublishedViewName,
    ) -> Result<Option<PublishedRemoteGraphView>, Self::Error> {
        load_graph_view(&self.connection, repository, view_name)
    }

    fn memory_view(
        &self,
        project: &RemoteProjectRef,
        view_name: &MemoryViewName,
    ) -> Result<Option<PublishedRemoteMemoryView>, Self::Error> {
        load_memory_view(&self.connection, project, view_name)
    }

    fn federated_view(
        &self,
        repository: &RemoteRepositoryRef,
        graph_view: &PublishedViewName,
        memory_view: &MemoryViewName,
    ) -> Result<Option<FederatedViewRef>, Self::Error> {
        self.connection
            .query_row(
                "SELECT graph.snapshot_id, memory.revision_id
                 FROM remote_graph_views graph
                 JOIN remote_memory_views memory
                   ON memory.tenant_id = graph.tenant_id AND memory.project_id = graph.project_id
                 WHERE graph.tenant_id = ?1 AND graph.project_id = ?2 AND graph.repository_id = ?3
                   AND graph.view_name = ?4 AND memory.view_name = ?5",
                params![
                    repository.project.tenant_id.as_str(),
                    repository.project.project_id.as_str(),
                    repository.repository_id.as_str(),
                    graph_view.as_str(),
                    memory_view.as_str()
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .map(|(snapshot_id, revision_id)| {
                FederatedViewRef::new(
                    RemoteGraphSnapshotRef {
                        repository: repository.clone(),
                        snapshot_id: SnapshotId::new(snapshot_id)
                            .map_err(|_| RemoteStoreError::IntegrityFailure)?,
                    },
                    RemoteMemoryRevisionRef {
                        project: repository.project.clone(),
                        revision_id: MemoryRevisionId::new(revision_id)
                            .map_err(|_| RemoteStoreError::IntegrityFailure)?,
                    },
                )
                .map_err(|_| RemoteStoreError::IntegrityFailure)
            })
            .transpose()
    }
}

mod write;
use write::*;

mod read;
use read::*;

#[cfg(test)]
#[path = "publication_sqlite_tests.rs"]
mod tests;
