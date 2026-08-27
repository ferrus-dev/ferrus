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

#[test]
fn memory_publication_rejects_a_target_from_another_logical_project() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("control.db");
    let mut coordinator = coordinator(&path);
    let job = publishing_job(
        &mut coordinator,
        IndexJobKind::ProjectMemory,
        "19",
        "memory-project-revision",
    );
    let request = memory_request(&job, "memory-project-revision", None);
    let batch =
        memory_batch_for_project(&job.job, "memory-project-revision", foreign_local_project());

    assert!(matches!(
        store(&path).publish_memory(&request, &[batch], Utc::now()),
        Err(RemoteStoreError::InvalidInput)
    ));
}

#[test]
fn graph_publication_rejects_a_target_not_bound_to_the_job_input() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("control.db");
    let mut coordinator = coordinator(&path);
    let job = publishing_job(
        &mut coordinator,
        IndexJobKind::RepositoryGraph,
        "1e",
        "snapshot-expected",
    );
    let request = graph_request(&job, "snapshot-forged", None);
    let batch = graph_batch(&job.job, "snapshot-forged", "internally-consistent");
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
}

#[test]
fn memory_publication_rejects_a_target_not_bound_to_the_job_input() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("control.db");
    let mut coordinator = coordinator(&path);
    let job = publishing_job(
        &mut coordinator,
        IndexJobKind::ProjectMemory,
        "1f",
        "memory-expected",
    );
    let request = memory_request(&job, "memory-forged", None);
    let batch = memory_batch(&job.job, "memory-forged");
    let mut store = store(&path);

    assert!(matches!(
        store.publish_memory(&request, &[batch], Utc::now()),
        Err(RemoteStoreError::InvalidInput)
    ));
    assert!(
        store
            .memory_view(&request.project, &request.view_name)
            .unwrap()
            .is_none()
    );
}

