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

const STORAGE_SCHEMA_VERSION: u32 = 1;
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

    pub fn graph_snapshot_bounded(
        &self,
        snapshot: &RemoteGraphSnapshotRef,
        started: Instant,
        duration: Duration,
    ) -> Result<Option<StoredRemoteGraphSnapshot>, RemoteStoreError> {
        let deadline = ReadDeadline::install(&self.connection, started, duration)?;
        let result = self.load_graph_snapshot(snapshot, Some((started, duration)));
        drop(deadline);
        result
    }

    pub fn memory_revision_bounded(
        &self,
        revision: &RemoteMemoryRevisionRef,
        started: Instant,
        duration: Duration,
    ) -> Result<Option<StoredRemoteMemoryRevision>, RemoteStoreError> {
        let deadline = ReadDeadline::install(&self.connection, started, duration)?;
        let result = self.load_memory_revision(revision, Some((started, duration)));
        drop(deadline);
        result
    }

    fn load_graph_snapshot(
        &self,
        snapshot: &RemoteGraphSnapshotRef,
        deadline: Option<(Instant, Duration)>,
    ) -> Result<Option<StoredRemoteGraphSnapshot>, RemoteStoreError> {
        ensure_read_budget(deadline)?;
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
    ) -> Result<Option<StoredRemoteMemoryRevision>, RemoteStoreError> {
        ensure_read_budget(deadline)?;
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
        self.load_graph_snapshot(snapshot, None)
    }

    fn memory_revision(
        &self,
        revision: &RemoteMemoryRevisionRef,
    ) -> Result<Option<StoredRemoteMemoryRevision>, Self::Error> {
        self.load_memory_revision(revision, None)
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

fn prepare_graph(
    request: &PublishGraphRequest,
    batches: &[FactBatch],
    now: DateTime<Utc>,
    max_facts: NonZeroU64,
) -> Result<PreparedGraph, RemoteStoreError> {
    validate_batch_stream(&request.job, batches)?;
    let mut nodes = BTreeMap::<NodeId, GraphNode>::new();
    let mut edges = BTreeMap::<EdgeId, GraphEdge>::new();
    let mut diagnostics = BTreeMap::<String, GraphDiagnostic>::new();
    let mut target = None;
    let mut extractor_set_digest = None;
    for batch in batches {
        let FactTarget::RepositoryGraph { snapshot, build_id } = &batch.header.target else {
            return Err(RemoteStoreError::InvalidInput);
        };
        if snapshot.repository != request.repository || snapshot.snapshot_id != request.snapshot_id
        {
            return Err(RemoteStoreError::InvalidInput);
        }
        match &target {
            None => target = Some((snapshot.clone(), build_id.clone())),
            Some((existing_snapshot, existing_build))
                if existing_snapshot == snapshot && existing_build == build_id => {}
            Some(_) => return Err(RemoteStoreError::InvalidInput),
        }
        match &extractor_set_digest {
            None => extractor_set_digest = Some(batch.header.extractor_set_digest.clone()),
            Some(existing) if existing == &batch.header.extractor_set_digest => {}
            Some(_) => return Err(RemoteStoreError::InvalidInput),
        }
        let FactBatchPayload::RepositoryGraph {
            nodes: batch_nodes,
            edges: batch_edges,
            diagnostics: batch_diagnostics,
        } = &batch.payload
        else {
            return Err(RemoteStoreError::InvalidInput);
        };
        merge_facts(&mut nodes, batch_nodes, |node| &node.id)?;
        merge_facts(&mut edges, batch_edges, |edge| &edge.id)?;
        for diagnostic in batch_diagnostics {
            let encoded =
                serde_json::to_vec(diagnostic).map_err(|_| RemoteStoreError::Serialization)?;
            let id = sha256_value(b"ferrus.remote.graph-diagnostic.v1\0", &encoded);
            if diagnostics
                .insert(id, diagnostic.clone())
                .is_some_and(|existing| existing != *diagnostic)
            {
                return Err(RemoteStoreError::FactConflict);
            }
        }
    }
    for edge in edges.values() {
        if !nodes.contains_key(&edge.source)
            || matches!(&edge.target, EdgeTarget::Node(target) if !nodes.contains_key(target))
        {
            return Err(RemoteStoreError::FactConflict);
        }
    }
    let count = checked_fact_count(nodes.len(), edges.len(), diagnostics.len())?;
    if count > max_facts.get() {
        return Err(RemoteStoreError::QuotaExceeded);
    }
    let (snapshot, build_id) = target.ok_or(RemoteStoreError::InvalidInput)?;
    let extractor_set_digest = extractor_set_digest.ok_or(RemoteStoreError::InvalidInput)?;
    let fact_set_digest = canonical_digest(&(
        nodes.values().collect::<Vec<_>>(),
        edges.values().collect::<Vec<_>>(),
        diagnostics.values().collect::<Vec<_>>(),
    ))?;
    let mut facts = Vec::with_capacity(count as usize);
    append_facts(
        &mut facts,
        "node",
        nodes.into_iter().map(|(id, value)| (id.to_string(), value)),
    )?;
    append_facts(
        &mut facts,
        "edge",
        edges.into_iter().map(|(id, value)| (id.to_string(), value)),
    )?;
    append_facts(&mut facts, "diagnostic", diagnostics)?;
    Ok(PreparedGraph {
        record: RemoteGraphSnapshotRecord {
            snapshot,
            job: request.job.clone(),
            build_id,
            extractor_set_digest,
            fact_set_digest,
            counts: RemoteFactCounts {
                primary: facts.iter().filter(|fact| fact.kind == "node").count() as u64,
                relationships: facts.iter().filter(|fact| fact.kind == "edge").count() as u64,
                diagnostics: facts
                    .iter()
                    .filter(|fact| fact.kind == "diagnostic")
                    .count() as u64,
            },
            completed_at: now,
        },
        facts,
    })
}

fn prepare_memory(
    request: &PublishMemoryRequest,
    batches: &[FactBatch],
    now: DateTime<Utc>,
    max_facts: NonZeroU64,
) -> Result<PreparedMemory, RemoteStoreError> {
    validate_batch_stream(&request.job, batches)?;
    let mut entities = BTreeMap::<MemoryEntityId, MemoryEntity>::new();
    let mut relationships = BTreeMap::<MemoryRelationshipId, MemoryRelationship>::new();
    let mut diagnostics = BTreeMap::<String, MemoryDiagnostic>::new();
    let mut target = None;
    let mut extractor_set_digest = None;
    for batch in batches {
        let FactTarget::ProjectMemory { revision, build_id } = &batch.header.target else {
            return Err(RemoteStoreError::InvalidInput);
        };
        if revision.project != request.project || revision.revision_id != request.revision_id {
            return Err(RemoteStoreError::InvalidInput);
        }
        match &target {
            None => target = Some((revision.clone(), build_id.clone())),
            Some((existing_revision, existing_build))
                if existing_revision == revision && existing_build == build_id => {}
            Some(_) => return Err(RemoteStoreError::InvalidInput),
        }
        match &extractor_set_digest {
            None => extractor_set_digest = Some(batch.header.extractor_set_digest.clone()),
            Some(existing) if existing == &batch.header.extractor_set_digest => {}
            Some(_) => return Err(RemoteStoreError::InvalidInput),
        }
        let FactBatchPayload::ProjectMemory {
            entities: batch_entities,
            relationships: batch_relationships,
            diagnostics: batch_diagnostics,
        } = &batch.payload
        else {
            return Err(RemoteStoreError::InvalidInput);
        };
        merge_facts(&mut entities, batch_entities, |entity| &entity.id)?;
        merge_facts(&mut relationships, batch_relationships, |relationship| {
            &relationship.id
        })?;
        for diagnostic in batch_diagnostics {
            let encoded =
                serde_json::to_vec(diagnostic).map_err(|_| RemoteStoreError::Serialization)?;
            let id = sha256_value(b"ferrus.remote.memory-diagnostic.v1\0", &encoded);
            if diagnostics
                .insert(id, diagnostic.clone())
                .is_some_and(|existing| existing != *diagnostic)
            {
                return Err(RemoteStoreError::FactConflict);
            }
        }
    }
    for relationship in relationships.values() {
        if !entities.contains_key(&relationship.source)
            || matches!(
                &relationship.target,
                MemoryRelationshipTarget::MemoryEntity { entity_id }
                    if !entities.contains_key(entity_id)
            )
        {
            return Err(RemoteStoreError::FactConflict);
        }
    }
    let count = checked_fact_count(entities.len(), relationships.len(), diagnostics.len())?;
    if count > max_facts.get() {
        return Err(RemoteStoreError::QuotaExceeded);
    }
    let (revision, build_id) = target.ok_or(RemoteStoreError::InvalidInput)?;
    let extractor_set_digest = extractor_set_digest.ok_or(RemoteStoreError::InvalidInput)?;
    let fact_set_digest = canonical_digest(&(
        entities.values().collect::<Vec<_>>(),
        relationships.values().collect::<Vec<_>>(),
        diagnostics.values().collect::<Vec<_>>(),
    ))?;
    let mut facts = Vec::with_capacity(count as usize);
    append_facts(
        &mut facts,
        "entity",
        entities
            .into_iter()
            .map(|(id, value)| (id.to_string(), value)),
    )?;
    append_facts(
        &mut facts,
        "relationship",
        relationships
            .into_iter()
            .map(|(id, value)| (id.to_string(), value)),
    )?;
    append_facts(&mut facts, "diagnostic", diagnostics)?;
    Ok(PreparedMemory {
        record: RemoteMemoryRevisionRecord {
            revision,
            job: request.job.clone(),
            build_id,
            extractor_set_digest,
            fact_set_digest,
            counts: RemoteFactCounts {
                primary: facts.iter().filter(|fact| fact.kind == "entity").count() as u64,
                relationships: facts
                    .iter()
                    .filter(|fact| fact.kind == "relationship")
                    .count() as u64,
                diagnostics: facts
                    .iter()
                    .filter(|fact| fact.kind == "diagnostic")
                    .count() as u64,
            },
            completed_at: now,
        },
        facts,
    })
}

fn validate_batch_stream(job: &IndexJobRef, batches: &[FactBatch]) -> Result<(), RemoteStoreError> {
    if batches.is_empty() {
        return Err(RemoteStoreError::InvalidInput);
    }
    let shard = &batches[0].header.shard_id;
    for (index, batch) in batches.iter().enumerate() {
        batch
            .validate()
            .map_err(|_| RemoteStoreError::InvalidInput)?;
        if batch.header.job != *job
            || batch.header.shard_id != *shard
            || usize::try_from(batch.header.sequence).ok() != Some(index)
            || batch.header.final_batch != (index + 1 == batches.len())
        {
            return Err(RemoteStoreError::InvalidInput);
        }
    }
    Ok(())
}

fn merge_facts<K, V, F>(
    destination: &mut BTreeMap<K, V>,
    incoming: &[V],
    key: F,
) -> Result<(), RemoteStoreError>
where
    K: Ord + Clone,
    V: Clone + PartialEq,
    F: Fn(&V) -> &K,
{
    for value in incoming {
        let id = key(value).clone();
        if destination
            .insert(id, value.clone())
            .is_some_and(|existing| existing != *value)
        {
            return Err(RemoteStoreError::FactConflict);
        }
    }
    Ok(())
}

fn append_facts<T: Serialize>(
    output: &mut Vec<PlainFact>,
    kind: &'static str,
    facts: impl IntoIterator<Item = (String, T)>,
) -> Result<(), RemoteStoreError> {
    for (id, fact) in facts {
        output.push(PlainFact {
            kind,
            id,
            encoded: serde_json::to_vec(&fact).map_err(|_| RemoteStoreError::Serialization)?,
        });
    }
    Ok(())
}

fn checked_fact_count(
    primary: usize,
    relationships: usize,
    diagnostics: usize,
) -> Result<u64, RemoteStoreError> {
    [primary, relationships, diagnostics]
        .into_iter()
        .try_fold(0u64, |total, value| {
            total.checked_add(u64::try_from(value).ok()?)
        })
        .ok_or(RemoteStoreError::QuotaExceeded)
}

fn canonical_digest(value: &impl Serialize) -> Result<Digest, RemoteStoreError> {
    let encoded = serde_json::to_vec(value).map_err(|_| RemoteStoreError::Serialization)?;
    Ok(Digest::new(
        "sha256",
        sha256_value(b"ferrus.remote.fact-set.v1\0", &encoded),
    )
    .expect("sha256 output is canonical"))
}

fn sha256_value(domain: &[u8], bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(bytes);
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn encrypt_facts(
    key: &LessSafeKey,
    job: &IndexJobRef,
    domain: &str,
    target_id: &str,
    facts: Vec<PlainFact>,
    max_fact_bytes: NonZeroU64,
) -> Result<Vec<EncryptedFact>, RemoteStoreError> {
    facts
        .into_iter()
        .map(|fact| {
            let byte_len =
                u64::try_from(fact.encoded.len()).map_err(|_| RemoteStoreError::QuotaExceeded)?;
            if byte_len > max_fact_bytes.get() {
                return Err(RemoteStoreError::QuotaExceeded);
            }
            let mut nonce_bytes = [0u8; NONCE_BYTES];
            SystemRandom::new()
                .fill(&mut nonce_bytes)
                .map_err(|_| RemoteStoreError::Encryption)?;
            let nonce = Nonce::assume_unique_for_key(nonce_bytes);
            let mut ciphertext = fact.encoded;
            key.seal_in_place_append_tag(
                nonce,
                Aad::from(fact_aad(job, domain, target_id, fact.kind, &fact.id)),
                &mut ciphertext,
            )
            .map_err(|_| RemoteStoreError::Encryption)?;
            Ok(EncryptedFact {
                kind: fact.kind,
                id: fact.id,
                byte_len,
                nonce: nonce_bytes,
                ciphertext,
            })
        })
        .collect()
}

fn fact_aad(
    job: &IndexJobRef,
    domain: &str,
    target_id: &str,
    fact_kind: &str,
    fact_id: &str,
) -> Vec<u8> {
    format!(
        "{}\0{}\0{}\0{}\0{}\0{}\0{}",
        job.project.tenant_id,
        job.project.project_id,
        job.job_id,
        domain,
        target_id,
        fact_kind,
        fact_id
    )
    .into_bytes()
}

struct JobAuthority {
    spec: IndexJobSpec,
}

fn require_publication_authority(
    transaction: &Transaction<'_>,
    job: &IndexJobRef,
    worker_id: &WorkerId,
    lease_generation: NonZeroU64,
    now: DateTime<Utc>,
) -> Result<JobAuthority, RemoteStoreError> {
    let record = transaction
        .query_row(
            "SELECT kind, spec_json, state, cancellation_requested, lease_worker_id,
                    lease_generation, lease_until_ms, deadline_at_ms
             FROM distributed_index_jobs
             WHERE tenant_id = ?1 AND project_id = ?2 AND job_id = ?3",
            params![
                job.project.tenant_id.as_str(),
                job.project.project_id.as_str(),
                job.job_id.as_str()
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            },
        )
        .optional()?
        .ok_or(RemoteStoreError::AuthorityLost)?;
    let kind = parse_job_kind(&record.0)?;
    let spec: IndexJobSpec =
        serde_json::from_slice(&record.1).map_err(|_| RemoteStoreError::IntegrityFailure)?;
    spec.validate()
        .map_err(|_| RemoteStoreError::IntegrityFailure)?;
    let live = kind == job.kind
        && spec.project() == &job.project
        && record.2 == "publishing"
        && !record.3
        && record.4.as_deref() == Some(worker_id.as_str())
        && u64::try_from(record.5).ok() == Some(lease_generation.get())
        && record
            .6
            .is_some_and(|expires| expires > now.timestamp_millis())
        && record.7 > now.timestamp_millis();
    if !live {
        return Err(RemoteStoreError::AuthorityLost);
    }
    Ok(JobAuthority { spec })
}

trait PublicationLease {
    fn job(&self) -> &IndexJobRef;
    fn worker_id(&self) -> &WorkerId;
    fn lease_generation(&self) -> NonZeroU64;
}

impl PublicationLease for PublishGraphRequest {
    fn job(&self) -> &IndexJobRef {
        &self.job
    }
    fn worker_id(&self) -> &WorkerId {
        &self.worker_id
    }
    fn lease_generation(&self) -> NonZeroU64 {
        self.lease_generation
    }
}

impl PublicationLease for PublishMemoryRequest {
    fn job(&self) -> &IndexJobRef {
        &self.job
    }
    fn worker_id(&self) -> &WorkerId {
        &self.worker_id
    }
    fn lease_generation(&self) -> NonZeroU64 {
        self.lease_generation
    }
}

fn complete_job(
    transaction: &Transaction<'_>,
    request: &impl PublicationLease,
    now: DateTime<Utc>,
) -> Result<(), RemoteStoreError> {
    let changed = transaction.execute(
        "UPDATE distributed_index_jobs
         SET state = 'complete', lease_worker_id = NULL, lease_until_ms = NULL,
             failure_code = NULL, updated_at_ms = ?1
         WHERE tenant_id = ?2 AND project_id = ?3 AND job_id = ?4
           AND state = 'publishing' AND cancellation_requested = 0
           AND lease_worker_id = ?5 AND lease_generation = ?6
           AND lease_until_ms > ?1 AND deadline_at_ms > ?1",
        params![
            now.timestamp_millis(),
            request.job().project.tenant_id.as_str(),
            request.job().project.project_id.as_str(),
            request.job().job_id.as_str(),
            request.worker_id().as_str(),
            i64::try_from(request.lease_generation().get())
                .map_err(|_| RemoteStoreError::InvalidInput)?
        ],
    )?;
    if changed != 1 {
        return Err(RemoteStoreError::AuthorityLost);
    }
    Ok(())
}

fn insert_graph_snapshot(
    transaction: &Transaction<'_>,
    record: &RemoteGraphSnapshotRecord,
    facts: &[EncryptedFact],
    limits: RemoteStoreLimits,
) -> Result<bool, RemoteStoreError> {
    insert_revision(
        transaction,
        &record.snapshot.repository.project,
        "repository_graph",
        record.snapshot.repository.repository_id.as_str(),
        record.snapshot.snapshot_id.as_str(),
        &record.job,
        record.build_id.as_str(),
        &record.extractor_set_digest,
        &record.fact_set_digest,
        record.counts,
        record.completed_at,
        facts,
        limits,
    )
}

fn insert_memory_revision(
    transaction: &Transaction<'_>,
    record: &RemoteMemoryRevisionRecord,
    facts: &[EncryptedFact],
    limits: RemoteStoreLimits,
) -> Result<bool, RemoteStoreError> {
    insert_revision(
        transaction,
        &record.revision.project,
        "project_memory",
        "",
        record.revision.revision_id.as_str(),
        &record.job,
        record.build_id.as_str(),
        &record.extractor_set_digest,
        &record.fact_set_digest,
        record.counts,
        record.completed_at,
        facts,
        limits,
    )
}

#[allow(clippy::too_many_arguments)]
fn insert_revision(
    transaction: &Transaction<'_>,
    project: &RemoteProjectRef,
    domain: &str,
    repository_id: &str,
    target_id: &str,
    job: &IndexJobRef,
    build_id: &str,
    extractor_set_digest: &Digest,
    fact_set_digest: &Digest,
    counts: RemoteFactCounts,
    completed_at: DateTime<Utc>,
    facts: &[EncryptedFact],
    limits: RemoteStoreLimits,
) -> Result<bool, RemoteStoreError> {
    let existing = transaction
        .query_row(
            "SELECT fact_digest_algorithm, fact_digest_value, extractor_digest_algorithm,
                    extractor_digest_value, primary_count, relationship_count, diagnostic_count
             FROM remote_immutable_revisions
             WHERE tenant_id = ?1 AND project_id = ?2 AND domain = ?3
               AND repository_id = ?4 AND target_id = ?5",
            params![
                project.tenant_id.as_str(),
                project.project_id.as_str(),
                domain,
                repository_id,
                target_id
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )
        .optional()?;
    if let Some(existing) = existing {
        let same = existing.0 == fact_set_digest.algorithm()
            && existing.1 == fact_set_digest.value()
            && existing.2 == extractor_set_digest.algorithm()
            && existing.3 == extractor_set_digest.value()
            && u64::try_from(existing.4).ok() == Some(counts.primary)
            && u64::try_from(existing.5).ok() == Some(counts.relationships)
            && u64::try_from(existing.6).ok() == Some(counts.diagnostics);
        return same
            .then_some(true)
            .ok_or(RemoteStoreError::ImmutableConflict);
    }

    enforce_project_quota(transaction, project, facts, limits)?;
    transaction.execute(
        "INSERT INTO remote_immutable_revisions (
             tenant_id, project_id, domain, repository_id, target_id, job_id, job_kind,
             build_id, extractor_digest_algorithm, extractor_digest_value,
             fact_digest_algorithm, fact_digest_value, primary_count, relationship_count,
             diagnostic_count, completed_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            project.tenant_id.as_str(),
            project.project_id.as_str(),
            domain,
            repository_id,
            target_id,
            job.job_id.as_str(),
            job_kind(job.kind),
            build_id,
            extractor_set_digest.algorithm(),
            extractor_set_digest.value(),
            fact_set_digest.algorithm(),
            fact_set_digest.value(),
            i64_from_u64(counts.primary)?,
            i64_from_u64(counts.relationships)?,
            i64_from_u64(counts.diagnostics)?,
            completed_at.timestamp_millis()
        ],
    )?;
    for fact in facts {
        transaction.execute(
            "INSERT INTO remote_encrypted_facts (
                 tenant_id, project_id, domain, repository_id, target_id, fact_kind,
                 fact_id, byte_len, nonce, ciphertext
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                project.tenant_id.as_str(),
                project.project_id.as_str(),
                domain,
                repository_id,
                target_id,
                fact.kind,
                fact.id,
                i64_from_u64(fact.byte_len)?,
                fact.nonce.as_slice(),
                fact.ciphertext
            ],
        )?;
    }
    Ok(false)
}

fn enforce_project_quota(
    transaction: &Transaction<'_>,
    project: &RemoteProjectRef,
    facts: &[EncryptedFact],
    limits: RemoteStoreLimits,
) -> Result<(), RemoteStoreError> {
    let (snapshots, stored_facts, stored_bytes): (i64, i64, i64) = transaction.query_row(
        "SELECT
             (SELECT COUNT(*) FROM remote_immutable_revisions
              WHERE tenant_id = ?1 AND project_id = ?2),
             (SELECT COUNT(*) FROM remote_encrypted_facts
              WHERE tenant_id = ?1 AND project_id = ?2),
             (SELECT COALESCE(SUM(length(ciphertext)), 0) FROM remote_encrypted_facts
              WHERE tenant_id = ?1 AND project_id = ?2)",
        params![project.tenant_id.as_str(), project.project_id.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let incoming_facts = u64::try_from(facts.len()).map_err(|_| RemoteStoreError::QuotaExceeded)?;
    let incoming_bytes = facts
        .iter()
        .try_fold(0u64, |total, fact| {
            total.checked_add(u64::try_from(fact.ciphertext.len()).ok()?)
        })
        .ok_or(RemoteStoreError::QuotaExceeded)?;
    if u64::try_from(snapshots)
        .ok()
        .is_none_or(|value| value >= limits.max_snapshots_per_project.get())
        || u64::try_from(stored_facts).ok().is_none_or(|value| {
            value.saturating_add(incoming_facts) > limits.max_facts_per_project.get()
        })
        || u64::try_from(stored_bytes).ok().is_none_or(|value| {
            value.saturating_add(incoming_bytes) > limits.max_bytes_per_project.get()
        })
    {
        return Err(RemoteStoreError::QuotaExceeded);
    }
    Ok(())
}

fn graph_expected_matches(
    expected: Option<&GraphPublicationVersion>,
    actual: Option<&PublishedRemoteGraphView>,
) -> bool {
    match (expected, actual) {
        (None, None) => true,
        (Some(expected), Some(actual)) => {
            expected.snapshot_id == actual.snapshot_id && expected.generation == actual.generation
        }
        _ => false,
    }
}

fn memory_expected_matches(
    expected: Option<&MemoryPublicationVersion>,
    actual: Option<&PublishedRemoteMemoryView>,
) -> bool {
    match (expected, actual) {
        (None, None) => true,
        (Some(expected), Some(actual)) => {
            expected.revision_id == actual.revision_id && expected.generation == actual.generation
        }
        _ => false,
    }
}

fn next_generation(actual: Option<NonZeroU64>) -> Result<NonZeroU64, RemoteStoreError> {
    let generation = actual
        .map(NonZeroU64::get)
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(RemoteStoreError::IntegrityFailure)?;
    NonZeroU64::new(generation).ok_or(RemoteStoreError::IntegrityFailure)
}

fn upsert_graph_view(
    transaction: &Transaction<'_>,
    view: &PublishedRemoteGraphView,
) -> Result<(), RemoteStoreError> {
    transaction.execute(
        "INSERT INTO remote_graph_views (
             tenant_id, project_id, domain, repository_id, view_name, snapshot_id, job_id,
             generation
         ) VALUES (?1, ?2, 'repository_graph', ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (tenant_id, project_id, repository_id, view_name) DO UPDATE SET
             snapshot_id = excluded.snapshot_id,
             job_id = excluded.job_id,
             generation = excluded.generation",
        params![
            view.repository.project.tenant_id.as_str(),
            view.repository.project.project_id.as_str(),
            view.repository.repository_id.as_str(),
            view.view_name.as_str(),
            view.snapshot_id.as_str(),
            view.job.job_id.as_str(),
            i64_from_u64(view.generation.get())?
        ],
    )?;
    Ok(())
}

fn upsert_memory_view(
    transaction: &Transaction<'_>,
    view: &PublishedRemoteMemoryView,
) -> Result<(), RemoteStoreError> {
    transaction.execute(
        "INSERT INTO remote_memory_views (
             tenant_id, project_id, domain, repository_id, view_name, revision_id, job_id,
             generation
         ) VALUES (?1, ?2, 'project_memory', '', ?3, ?4, ?5, ?6)
         ON CONFLICT (tenant_id, project_id, view_name) DO UPDATE SET
             revision_id = excluded.revision_id,
             job_id = excluded.job_id,
             generation = excluded.generation",
        params![
            view.project.tenant_id.as_str(),
            view.project.project_id.as_str(),
            view.view_name.as_str(),
            view.revision_id.as_str(),
            view.job.job_id.as_str(),
            i64_from_u64(view.generation.get())?
        ],
    )?;
    Ok(())
}

fn load_graph_view(
    connection: &Connection,
    repository: &RemoteRepositoryRef,
    view_name: &PublishedViewName,
) -> Result<Option<PublishedRemoteGraphView>, RemoteStoreError> {
    connection
        .query_row(
            "SELECT snapshot_id, job_id, generation FROM remote_graph_views
             WHERE tenant_id = ?1 AND project_id = ?2 AND repository_id = ?3 AND view_name = ?4",
            params![
                repository.project.tenant_id.as_str(),
                repository.project.project_id.as_str(),
                repository.repository_id.as_str(),
                view_name.as_str()
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?
        .map(|(snapshot_id, job_id, generation)| {
            Ok(PublishedRemoteGraphView {
                repository: repository.clone(),
                view_name: view_name.clone(),
                snapshot_id: SnapshotId::new(snapshot_id)
                    .map_err(|_| RemoteStoreError::IntegrityFailure)?,
                job: IndexJobRef {
                    project: repository.project.clone(),
                    job_id: IndexJobId::new(job_id)
                        .map_err(|_| RemoteStoreError::IntegrityFailure)?,
                    kind: IndexJobKind::RepositoryGraph,
                },
                generation: NonZeroU64::new(
                    u64::try_from(generation).map_err(|_| RemoteStoreError::IntegrityFailure)?,
                )
                .ok_or(RemoteStoreError::IntegrityFailure)?,
            })
        })
        .transpose()
}

fn load_memory_view(
    connection: &Connection,
    project: &RemoteProjectRef,
    view_name: &MemoryViewName,
) -> Result<Option<PublishedRemoteMemoryView>, RemoteStoreError> {
    connection
        .query_row(
            "SELECT revision_id, job_id, generation FROM remote_memory_views
             WHERE tenant_id = ?1 AND project_id = ?2 AND view_name = ?3",
            params![
                project.tenant_id.as_str(),
                project.project_id.as_str(),
                view_name.as_str()
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?
        .map(|(revision_id, job_id, generation)| {
            Ok(PublishedRemoteMemoryView {
                project: project.clone(),
                view_name: view_name.clone(),
                revision_id: MemoryRevisionId::new(revision_id)
                    .map_err(|_| RemoteStoreError::IntegrityFailure)?,
                job: IndexJobRef {
                    project: project.clone(),
                    job_id: IndexJobId::new(job_id)
                        .map_err(|_| RemoteStoreError::IntegrityFailure)?,
                    kind: IndexJobKind::ProjectMemory,
                },
                generation: NonZeroU64::new(
                    u64::try_from(generation).map_err(|_| RemoteStoreError::IntegrityFailure)?,
                )
                .ok_or(RemoteStoreError::IntegrityFailure)?,
            })
        })
        .transpose()
}

fn load_graph_record(
    connection: &Connection,
    snapshot: &RemoteGraphSnapshotRef,
) -> Result<Option<RemoteGraphSnapshotRecord>, RemoteStoreError> {
    load_revision_row(
        connection,
        &snapshot.repository.project,
        "repository_graph",
        snapshot.repository.repository_id.as_str(),
        snapshot.snapshot_id.as_str(),
    )?
    .map(|row| {
        Ok(RemoteGraphSnapshotRecord {
            snapshot: snapshot.clone(),
            job: IndexJobRef {
                project: snapshot.repository.project.clone(),
                job_id: row.job_id,
                kind: IndexJobKind::RepositoryGraph,
            },
            build_id: BuildId::new(row.build_id).map_err(|_| RemoteStoreError::IntegrityFailure)?,
            extractor_set_digest: row.extractor_set_digest,
            fact_set_digest: row.fact_set_digest,
            counts: row.counts,
            completed_at: row.completed_at,
        })
    })
    .transpose()
}

fn load_memory_record(
    connection: &Connection,
    revision: &RemoteMemoryRevisionRef,
) -> Result<Option<RemoteMemoryRevisionRecord>, RemoteStoreError> {
    load_revision_row(
        connection,
        &revision.project,
        "project_memory",
        "",
        revision.revision_id.as_str(),
    )?
    .map(|row| {
        Ok(RemoteMemoryRevisionRecord {
            revision: revision.clone(),
            job: IndexJobRef {
                project: revision.project.clone(),
                job_id: row.job_id,
                kind: IndexJobKind::ProjectMemory,
            },
            build_id: MemoryBuildId::new(row.build_id)
                .map_err(|_| RemoteStoreError::IntegrityFailure)?,
            extractor_set_digest: row.extractor_set_digest,
            fact_set_digest: row.fact_set_digest,
            counts: row.counts,
            completed_at: row.completed_at,
        })
    })
    .transpose()
}

struct RevisionRow {
    job_id: IndexJobId,
    build_id: String,
    extractor_set_digest: Digest,
    fact_set_digest: Digest,
    counts: RemoteFactCounts,
    completed_at: DateTime<Utc>,
}

fn load_revision_row(
    connection: &Connection,
    project: &RemoteProjectRef,
    domain: &str,
    repository_id: &str,
    target_id: &str,
) -> Result<Option<RevisionRow>, RemoteStoreError> {
    connection
        .query_row(
            "SELECT job_id, build_id, extractor_digest_algorithm, extractor_digest_value,
                    fact_digest_algorithm, fact_digest_value, primary_count, relationship_count,
                    diagnostic_count, completed_at_ms
             FROM remote_immutable_revisions
             WHERE tenant_id = ?1 AND project_id = ?2 AND domain = ?3
               AND repository_id = ?4 AND target_id = ?5",
            params![
                project.tenant_id.as_str(),
                project.project_id.as_str(),
                domain,
                repository_id,
                target_id
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                ))
            },
        )
        .optional()?
        .map(|row| {
            Ok(RevisionRow {
                job_id: IndexJobId::new(row.0).map_err(|_| RemoteStoreError::IntegrityFailure)?,
                build_id: row.1,
                extractor_set_digest: Digest::new(row.2, row.3)
                    .map_err(|_| RemoteStoreError::IntegrityFailure)?,
                fact_set_digest: Digest::new(row.4, row.5)
                    .map_err(|_| RemoteStoreError::IntegrityFailure)?,
                counts: RemoteFactCounts {
                    primary: u64::try_from(row.6)
                        .map_err(|_| RemoteStoreError::IntegrityFailure)?,
                    relationships: u64::try_from(row.7)
                        .map_err(|_| RemoteStoreError::IntegrityFailure)?,
                    diagnostics: u64::try_from(row.8)
                        .map_err(|_| RemoteStoreError::IntegrityFailure)?,
                },
                completed_at: DateTime::from_timestamp_millis(row.9)
                    .ok_or(RemoteStoreError::IntegrityFailure)?,
            })
        })
        .transpose()
}

fn load_facts(
    connection: &Connection,
    key: &LessSafeKey,
    job: &IndexJobRef,
    domain: &str,
    target_id: &str,
    repository_id: &str,
    deadline: Option<(Instant, Duration)>,
) -> Result<Vec<(String, Vec<u8>)>, RemoteStoreError> {
    let mut statement = connection.prepare(
        "SELECT fact_kind, fact_id, byte_len, nonce, ciphertext
         FROM remote_encrypted_facts
         WHERE tenant_id = ?1 AND project_id = ?2 AND domain = ?3
           AND repository_id = ?4 AND target_id = ?5
         ORDER BY fact_kind, fact_id",
    )?;
    let rows = statement.query_map(
        params![
            job.project.tenant_id.as_str(),
            job.project.project_id.as_str(),
            domain,
            repository_id,
            target_id
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, Vec<u8>>(4)?,
            ))
        },
    )?;
    let mut facts = Vec::new();
    for row in rows {
        ensure_read_budget(deadline)?;
        let (kind, id, byte_len, nonce, mut ciphertext) = match row {
            Ok(row) => row,
            Err(_) if read_budget_exceeded(deadline) => {
                return Err(RemoteStoreError::ReadBudgetExceeded);
            }
            Err(error) => return Err(RemoteStoreError::Database(error)),
        };
        let nonce = Nonce::try_assume_unique_for_key(&nonce)
            .map_err(|_| RemoteStoreError::IntegrityFailure)?;
        let plaintext = key
            .open_in_place(
                nonce,
                Aad::from(fact_aad(job, domain, target_id, &kind, &id)),
                &mut ciphertext,
            )
            .map_err(|_| RemoteStoreError::IntegrityFailure)?;
        if u64::try_from(plaintext.len()).ok() != u64::try_from(byte_len).ok() {
            return Err(RemoteStoreError::IntegrityFailure);
        }
        facts.push((kind, plaintext.to_vec()));
    }
    Ok(facts)
}

fn read_budget_exceeded(deadline: Option<(Instant, Duration)>) -> bool {
    deadline.is_some_and(|(started, duration)| started.elapsed() >= duration)
}

fn ensure_read_budget(deadline: Option<(Instant, Duration)>) -> Result<(), RemoteStoreError> {
    if read_budget_exceeded(deadline) {
        Err(RemoteStoreError::ReadBudgetExceeded)
    } else {
        Ok(())
    }
}

fn decode<T: serde::de::DeserializeOwned>(encoded: &[u8]) -> Result<T, RemoteStoreError> {
    serde_json::from_slice(encoded).map_err(|_| RemoteStoreError::IntegrityFailure)
}

fn parse_job_kind(value: &str) -> Result<IndexJobKind, RemoteStoreError> {
    match value {
        "repository_graph" => Ok(IndexJobKind::RepositoryGraph),
        "project_memory" => Ok(IndexJobKind::ProjectMemory),
        _ => Err(RemoteStoreError::IntegrityFailure),
    }
}

fn job_kind(value: IndexJobKind) -> &'static str {
    match value {
        IndexJobKind::RepositoryGraph => "repository_graph",
        IndexJobKind::ProjectMemory => "project_memory",
    }
}

fn i64_from_u64(value: u64) -> Result<i64, RemoteStoreError> {
    i64::try_from(value).map_err(|_| RemoteStoreError::QuotaExceeded)
}

fn initialize_schema(connection: &Connection) -> Result<(), RemoteStoreError> {
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS remote_storage_metadata (
             singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
             schema_version INTEGER NOT NULL,
             protocol_version INTEGER NOT NULL
         );",
    )?;
    let coordinator_exists = connection
        .query_row(
            "SELECT 1 FROM sqlite_master
             WHERE type = 'table' AND name = 'distributed_coordinator_metadata'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !coordinator_exists {
        return Err(RemoteStoreError::MissingCoordinatorSchema);
    }
    let coordinator_version = connection
        .query_row(
            "SELECT schema_version FROM distributed_coordinator_metadata WHERE singleton = 1",
            [],
            |row| row.get::<_, u32>(0),
        )
        .optional()?;
    if coordinator_version != Some(COORDINATOR_SCHEMA_VERSION) {
        return Err(RemoteStoreError::IncompatibleSchema);
    }
    let jobs_exist = connection
        .query_row(
            "SELECT 1 FROM sqlite_master
             WHERE type = 'table' AND name = 'distributed_index_jobs'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !jobs_exist {
        return Err(RemoteStoreError::MissingCoordinatorSchema);
    }
    let version = connection
        .query_row(
            "SELECT schema_version, protocol_version FROM remote_storage_metadata WHERE singleton = 1",
            [],
            |row| Ok((row.get::<_, u32>(0)?, row.get::<_, u32>(1)?)),
        )
        .optional()?;
    match version {
        None => {
            connection.execute(
                "INSERT OR IGNORE INTO remote_storage_metadata
                 (singleton, schema_version, protocol_version) VALUES (1, ?1, ?2)",
                params![STORAGE_SCHEMA_VERSION, DISTRIBUTED_STORAGE_PROTOCOL_VERSION],
            )?;
        }
        Some((schema, protocol))
            if schema == STORAGE_SCHEMA_VERSION
                && protocol == DISTRIBUTED_STORAGE_PROTOCOL_VERSION => {}
        Some(_) => return Err(RemoteStoreError::IncompatibleSchema),
    }
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS remote_immutable_revisions (
             tenant_id TEXT NOT NULL,
             project_id TEXT NOT NULL,
             domain TEXT NOT NULL CHECK (domain IN ('repository_graph', 'project_memory')),
             repository_id TEXT NOT NULL,
             target_id TEXT NOT NULL,
             job_id TEXT NOT NULL,
             job_kind TEXT NOT NULL,
             build_id TEXT NOT NULL,
             extractor_digest_algorithm TEXT NOT NULL,
             extractor_digest_value TEXT NOT NULL,
             fact_digest_algorithm TEXT NOT NULL,
             fact_digest_value TEXT NOT NULL,
             primary_count INTEGER NOT NULL CHECK (primary_count >= 0),
             relationship_count INTEGER NOT NULL CHECK (relationship_count >= 0),
             diagnostic_count INTEGER NOT NULL CHECK (diagnostic_count >= 0),
             completed_at_ms INTEGER NOT NULL,
             PRIMARY KEY (tenant_id, project_id, domain, repository_id, target_id)
         );
         CREATE TABLE IF NOT EXISTS remote_encrypted_facts (
             tenant_id TEXT NOT NULL,
             project_id TEXT NOT NULL,
             domain TEXT NOT NULL,
             repository_id TEXT NOT NULL,
             target_id TEXT NOT NULL,
             fact_kind TEXT NOT NULL,
             fact_id TEXT NOT NULL,
             byte_len INTEGER NOT NULL CHECK (byte_len >= 0),
             nonce BLOB NOT NULL,
             ciphertext BLOB NOT NULL,
             PRIMARY KEY (
                 tenant_id, project_id, domain, repository_id, target_id, fact_kind, fact_id
             ),
             FOREIGN KEY (tenant_id, project_id, domain, repository_id, target_id)
                 REFERENCES remote_immutable_revisions (
                     tenant_id, project_id, domain, repository_id, target_id
                 ) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS remote_graph_views (
             tenant_id TEXT NOT NULL,
             project_id TEXT NOT NULL,
             domain TEXT NOT NULL CHECK (domain = 'repository_graph'),
             repository_id TEXT NOT NULL,
             view_name TEXT NOT NULL,
             snapshot_id TEXT NOT NULL,
             job_id TEXT NOT NULL,
             generation INTEGER NOT NULL CHECK (generation > 0),
             PRIMARY KEY (tenant_id, project_id, repository_id, view_name),
             FOREIGN KEY (tenant_id, project_id, domain, repository_id, snapshot_id)
                 REFERENCES remote_immutable_revisions (
                     tenant_id, project_id, domain, repository_id, target_id
                 ) ON DELETE RESTRICT
         );
         CREATE TABLE IF NOT EXISTS remote_memory_views (
             tenant_id TEXT NOT NULL,
             project_id TEXT NOT NULL,
             domain TEXT NOT NULL CHECK (domain = 'project_memory'),
             repository_id TEXT NOT NULL CHECK (repository_id = ''),
             view_name TEXT NOT NULL,
             revision_id TEXT NOT NULL,
             job_id TEXT NOT NULL,
             generation INTEGER NOT NULL CHECK (generation > 0),
             PRIMARY KEY (tenant_id, project_id, view_name),
             FOREIGN KEY (tenant_id, project_id, domain, repository_id, revision_id)
                 REFERENCES remote_immutable_revisions (
                     tenant_id, project_id, domain, repository_id, target_id
                 ) ON DELETE RESTRICT
         );",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        distributed::{
            DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            coordinator::{AdvanceIndexJobRequest, ClaimIndexJobRequest, IndexJobCoordinator},
            coordinator_sqlite::{CoordinatorLimits, SqliteIndexJobCoordinator},
            identity::{
                FactShardId, MemoryManifestId, ObjectId, RemoteProjectId, RemoteRepositoryId,
                RepositoryManifestId, RequestId, TenantId, TenantObjectRef,
            },
            protocol::{IndexInputRef, IndexSemantics, SubmitIndexJobRequest},
        },
        repository_graph::domain::{
            Confidence, ExtractorId, ExtractorIdentity, FactProvenance, GraphValue,
            ResolutionState, SemanticKey,
        },
    };

    fn digest(value: &str) -> Digest {
        Digest::new("sha256", value).unwrap()
    }

    fn project(tenant: &str) -> RemoteProjectRef {
        RemoteProjectRef {
            tenant_id: TenantId::new(tenant).unwrap(),
            project_id: RemoteProjectId::new("project").unwrap(),
        }
    }

    fn repository(tenant: &str) -> RemoteRepositoryRef {
        RemoteRepositoryRef {
            project: project(tenant),
            repository_id: RemoteRepositoryId::new("repository").unwrap(),
        }
    }

    fn limits() -> RemoteStoreLimits {
        RemoteStoreLimits {
            max_snapshots_per_project: NonZeroU64::new(100).unwrap(),
            max_facts_per_project: NonZeroU64::new(10_000).unwrap(),
            max_bytes_per_project: NonZeroU64::new(16 * 1024 * 1024).unwrap(),
            max_facts_per_snapshot: NonZeroU64::new(1_000).unwrap(),
            max_fact_bytes: NonZeroU64::new(1024 * 1024).unwrap(),
        }
    }

    fn coordinator(path: &Path) -> SqliteIndexJobCoordinator {
        SqliteIndexJobCoordinator::open(
            path,
            CoordinatorLimits {
                max_attempts: std::num::NonZeroU32::new(3).unwrap(),
                lease_ttl_ms: NonZeroU64::new(60_000).unwrap(),
                max_job_duration_ms: NonZeroU64::new(120_000).unwrap(),
            },
        )
        .unwrap()
    }

    fn store(path: &Path) -> SqliteRemotePublicationStore {
        SqliteRemotePublicationStore::open(path, [53; 32], limits(), true).unwrap()
    }

    fn input(kind: IndexJobKind, tenant: &str, unique: &str) -> IndexInputRef {
        let project = project(tenant);
        let identity = digest(unique);
        let object = TenantObjectRef {
            project: project.clone(),
            object_id: ObjectId::new(identity.value()).unwrap(),
            content_identity: identity.clone(),
        };
        match kind {
            IndexJobKind::RepositoryGraph => {
                IndexInputRef::Repository(super::super::identity::RepositoryManifestRef {
                    repository: repository(tenant),
                    manifest_id: RepositoryManifestId::new(identity.value()).unwrap(),
                    manifest_digest: identity,
                    source_policy_digest: digest("22"),
                    manifest_object: object,
                })
            }
            IndexJobKind::ProjectMemory => {
                IndexInputRef::Memory(super::super::identity::MemoryManifestRef {
                    project,
                    manifest_id: MemoryManifestId::new(identity.value()).unwrap(),
                    manifest_digest: identity,
                    memory_policy_digest: digest("22"),
                    manifest_object: object,
                })
            }
        }
    }

    fn publishing_job(
        coordinator: &mut SqliteIndexJobCoordinator,
        kind: IndexJobKind,
        unique: &str,
    ) -> super::super::protocol::IndexJobRecord {
        let now = Utc::now();
        let spec = IndexJobSpec::new(
            kind,
            input(kind, "tenant-a", unique),
            IndexSemantics {
                semantic_config_digest: digest("33"),
                model_version: std::num::NonZeroU32::new(1).unwrap(),
                extractor_set_digest: digest("44"),
            },
        )
        .unwrap();
        coordinator
            .submit(
                &SubmitIndexJobRequest {
                    protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                    request_id: RequestId::new(format!("submit-{unique}")).unwrap(),
                    project: project("tenant-a"),
                    job: spec,
                },
                now,
            )
            .unwrap();
        let leased = coordinator
            .claim(
                &ClaimIndexJobRequest {
                    protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                    request_id: RequestId::new(format!("claim-{unique}")).unwrap(),
                    project: project("tenant-a"),
                    kind,
                    worker_id: WorkerId::new(format!("worker-{unique}")).unwrap(),
                },
                now,
            )
            .unwrap()
            .unwrap();
        let running = coordinator.start(&advance(&leased, unique), now).unwrap();
        coordinator
            .begin_publication(&advance(&running, unique), now)
            .unwrap()
    }

    fn advance(
        record: &super::super::protocol::IndexJobRecord,
        unique: &str,
    ) -> AdvanceIndexJobRequest {
        let lease = record.lease.as_ref().unwrap();
        AdvanceIndexJobRequest {
            protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            request_id: RequestId::new(format!("advance-{unique}")).unwrap(),
            job: record.job.clone(),
            worker_id: lease.worker_id.clone(),
            lease_generation: lease.generation,
        }
    }

    fn graph_request(
        job: &super::super::protocol::IndexJobRecord,
        snapshot: &str,
        expected: Option<GraphPublicationVersion>,
    ) -> PublishGraphRequest {
        let lease = job.lease.as_ref().unwrap();
        PublishGraphRequest {
            protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            request_id: RequestId::new(format!("publish-{snapshot}")).unwrap(),
            job: job.job.clone(),
            worker_id: lease.worker_id.clone(),
            lease_generation: lease.generation,
            repository: repository("tenant-a"),
            view_name: PublishedViewName::new("canonical").unwrap(),
            snapshot_id: SnapshotId::new(snapshot).unwrap(),
            expected,
        }
    }

    fn memory_request(
        job: &super::super::protocol::IndexJobRecord,
        revision: &str,
        expected: Option<MemoryPublicationVersion>,
    ) -> PublishMemoryRequest {
        let lease = job.lease.as_ref().unwrap();
        PublishMemoryRequest {
            protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            request_id: RequestId::new(format!("publish-{revision}")).unwrap(),
            job: job.job.clone(),
            worker_id: lease.worker_id.clone(),
            lease_generation: lease.generation,
            project: project("tenant-a"),
            view_name: MemoryViewName::new("project").unwrap(),
            revision_id: MemoryRevisionId::new(revision).unwrap(),
            expected,
        }
    }

    fn graph_batch(job: &IndexJobRef, snapshot: &str, secret: &str) -> FactBatch {
        graph_batch_with_final(job, snapshot, secret, true)
    }

    fn graph_batch_with_final(
        job: &IndexJobRef,
        snapshot: &str,
        secret: &str,
        final_batch: bool,
    ) -> FactBatch {
        let snapshot_id = SnapshotId::new(snapshot).unwrap();
        let node = GraphNode {
            snapshot_id: snapshot_id.clone(),
            id: NodeId::new(format!("node-{snapshot}")).unwrap(),
            kind: "symbol".to_string(),
            semantic_key: Some(SemanticKey::new(format!("symbol-{snapshot}")).unwrap()),
            provenance: FactProvenance {
                extractor: ExtractorIdentity {
                    id: ExtractorId::new("test.extractor").unwrap(),
                    version: "1".to_string(),
                    contract_version: 1,
                },
                evidence: None,
                resolution: ResolutionState::Resolved,
                confidence: Confidence::Exact,
            },
            properties: BTreeMap::from([(
                "private".to_string(),
                GraphValue::String(secret.to_string()),
            )]),
        };
        FactBatch::new(
            job.clone(),
            FactTarget::RepositoryGraph {
                snapshot: RemoteGraphSnapshotRef {
                    repository: repository("tenant-a"),
                    snapshot_id,
                },
                build_id: BuildId::new(format!("build-{snapshot}")).unwrap(),
            },
            FactShardId::new("repository-all").unwrap(),
            0,
            digest("44"),
            final_batch,
            FactBatchPayload::RepositoryGraph {
                nodes: vec![node],
                edges: Vec::new(),
                diagnostics: Vec::new(),
            },
        )
        .unwrap()
    }

    fn memory_batch(job: &IndexJobRef, revision: &str) -> FactBatch {
        FactBatch::new(
            job.clone(),
            FactTarget::ProjectMemory {
                revision: RemoteMemoryRevisionRef {
                    project: project("tenant-a"),
                    revision_id: MemoryRevisionId::new(revision).unwrap(),
                },
                build_id: MemoryBuildId::new(format!("build-{revision}")).unwrap(),
            },
            FactShardId::new("memory-all").unwrap(),
            0,
            digest("44"),
            true,
            FactBatchPayload::ProjectMemory {
                entities: Vec::new(),
                relationships: Vec::new(),
                diagnostics: Vec::new(),
            },
        )
        .unwrap()
    }

    #[test]
    fn graph_publication_is_atomic_encrypted_and_completes_the_job() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.db");
        let mut coordinator = coordinator(&path);
        let job = publishing_job(&mut coordinator, IndexJobKind::RepositoryGraph, "11");
        let request = graph_request(&job, "snapshot-one", None);
        let batch = graph_batch(&job.job, "snapshot-one", "private-symbol-name");
        let mut store = store(&path);
        let outcome = store
            .publish_graph(&request, std::slice::from_ref(&batch), Utc::now())
            .unwrap();
        assert!(matches!(
            outcome,
            GraphPublicationOutcome::Published { ref view, .. }
                if view.snapshot_id == request.snapshot_id && view.generation.get() == 1
        ));
        let stored = store
            .graph_snapshot(&RemoteGraphSnapshotRef {
                repository: request.repository.clone(),
                snapshot_id: request.snapshot_id.clone(),
            })
            .unwrap()
            .unwrap();
        assert_eq!(stored.nodes.len(), 1);
        store
            .connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            !bytes
                .windows("private-symbol-name".len())
                .any(|window| window == b"private-symbol-name")
        );
        let inspected = coordinator
            .inspect(&super::super::protocol::InspectIndexJobRequest {
                protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                request_id: RequestId::new("inspect-complete").unwrap(),
                job: job.job,
            })
            .unwrap()
            .unwrap();
        assert_eq!(
            inspected.state,
            super::super::protocol::IndexJobState::Complete
        );
    }

    #[test]
    fn partial_stream_and_cancelled_publication_remain_invisible() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.db");
        let mut coordinator = coordinator(&path);
        let job = publishing_job(&mut coordinator, IndexJobKind::RepositoryGraph, "12");
        let request = graph_request(&job, "snapshot-partial", None);
        let batch = graph_batch_with_final(&job.job, "snapshot-partial", "private", false);
        let mut store = store(&path);
        assert!(matches!(
            store.publish_graph(&request, &[batch], Utc::now()),
            Err(RemoteStoreError::InvalidInput)
        ));
        assert!(
            store
                .graph_view(&request.repository, &request.view_name)
                .unwrap()
                .is_none()
        );

        coordinator
            .cancel(
                &super::super::protocol::CancelIndexJobRequest {
                    protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                    request_id: RequestId::new("cancel").unwrap(),
                    job: job.job.clone(),
                    expected_state: Some(super::super::protocol::IndexJobState::Publishing),
                },
                Utc::now(),
            )
            .unwrap();
        let valid = graph_batch(&job.job, "snapshot-partial", "private");
        assert!(matches!(
            store.publish_graph(&request, &[valid], Utc::now()),
            Err(RemoteStoreError::AuthorityLost)
        ));
        assert!(
            store
                .graph_snapshot(&RemoteGraphSnapshotRef {
                    repository: request.repository,
                    snapshot_id: request.snapshot_id,
                })
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn stale_graph_cas_cannot_replace_the_current_pointer() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.db");
        let mut coordinator = coordinator(&path);
        let first_job = publishing_job(&mut coordinator, IndexJobKind::RepositoryGraph, "13");
        let first_request = graph_request(&first_job, "snapshot-first", None);
        let mut store = store(&path);
        store
            .publish_graph(
                &first_request,
                &[graph_batch(&first_job.job, "snapshot-first", "one")],
                Utc::now(),
            )
            .unwrap();

        let old_job = publishing_job(&mut coordinator, IndexJobKind::RepositoryGraph, "14");
        let old_request = graph_request(&old_job, "snapshot-old", None);
        let outcome = store
            .publish_graph(
                &old_request,
                &[graph_batch(&old_job.job, "snapshot-old", "old")],
                Utc::now(),
            )
            .unwrap();
        assert!(matches!(
            outcome,
            GraphPublicationOutcome::Superseded { current: Some(ref view) }
                if view.snapshot_id == first_request.snapshot_id
        ));
        assert!(
            store
                .graph_snapshot(&RemoteGraphSnapshotRef {
                    repository: old_request.repository.clone(),
                    snapshot_id: old_request.snapshot_id,
                })
                .unwrap()
                .is_some()
        );
        assert_eq!(
            store
                .graph_view(&first_request.repository, &first_request.view_name)
                .unwrap()
                .unwrap()
                .snapshot_id,
            first_request.snapshot_id
        );
    }

    #[test]
    fn concurrent_graph_publishers_with_one_expectation_choose_one_winner() {
        use std::sync::{Arc, Barrier};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.db");
        let mut coordinator = coordinator(&path);
        let left_job = publishing_job(&mut coordinator, IndexJobKind::RepositoryGraph, "17");
        let right_job = publishing_job(&mut coordinator, IndexJobKind::RepositoryGraph, "18");
        let left_request = graph_request(&left_job, "snapshot-left", None);
        let right_request = graph_request(&right_job, "snapshot-right", None);
        let left_batch = graph_batch(&left_job.job, "snapshot-left", "left");
        let right_batch = graph_batch(&right_job.job, "snapshot-right", "right");
        let barrier = Arc::new(Barrier::new(3));

        let left_path = path.clone();
        let left_barrier = Arc::clone(&barrier);
        let left = std::thread::spawn(move || {
            let mut store = store(&left_path);
            left_barrier.wait();
            store
                .publish_graph(&left_request, &[left_batch], Utc::now())
                .unwrap()
        });
        let right_path = path.clone();
        let right_barrier = Arc::clone(&barrier);
        let right = std::thread::spawn(move || {
            let mut store = store(&right_path);
            right_barrier.wait();
            store
                .publish_graph(&right_request, &[right_batch], Utc::now())
                .unwrap()
        });
        barrier.wait();
        let outcomes = [left.join().unwrap(), right.join().unwrap()];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, GraphPublicationOutcome::Published { .. }))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, GraphPublicationOutcome::Superseded { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn same_snapshot_is_reused_only_after_expected_pointer_matches() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.db");
        let mut coordinator = coordinator(&path);
        let first_job = publishing_job(&mut coordinator, IndexJobKind::RepositoryGraph, "19");
        let first_request = graph_request(&first_job, "snapshot-reused", None);
        let mut store = store(&path);
        let first = store
            .publish_graph(
                &first_request,
                &[graph_batch(&first_job.job, "snapshot-reused", "same")],
                Utc::now(),
            )
            .unwrap();
        let GraphPublicationOutcome::Published { view, .. } = first else {
            panic!("initial publication must win");
        };

        let retry_job = publishing_job(&mut coordinator, IndexJobKind::RepositoryGraph, "1a");
        let retry_request = graph_request(
            &retry_job,
            "snapshot-reused",
            Some(GraphPublicationVersion {
                snapshot_id: view.snapshot_id.clone(),
                generation: view.generation,
            }),
        );
        let outcome = store
            .publish_graph(
                &retry_request,
                &[graph_batch(&retry_job.job, "snapshot-reused", "same")],
                Utc::now(),
            )
            .unwrap();
        assert!(matches!(
            outcome,
            GraphPublicationOutcome::Published {
                ref view,
                reused_snapshot: true
            } if view.generation.get() == 1
        ));
    }

    #[test]
    fn encrypted_fact_tampering_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.db");
        let mut coordinator = coordinator(&path);
        let job = publishing_job(&mut coordinator, IndexJobKind::RepositoryGraph, "1b");
        let request = graph_request(&job, "snapshot-tampered", None);
        let mut store = store(&path);
        store
            .publish_graph(
                &request,
                &[graph_batch(&job.job, "snapshot-tampered", "private")],
                Utc::now(),
            )
            .unwrap();
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE remote_encrypted_facts SET ciphertext = zeroblob(length(ciphertext))",
                [],
            )
            .unwrap();
        assert!(matches!(
            store.graph_snapshot(&RemoteGraphSnapshotRef {
                repository: request.repository,
                snapshot_id: request.snapshot_id,
            }),
            Err(RemoteStoreError::IntegrityFailure)
        ));
    }

    #[test]
    fn bounded_snapshot_reads_stop_after_the_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.db");
        let mut coordinator = coordinator(&path);
        let job = publishing_job(&mut coordinator, IndexJobKind::RepositoryGraph, "1c");
        let request = graph_request(&job, "snapshot-deadline", None);
        let mut store = store(&path);
        store
            .publish_graph(
                &request,
                &[graph_batch(&job.job, "snapshot-deadline", "private")],
                Utc::now(),
            )
            .unwrap();
        assert!(matches!(
            store.graph_snapshot_bounded(
                &RemoteGraphSnapshotRef {
                    repository: request.repository,
                    snapshot_id: request.snapshot_id,
                },
                Instant::now(),
                Duration::ZERO,
            ),
            Err(RemoteStoreError::ReadBudgetExceeded)
        ));
    }

    #[test]
    fn graph_and_memory_pointers_advance_independently_and_form_explicit_pair() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.db");
        let mut coordinator = coordinator(&path);
        let graph_job = publishing_job(&mut coordinator, IndexJobKind::RepositoryGraph, "15");
        let graph_request = graph_request(&graph_job, "snapshot-pair", None);
        let mut store = store(&path);
        store
            .publish_graph(
                &graph_request,
                &[graph_batch(&graph_job.job, "snapshot-pair", "pair")],
                Utc::now(),
            )
            .unwrap();
        let memory_job = publishing_job(&mut coordinator, IndexJobKind::ProjectMemory, "16");
        let memory_request = memory_request(&memory_job, "revision-pair", None);
        store
            .publish_memory(
                &memory_request,
                &[memory_batch(&memory_job.job, "revision-pair")],
                Utc::now(),
            )
            .unwrap();
        let graph = store
            .graph_view(&graph_request.repository, &graph_request.view_name)
            .unwrap()
            .unwrap();
        let memory = store
            .memory_view(&memory_request.project, &memory_request.view_name)
            .unwrap()
            .unwrap();
        assert_eq!(graph.generation.get(), 1);
        assert_eq!(memory.generation.get(), 1);
        let pair = store
            .federated_view(
                &graph_request.repository,
                &graph_request.view_name,
                &memory_request.view_name,
            )
            .unwrap()
            .unwrap();
        assert_eq!(pair.graph.snapshot_id, graph.snapshot_id);
        assert_eq!(pair.memory.revision_id, memory.revision_id);
        assert!(
            store
                .graph_view(
                    &repository("tenant-b"),
                    &PublishedViewName::new("canonical").unwrap()
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn incompatible_or_missing_control_schema_fails_before_fact_tables() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing.db");
        assert!(matches!(
            SqliteRemotePublicationStore::open(&missing, [1; 32], limits(), true),
            Err(RemoteStoreError::MissingCoordinatorSchema)
        ));
        let connection = Connection::open(&missing).unwrap();
        assert!(connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'remote_encrypted_facts'",
                [],
                |_| Ok(())
            )
            .optional()
            .unwrap()
            .is_none());

        let incompatible = directory.path().join("incompatible.db");
        drop(coordinator(&incompatible));
        Connection::open(&incompatible)
            .unwrap()
            .execute(
                "UPDATE distributed_coordinator_metadata SET schema_version = 999",
                [],
            )
            .unwrap();
        assert!(matches!(
            SqliteRemotePublicationStore::open(&incompatible, [1; 32], limits(), true),
            Err(RemoteStoreError::IncompatibleSchema)
        ));

        let incompatible_storage = directory.path().join("incompatible-storage.db");
        drop(coordinator(&incompatible_storage));
        drop(store(&incompatible_storage));
        Connection::open(&incompatible_storage)
            .unwrap()
            .execute(
                "UPDATE remote_storage_metadata SET schema_version = 999",
                [],
            )
            .unwrap();
        assert!(matches!(
            SqliteRemotePublicationStore::open(&incompatible_storage, [1; 32], limits(), true),
            Err(RemoteStoreError::IncompatibleSchema)
        ));
    }
}
