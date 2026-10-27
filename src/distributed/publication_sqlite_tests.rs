//! Publication tests for immutable facts, pointer CAS, lease authority, and domain isolation.

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
        protocol::{
            IndexInputRef, IndexSemantics, RemoteMemoryLinkSetTarget, SubmitIndexJobRequest,
        },
    },
    project_memory::domain::{
        MemoryConfidence, MemoryEntityData, MemoryEvidenceLocator, MemoryExtractorId,
        MemoryExtractorIdentity, MemoryIndexTimestamps, MemoryProvenance, MemoryRecordId,
        MemoryRelationshipKind, MemoryRepositoryLinkSet, MemoryRepositoryLinkSetId,
        MemoryResolutionState, MemorySourceCategory, MemorySourceLocator, MemoryStatusToken,
        MemoryText, ProjectId, ProjectNamespace, ProjectRef,
    },
    repository_graph::domain::{
        Confidence, ExtractorId, ExtractorIdentity, FactProvenance, GraphValue, RepoPath,
        RepositoryId, RepositoryNamespace, RepositoryRef, ResolutionState, SemanticKey,
        SourceEvidence,
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

fn local_project() -> ProjectRef {
    ProjectRef {
        namespace: ProjectNamespace::new("remote:test").unwrap(),
        project_id: ProjectId::new("project").unwrap(),
    }
}

fn foreign_local_project() -> ProjectRef {
    ProjectRef {
        namespace: ProjectNamespace::new("remote:test").unwrap(),
        project_id: ProjectId::new("foreign").unwrap(),
    }
}

fn repository(tenant: &str) -> RemoteRepositoryRef {
    RemoteRepositoryRef {
        project: project(tenant),
        repository_id: RemoteRepositoryId::new("repository").unwrap(),
    }
}

fn local_repository() -> RepositoryRef {
    RepositoryRef {
        namespace: RepositoryNamespace::new("remote:test").unwrap(),
        repository_id: RepositoryId::new("root").unwrap(),
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

fn input(kind: IndexJobKind, tenant: &str, unique: &str, target: &str) -> IndexInputRef {
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
                repository_identity: local_repository(),
                manifest_id: RepositoryManifestId::new(identity.value()).unwrap(),
                manifest_digest: identity,
                source_policy_digest: digest("22"),
                expected_snapshot_id: SnapshotId::new(target).unwrap(),
                manifest_object: object,
            })
        }
        IndexJobKind::ProjectMemory => {
            IndexInputRef::Memory(super::super::identity::MemoryManifestRef {
                project,
                project_identity: local_project(),
                manifest_id: MemoryManifestId::new(identity.value()).unwrap(),
                manifest_digest: identity,
                memory_policy_digest: digest("22"),
                expected_revision_id: MemoryRevisionId::new(target).unwrap(),
                manifest_object: object,
                repository_snapshot: None,
                repository_origin_snapshots: Vec::new(),
            })
        }
    }
}

fn publishing_job(
    coordinator: &mut SqliteIndexJobCoordinator,
    kind: IndexJobKind,
    unique: &str,
    target: &str,
) -> super::super::protocol::IndexJobRecord {
    publishing_job_with_input(
        coordinator,
        kind,
        unique,
        input(kind, "tenant-a", unique, target),
    )
}