#[test]
fn memory_publication_checks_authority_before_loading_repository_links() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("control.db");
    let mut coordinator = coordinator(&path);
    let job = publishing_job(
        &mut coordinator,
        IndexJobKind::ProjectMemory,
        "20",
        "memory-authority-first",
    );
    let revision_id = MemoryRevisionId::new("memory-authority-first").unwrap();
    let graph = RemoteGraphSnapshotRef {
        repository: repository("tenant-a"),
        snapshot_id: SnapshotId::new("missing-graph-snapshot").unwrap(),
    };
    let link_set = MemoryRepositoryLinkSet {
        id: MemoryRepositoryLinkSetId::new("memory-links:authority-first").unwrap(),
        project: local_project(),
        memory_revision_id: revision_id.clone(),
        repository: local_repository(),
        repository_snapshot_id: Some(graph.snapshot_id.clone()),
        resolver: MemoryExtractorIdentity::current(
            MemoryExtractorId::new("memory.test").unwrap(),
            MemoryStatusToken::new("v1").unwrap(),
        ),
    };
    let batch = FactBatch::new(
        job.job.clone(),
        FactTarget::ProjectMemory {
            revision: RemoteMemoryRevisionRef {
                project: project("tenant-a"),
                revision_id: revision_id.clone(),
            },
            project_identity: local_project(),
            build_id: MemoryBuildId::new("build-memory-authority-first").unwrap(),
            repository_links: Some(Box::new(RemoteMemoryLinkSetTarget { graph, link_set })),
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
    .unwrap();
    let mut request = memory_request(&job, revision_id.as_str(), None);
    request.worker_id = WorkerId::new("forged-worker").unwrap();

    assert!(matches!(
        store(&path).publish_memory(&request, &[batch], Utc::now()),
        Err(RemoteStoreError::AuthorityLost)
    ));
}

#[test]
fn memory_publication_keeps_repository_links_in_an_exact_federated_link_set() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("control.db");
    let mut coordinator = coordinator(&path);
    let graph_job = publishing_job(
        &mut coordinator,
        IndexJobKind::RepositoryGraph,
        "aa",
        "snapshot-linked",
    );
    let graph_request = graph_request(&graph_job, "snapshot-linked", None);
    let graph_batch = graph_batch(&graph_job.job, "snapshot-linked", "linked repository node");
    let mut store = store(&path);
    store
        .publish_graph(&graph_request, &[graph_batch], Utc::now())
        .unwrap();
    let graph = RemoteGraphSnapshotRef {
        repository: repository("tenant-a"),
        snapshot_id: SnapshotId::new("snapshot-linked").unwrap(),
    };

    let mut memory_input = input(
        IndexJobKind::ProjectMemory,
        "tenant-a",
        "bb",
        "memory-linked",
    );
    let IndexInputRef::Memory(manifest) = &mut memory_input else {
        unreachable!();
    };
    manifest.repository_snapshot = Some(graph.clone());
    let memory_job = publishing_job_with_input(
        &mut coordinator,
        IndexJobKind::ProjectMemory,
        "bb",
        memory_input,
    );
    let revision_id = MemoryRevisionId::new("memory-linked").unwrap();
    let entity_id = MemoryEntityId::new("linked-memory-entity").unwrap();
    let now = Utc::now();
    let provenance = MemoryProvenance {
        source_category: MemorySourceCategory::ApprovedOutcome,
        source_locator: MemorySourceLocator::TrackedFile {
            path: RepoPath::new("docs/spec.md").unwrap(),
        },
        source_fingerprint: digest("77"),
        extractor: MemoryExtractorIdentity::current(
            MemoryExtractorId::new("memory.test").unwrap(),
            MemoryStatusToken::new("v1").unwrap(),
        ),
        evidence: MemoryEvidenceLocator::Record(MemoryRecordId::new("outcome").unwrap()),
        resolution: MemoryResolutionState::Resolved,
        confidence: MemoryConfidence::Exact,
        timestamps: MemoryIndexTimestamps {
            source_observed_at: now,
            indexed_at: now,
        },
    };
    let entity = MemoryEntity {
        project: local_project(),
        memory_revision_id: revision_id.clone(),
        id: entity_id.clone(),
        data: MemoryEntityData::Outcome {
            text: MemoryText::new("Linked outcome").unwrap(),
        },
        provenance: provenance.clone(),
    };
    let link_set = MemoryRepositoryLinkSet {
        id: MemoryRepositoryLinkSetId::new("memory-links:linked").unwrap(),
        project: local_project(),
        memory_revision_id: revision_id.clone(),
        repository: local_repository(),
        repository_snapshot_id: Some(graph.snapshot_id.clone()),
        resolver: provenance.extractor.clone(),
    };
    let relationship = MemoryRelationship {
        project: local_project(),
        memory_revision_id: revision_id.clone(),
        id: MemoryRelationshipId::new("linked-relationship").unwrap(),
        kind: MemoryRelationshipKind::Touches,
        source: entity_id,
        target: MemoryRelationshipTarget::RepositoryNode {
            repository: local_repository(),
            snapshot_id: graph.snapshot_id.clone(),
            node_id: NodeId::new("node-snapshot-linked").unwrap(),
        },
        provenance,
    };
    let batch = FactBatch::new(
        memory_job.job.clone(),
        FactTarget::ProjectMemory {
            revision: RemoteMemoryRevisionRef {
                project: project("tenant-a"),
                revision_id: revision_id.clone(),
            },
            project_identity: local_project(),
            build_id: MemoryBuildId::new("build-memory-linked").unwrap(),
            repository_links: Some(Box::new(RemoteMemoryLinkSetTarget {
                graph: graph.clone(),
                link_set,
            })),
        },
        FactShardId::new("memory-all").unwrap(),
        0,
        digest("44"),
        true,
        FactBatchPayload::ProjectMemory {
            entities: vec![entity],
            relationships: vec![relationship.clone()],
            diagnostics: Vec::new(),
        },
    )
    .unwrap();
    let request = memory_request(&memory_job, "memory-linked", None);
    store
        .publish_memory(&request, &[batch], Utc::now())
        .unwrap();

    let memory_ref = RemoteMemoryRevisionRef {
        project: project("tenant-a"),
        revision_id,
    };
    let memory = store.memory_revision(&memory_ref).unwrap().unwrap();
    assert!(memory.relationships.is_empty());
    let federated = FederatedViewRef::new(graph, memory_ref).unwrap();
    let links = store.memory_repository_links(&federated).unwrap().unwrap();
    assert_eq!(links.relationships, vec![relationship]);
}

#[test]
fn graph_publication_is_atomic_encrypted_and_completes_the_job() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("control.db");
    let mut coordinator = coordinator(&path);
    let job = publishing_job(
        &mut coordinator,
        IndexJobKind::RepositoryGraph,
        "11",
        "snapshot-one",
    );
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

