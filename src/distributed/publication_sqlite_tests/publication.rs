//! Tests for publication.

use super::*;

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
            repository_links: Some(Box::new(RemoteMemoryLinkSetTarget {
                graph,
                origin_graphs: Vec::new(),
                link_set,
            })),
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
    let origin_job = publishing_job(
        &mut coordinator,
        IndexJobKind::RepositoryGraph,
        "a0",
        "snapshot-origin",
    );
    let origin_request = graph_request(&origin_job, "snapshot-origin", None);
    let origin_batch = graph_batch_with_path(
        &origin_job.job,
        "snapshot-origin",
        "deleted repository node",
        "src/deleted.rs",
    );
    let mut store = store(&path);
    let origin_version = match store
        .publish_graph(&origin_request, &[origin_batch], Utc::now())
        .unwrap()
    {
        GraphPublicationOutcome::Published { view, .. } => GraphPublicationVersion {
            snapshot_id: view.snapshot_id,
            generation: view.generation,
        },
        GraphPublicationOutcome::Superseded { .. } => panic!("first graph must publish"),
    };
    let origin_graph = RemoteGraphSnapshotRef {
        repository: repository("tenant-a"),
        snapshot_id: SnapshotId::new("snapshot-origin").unwrap(),
    };
    let graph_job = publishing_job(
        &mut coordinator,
        IndexJobKind::RepositoryGraph,
        "aa",
        "snapshot-linked",
    );
    let graph_request = graph_request(&graph_job, "snapshot-linked", Some(origin_version));
    let graph_batch = graph_batch(&graph_job.job, "snapshot-linked", "linked repository node");
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
    manifest.repository_origin_snapshots = vec![origin_graph.clone()];
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
        source: entity_id.clone(),
        target: MemoryRelationshipTarget::RepositoryNode {
            repository: local_repository(),
            snapshot_id: graph.snapshot_id.clone(),
            node_id: NodeId::new("node-snapshot-linked").unwrap(),
            semantic_key: Some(SemanticKey::new("symbol-snapshot-linked").unwrap()),
        },
        provenance: provenance.clone(),
    };
    let mut stale_provenance = provenance;
    stale_provenance.resolution = MemoryResolutionState::Stale;
    let stale_relationship = MemoryRelationship {
        project: local_project(),
        memory_revision_id: revision_id.clone(),
        id: MemoryRelationshipId::new("stale-relationship").unwrap(),
        kind: MemoryRelationshipKind::Touches,
        source: entity_id,
        target: MemoryRelationshipTarget::RepositoryPath {
            repository: local_repository(),
            path: RepoPath::new("src/deleted.rs").unwrap(),
            snapshot_id: Some(origin_graph.snapshot_id.clone()),
        },
        provenance: stale_provenance,
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
                origin_graphs: vec![origin_graph.clone()],
                link_set,
            })),
        },
        FactShardId::new("memory-all").unwrap(),
        0,
        digest("44"),
        true,
        FactBatchPayload::ProjectMemory {
            entities: vec![entity],
            relationships: vec![relationship.clone(), stale_relationship.clone()],
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
    assert_eq!(links.target.origin_graphs, vec![origin_graph]);
    assert_eq!(links.relationships, vec![relationship, stale_relationship]);
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
        .inspect(&super::super::super::protocol::InspectIndexJobRequest {
            protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            request_id: RequestId::new("inspect-complete").unwrap(),
            job: job.job,
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        inspected.state,
        super::super::super::protocol::IndexJobState::Complete
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