fn publishing_job_with_input(
    coordinator: &mut SqliteIndexJobCoordinator,
    kind: IndexJobKind,
    unique: &str,
    input: IndexInputRef,
) -> super::super::protocol::IndexJobRecord {
    let now = Utc::now();
    let spec = IndexJobSpec::new(
        kind,
        input,
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

fn graph_batch_with_path(job: &IndexJobRef, snapshot: &str, secret: &str, path: &str) -> FactBatch {
    graph_batch_with_options(job, snapshot, secret, true, Some(path))
}

fn graph_batch_with_final(
    job: &IndexJobRef,
    snapshot: &str,
    secret: &str,
    final_batch: bool,
) -> FactBatch {
    graph_batch_with_options(job, snapshot, secret, final_batch, None)
}

fn graph_batch_with_options(
    job: &IndexJobRef,
    snapshot: &str,
    secret: &str,
    final_batch: bool,
    path: Option<&str>,
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
            evidence: path.map(|path| SourceEvidence {
                path: RepoPath::new(path).unwrap(),
                content_identity: digest("55"),
                span: None,
            }),
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
            repository_identity: local_repository(),
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
    memory_batch_for_project(job, revision, local_project())
}

fn memory_batch_for_project(
    job: &IndexJobRef,
    revision: &str,
    project_identity: ProjectRef,
) -> FactBatch {
    FactBatch::new(
        job.clone(),
        FactTarget::ProjectMemory {
            revision: RemoteMemoryRevisionRef {
                project: project("tenant-a"),
                revision_id: MemoryRevisionId::new(revision).unwrap(),
            },
            project_identity,
            build_id: MemoryBuildId::new(format!("build-{revision}")).unwrap(),
            repository_links: None,
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

#[path = "publication_sqlite_tests/cancellation.rs"]
mod cancellation_tests;
#[path = "publication_sqlite_tests/publication.rs"]
mod publication_tests;
#[path = "publication_sqlite_tests/review.rs"]
mod review_tests;

#[test]
fn encrypted_fact_tampering_fails_closed() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("control.db");
    let mut coordinator = coordinator(&path);
    let job = publishing_job(
        &mut coordinator,
        IndexJobKind::RepositoryGraph,
        "1b",
        "snapshot-tampered",
    );
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

#[test]
fn completed_graph_and_memory_publications_replay_the_recorded_outcome() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("control.db");
    let mut coordinator = coordinator(&path);
    let graph_job = publishing_job(
        &mut coordinator,
        IndexJobKind::RepositoryGraph,
        "1101",
        "snapshot-replay",
    );
    let graph_request = graph_request(&graph_job, "snapshot-replay", None);
    let graph_facts = graph_batch(&graph_job.job, "snapshot-replay", "same");
    let mut store = store(&path);
    let graph_outcome = store
        .publish_graph(
            &graph_request,
            std::slice::from_ref(&graph_facts),
            Utc::now(),
        )
        .unwrap();
    let mut graph_retry = graph_request.clone();
    graph_retry.request_id = RequestId::new("publish-snapshot-replay-retry").unwrap();
    assert_eq!(
        store.publish_graph(&graph_retry, &[], Utc::now()).unwrap(),
        graph_outcome
    );

    let memory_job = publishing_job(
        &mut coordinator,
        IndexJobKind::ProjectMemory,
        "1102",
        "revision-replay",
    );
    let memory_request = memory_request(&memory_job, "revision-replay", None);
    let memory_facts = memory_batch(&memory_job.job, "revision-replay");
    let memory_outcome = store
        .publish_memory(
            &memory_request,
            std::slice::from_ref(&memory_facts),
            Utc::now(),
        )
        .unwrap();
    let mut memory_retry = memory_request.clone();
    memory_retry.request_id = RequestId::new("publish-revision-replay-retry").unwrap();
    assert_eq!(
        store
            .publish_memory(&memory_retry, &[], Utc::now())
            .unwrap(),
        memory_outcome
    );
}

#[test]
fn stale_memory_cas_target_is_retained_but_not_query_visible() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("control.db");
    let mut coordinator = coordinator(&path);
    let first_job = publishing_job(
        &mut coordinator,
        IndexJobKind::ProjectMemory,
        "14a",
        "revision-first",
    );
    let first_request = memory_request(&first_job, "revision-first", None);
    let mut store = store(&path);
    store
        .publish_memory(
            &first_request,
            &[memory_batch(&first_job.job, "revision-first")],
            Utc::now(),
        )
        .unwrap();

    let old_job = publishing_job(
        &mut coordinator,
        IndexJobKind::ProjectMemory,
        "14b",
        "revision-old",
    );
    let old_request = memory_request(&old_job, "revision-old", None);
    let outcome = store
        .publish_memory(
            &old_request,
            &[memory_batch(&old_job.job, "revision-old")],
            Utc::now(),
        )
        .unwrap();
    assert!(matches!(
        outcome,
        MemoryPublicationOutcome::Superseded { current: Some(ref view) }
            if view.revision_id == first_request.revision_id
    ));
    let old_revision = RemoteMemoryRevisionRef {
        project: old_request.project,
        revision_id: old_request.revision_id,
    };
    assert!(store.memory_revision(&old_revision).unwrap().is_some());
    assert!(
        store
            .published_memory_revision_bounded(
                &old_revision,
                Instant::now(),
                Duration::from_secs(1),
            )
            .unwrap()
            .is_none()
    );
}

#[test]
fn completed_superseded_retry_keeps_its_original_current_view() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("control.db");
    let mut coordinator = coordinator(&path);
    let first_job = publishing_job(
        &mut coordinator,
        IndexJobKind::RepositoryGraph,
        "1103",
        "snapshot-replay-first",
    );
    let first_request = graph_request(&first_job, "snapshot-replay-first", None);
    let mut store = store(&path);
    let first_outcome = store
        .publish_graph(
            &first_request,
            &[graph_batch(
                &first_job.job,
                "snapshot-replay-first",
                "first",
            )],
            Utc::now(),
        )
        .unwrap();
    let GraphPublicationOutcome::Published {
        view: first_view, ..
    } = first_outcome
    else {
        panic!("initial publication must win");
    };

    let loser_job = publishing_job(
        &mut coordinator,
        IndexJobKind::RepositoryGraph,
        "1104",
        "snapshot-replay-loser",
    );
    let loser_request = graph_request(&loser_job, "snapshot-replay-loser", None);
    let loser_batch = graph_batch(&loser_job.job, "snapshot-replay-loser", "loser");
    let loser_outcome = store
        .publish_graph(
            &loser_request,
            std::slice::from_ref(&loser_batch),
            Utc::now(),
        )
        .unwrap();
    assert!(matches!(
        &loser_outcome,
        GraphPublicationOutcome::Superseded { current: Some(view) }
            if view == &first_view
    ));

    let next_job = publishing_job(
        &mut coordinator,
        IndexJobKind::RepositoryGraph,
        "1105",
        "snapshot-replay-next",
    );
    let next_request = graph_request(
        &next_job,
        "snapshot-replay-next",
        Some(GraphPublicationVersion {
            snapshot_id: first_view.snapshot_id,
            generation: first_view.generation,
        }),
    );
    store
        .publish_graph(
            &next_request,
            &[graph_batch(&next_job.job, "snapshot-replay-next", "next")],
            Utc::now(),
        )
        .unwrap();

    let mut loser_retry = loser_request.clone();
    loser_retry.request_id = RequestId::new("publish-snapshot-replay-loser-retry").unwrap();
    assert_eq!(
        store.publish_graph(&loser_retry, &[], Utc::now()).unwrap(),
        loser_outcome
    );
    assert_eq!(
        store
            .graph_view(&next_request.repository, &next_request.view_name)
            .unwrap()
            .unwrap()
            .generation
            .get(),
        2
    );
}