#[test]
fn project_deletion_tombstone_rejects_publication() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("control.db");
    let mut coordinator = coordinator(&path);
    let job = publishing_job(
        &mut coordinator,
        IndexJobKind::RepositoryGraph,
        "1d",
        "snapshot-deleted",
    );
    let request = graph_request(&job, "snapshot-deleted", None);
    let batch = graph_batch(&job.job, "snapshot-deleted", "private-symbol-name");
    let mut store = store(&path);
    store
        .connection
        .execute(
            "INSERT INTO project_deletion_tombstones (
                 tenant_id, project_id, deletion_id, created_at_ms
             ) VALUES (?1, ?2, 'delete-project', ?3)",
            params![
                request.job.project.tenant_id.as_str(),
                request.job.project.project_id.as_str(),
                Utc::now().timestamp_millis()
            ],
        )
        .unwrap();

    assert!(matches!(
        store.publish_graph(&request, &[batch], Utc::now()),
        Err(RemoteStoreError::ProjectDeleted)
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
fn partial_stream_and_cancelled_publication_remain_invisible() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("control.db");
    let mut coordinator = coordinator(&path);
    let job = publishing_job(
        &mut coordinator,
        IndexJobKind::RepositoryGraph,
        "12",
        "snapshot-partial",
    );
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
    let first_job = publishing_job(
        &mut coordinator,
        IndexJobKind::RepositoryGraph,
        "13",
        "snapshot-first",
    );
    let first_request = graph_request(&first_job, "snapshot-first", None);
    let mut store = store(&path);
    store
        .publish_graph(
            &first_request,
            &[graph_batch(&first_job.job, "snapshot-first", "one")],
            Utc::now(),
        )
        .unwrap();

    let old_job = publishing_job(
        &mut coordinator,
        IndexJobKind::RepositoryGraph,
        "14",
        "snapshot-old",
    );
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
                snapshot_id: old_request.snapshot_id.clone(),
            })
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .published_graph_snapshot_bounded(
                &RemoteGraphSnapshotRef {
                    repository: old_request.repository.clone(),
                    snapshot_id: old_request.snapshot_id,
                },
                Instant::now(),
                Duration::from_secs(1),
            )
            .unwrap()
            .is_none()
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
fn concurrent_graph_publishers_with_one_expectation_choose_one_winner() {
    use std::sync::{Arc, Barrier};

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("control.db");
    let mut coordinator = coordinator(&path);
    let left_job = publishing_job(
        &mut coordinator,
        IndexJobKind::RepositoryGraph,
        "17",
        "snapshot-left",
    );
    let right_job = publishing_job(
        &mut coordinator,
        IndexJobKind::RepositoryGraph,
        "18",
        "snapshot-right",
    );
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
    let first_job = publishing_job(
        &mut coordinator,
        IndexJobKind::RepositoryGraph,
        "19",
        "snapshot-reused",
    );
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

    let retry_job = publishing_job(
        &mut coordinator,
        IndexJobKind::RepositoryGraph,
        "1a",
        "snapshot-reused",
    );
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
fn previously_published_snapshot_remains_query_visible_after_view_advances() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("control.db");
    let mut coordinator = coordinator(&path);
    let first_job = publishing_job(
        &mut coordinator,
        IndexJobKind::RepositoryGraph,
        "19a",
        "snapshot-history-first",
    );
    let first_request = graph_request(&first_job, "snapshot-history-first", None);
    let mut store = store(&path);
    let first_outcome = store
        .publish_graph(
            &first_request,
            &[graph_batch(
                &first_job.job,
                "snapshot-history-first",
                "first",
            )],
            Utc::now(),
        )
        .unwrap();
    let GraphPublicationOutcome::Published { view, .. } = first_outcome else {
        panic!("initial publication must win");
    };

    let next_job = publishing_job(
        &mut coordinator,
        IndexJobKind::RepositoryGraph,
        "19b",
        "snapshot-history-next",
    );
    let next_request = graph_request(
        &next_job,
        "snapshot-history-next",
        Some(GraphPublicationVersion {
            snapshot_id: view.snapshot_id,
            generation: view.generation,
        }),
    );
    store
        .publish_graph(
            &next_request,
            &[graph_batch(&next_job.job, "snapshot-history-next", "next")],
            Utc::now(),
        )
        .unwrap();

    assert!(
        store
            .published_graph_snapshot_bounded(
                &RemoteGraphSnapshotRef {
                    repository: first_request.repository,
                    snapshot_id: first_request.snapshot_id,
                },
                Instant::now(),
                Duration::from_secs(1),
            )
            .unwrap()
            .is_some()
    );
}

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
fn bounded_snapshot_reads_stop_after_the_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("control.db");
    let mut coordinator = coordinator(&path);
    let job = publishing_job(
        &mut coordinator,
        IndexJobKind::RepositoryGraph,
        "1c",
        "snapshot-deadline",
    );
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
        store.published_graph_snapshot_bounded(
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
    let graph_job = publishing_job(
        &mut coordinator,
        IndexJobKind::RepositoryGraph,
        "15",
        "snapshot-pair",
    );
    let graph_request = graph_request(&graph_job, "snapshot-pair", None);
    let mut store = store(&path);
    store
        .publish_graph(
            &graph_request,
            &[graph_batch(&graph_job.job, "snapshot-pair", "pair")],
            Utc::now(),
        )
        .unwrap();
    let memory_job = publishing_job(
        &mut coordinator,
        IndexJobKind::ProjectMemory,
        "16",
        "revision-pair",
    );
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
