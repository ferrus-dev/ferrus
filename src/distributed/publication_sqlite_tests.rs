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
        Confidence, ExtractorId, ExtractorIdentity, FactProvenance, GraphValue, ResolutionState,
        SemanticKey,
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
