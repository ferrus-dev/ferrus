//! Tests for memory.

use super::*;

#[test]
fn memory_worker_extracts_only_sanitized_manifest_objects() {
    let content = b"# Example\n\nPrivate body.\n\n- [x] #5.3 Worker\n\nID: rg5.3\n".to_vec();
    let parsed = parse_spec_memory(std::str::from_utf8(&content).unwrap());
    let policy = MemoryPolicy::default();
    let descriptor = AuthorizedSourceDescriptor {
        project: local_project(),
        category: MemorySourceCategory::SpecificationStructure,
        locator: MemorySourceLocator::TrackedFile {
            path: RepoPath::new("docs/specs/example.md").unwrap(),
        },
        fingerprint: memory_digest(&parsed.structure).unwrap(),
        byte_len: content.len() as u64,
    };
    let mut local_manifest = AuthorizedSourceManifest {
        project: local_project(),
        policy_digest: policy.digest().unwrap(),
        source_set_digest: Digest::new("sha256", "11").unwrap(),
        extractor_set_digest: built_in_extractor_set_digest().unwrap(),
        sources: vec![descriptor],
    };
    local_manifest.source_set_digest = local_manifest.computed_source_set_digest().unwrap();
    let source = FakeMemorySource {
        manifest: local_manifest,
        contents: BTreeMap::from([(MemorySourceCategory::SpecificationStructure, content)]),
    };
    let storage_dir = tempfile::tempdir().unwrap();
    let mut objects = object_store(&storage_dir.path().join("objects"));
    let manifest = package_memory_source(
        &source,
        remote_project(),
        &policy,
        packaging_limits(),
        &mut objects,
    )
    .unwrap();
    let mut jobs = coordinator(&storage_dir.path().join("jobs.db"));
    let running = running_job(
        &mut jobs,
        IndexJobKind::ProjectMemory,
        IndexInputRef::Memory(manifest.reference.clone()),
        IndexSemantics {
            semantic_config_digest: policy.digest().unwrap(),
            model_version: NonZeroU32::new(MEMORY_MODEL_VERSION).unwrap(),
            extractor_set_digest: built_in_extractor_set_digest().unwrap(),
        },
    );
    let request = execution_request(running);
    let mut per_source_facts = fact_store(&storage_dir.path().join("memory-per-source-facts.db"));
    let mut per_source_limited = worker_limits();
    per_source_limited.max_facts_per_source = NonZeroU64::new(2).unwrap();
    assert_eq!(
        StatelessIndexWorker::new(per_source_limited).execute(
            &request,
            &jobs,
            &objects,
            &mut per_source_facts,
        ),
        Err(WorkerError::OutputLimitExceeded)
    );
    assert!(
        per_source_facts
            .load_for_ingestion(&request.job.job)
            .unwrap()
            .is_empty()
    );
    let mut total_facts = fact_store(&storage_dir.path().join("memory-total-facts.db"));
    let mut total_limited = worker_limits();
    total_limited.max_total_facts = NonZeroU64::new(2).unwrap();
    assert_eq!(
        StatelessIndexWorker::new(total_limited).execute(
            &request,
            &jobs,
            &objects,
            &mut total_facts,
        ),
        Err(WorkerError::OutputLimitExceeded)
    );
    assert!(
        total_facts
            .load_for_ingestion(&request.job.job)
            .unwrap()
            .is_empty()
    );
    let mut facts = fact_store(&storage_dir.path().join("facts.db"));
    let bounded_objects = BoundedReadTracker::new(&objects);
    let outcome = StatelessIndexWorker::new(worker_limits())
        .execute(&request, &jobs, &bounded_objects, &mut facts)
        .unwrap();
    assert_eq!(bounded_objects.reads.get(), manifest.body.sources.len() + 1);
    let encoded =
        serde_json::to_string(&facts.load_for_ingestion(&request.job.job).unwrap()).unwrap();
    assert!(outcome.emitted_facts > 0);
    assert!(encoded.contains("rg5.3"));
    assert!(!encoded.contains("Private body"));

    let mut singleton_limits = worker_limits();
    singleton_limits.max_facts_per_batch = NonZeroU64::new(1).unwrap();
    let mut singleton_facts = fact_store(&storage_dir.path().join("memory-singleton-facts.db"));
    StatelessIndexWorker::new(singleton_limits)
        .execute(&request, &jobs, &objects, &mut singleton_facts)
        .unwrap();
    let singleton_batches = singleton_facts
        .load_for_ingestion(&request.job.job)
        .unwrap();
    let batch_cap = max_encoded_batch_bytes(&singleton_batches);
    let mut byte_limited = worker_limits();
    byte_limited.max_facts_per_batch = NonZeroU64::new(100_000).unwrap();
    byte_limited.max_batch_bytes = batch_cap;
    let mut byte_sized_facts = fact_store(&storage_dir.path().join("memory-byte-sized-facts.db"));
    let byte_sized = StatelessIndexWorker::new(byte_limited)
        .execute(&request, &jobs, &objects, &mut byte_sized_facts)
        .unwrap();
    let byte_sized_batches = byte_sized_facts
        .load_for_ingestion(&request.job.job)
        .unwrap();
    assert_eq!(byte_sized.emitted_facts, outcome.emitted_facts);
    assert!(byte_sized_batches.len() > 1);
    assert!(
        byte_sized_batches
            .iter()
            .all(|batch| { serde_json::to_vec(batch).unwrap().len() as u64 <= batch_cap.get() })
    );

    let mut forged = manifest;
    forged.body.sources[0].source_fingerprint = Digest::new("sha256", "00").unwrap();
    let mut forged_sources = AuthorizedSourceManifest {
        project: forged.body.project_identity.clone(),
        policy_digest: forged.body.memory_policy_digest.clone(),
        source_set_digest: forged.body.source_set_digest.clone(),
        extractor_set_digest: forged.body.extractor_set_digest.clone(),
        sources: forged
            .body
            .sources
            .iter()
            .map(|source| AuthorizedSourceDescriptor {
                project: forged.body.project_identity.clone(),
                category: source.category,
                locator: source.locator.clone(),
                fingerprint: source.source_fingerprint.clone(),
                byte_len: source.sanitized_byte_len,
            })
            .collect(),
    };
    forged_sources.source_set_digest = forged_sources.computed_source_set_digest().unwrap();
    let expected_revision_id = forged_sources.revision_id().unwrap();
    forged.body.source_set_digest = forged_sources.source_set_digest;
    let encoded_manifest = serde_json::to_vec(&forged.body).unwrap();
    let manifest_digest = Digest::new(
        "sha256",
        Sha256::digest(&encoded_manifest)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
    )
    .unwrap();
    forged.reference.manifest_id = MemoryManifestId::new(manifest_digest.value()).unwrap();
    forged.reference.manifest_digest = manifest_digest.clone();
    forged.reference.expected_revision_id = expected_revision_id;
    forged.reference.manifest_object = objects
        .put_verified(
            &forged.reference.project,
            &manifest_digest,
            &encoded_manifest,
        )
        .unwrap()
        .object;
    forged.validate::<(), ()>().unwrap();

    let running = running_job(
        &mut jobs,
        IndexJobKind::ProjectMemory,
        IndexInputRef::Memory(forged.reference),
        IndexSemantics {
            semantic_config_digest: policy.digest().unwrap(),
            model_version: NonZeroU32::new(MEMORY_MODEL_VERSION).unwrap(),
            extractor_set_digest: built_in_extractor_set_digest().unwrap(),
        },
    );
    let forged_request = execution_request(running);
    let mut forged_facts = fact_store(&storage_dir.path().join("forged-memory-facts.db"));
    assert_eq!(
        StatelessIndexWorker::new(worker_limits()).execute(
            &forged_request,
            &jobs,
            &objects,
            &mut forged_facts,
        ),
        Err(WorkerError::InvalidInput)
    );
    assert!(
        forged_facts
            .load_for_ingestion(&forged_request.job.job)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn memory_worker_emits_a_snapshot_pinned_repository_link_set() {
    let content = b"# Example\n\n## Outcome\n\nDelivered.\n\n### Decisions\n\nUse `path:src/lib.rs` and `symbol:important-type`.\n".to_vec();
    let parsed = parse_spec_memory(std::str::from_utf8(&content).unwrap());
    let policy = MemoryPolicy::default();
    let baseline_snapshot_id = SnapshotId::new("baseline-snapshot").unwrap();
    let current_snapshot_id = SnapshotId::new("graph-snapshot").unwrap();
    let runtime = RuntimeSourceDocument {
        tasks: vec![RuntimeTaskDocument {
            id: "task-1".to_string(),
            milestone_id: None,
            status: "complete".to_string(),
            baseline_snapshot_id: Some(baseline_snapshot_id.clone()),
            repository_snapshot_id: Some(current_snapshot_id.clone()),
        }],
        runs: vec![RuntimeRunDocument {
            id: "run-1".to_string(),
            task_id: "task-1".to_string(),
            status: "completed".to_string(),
            check_ids: Vec::new(),
            baseline_snapshot_id: Some(baseline_snapshot_id.clone()),
            repository_snapshot_id: Some(current_snapshot_id.clone()),
        }],
        checks: Vec::new(),
    };
    let runtime_content = serde_json::to_vec(&runtime).unwrap();
    let descriptor = AuthorizedSourceDescriptor {
        project: local_project(),
        category: MemorySourceCategory::ApprovedOutcome,
        locator: MemorySourceLocator::TrackedFile {
            path: RepoPath::new("docs/specs/example.md").unwrap(),
        },
        fingerprint: memory_digest(parsed.outcome.as_ref().unwrap()).unwrap(),
        byte_len: content.len() as u64,
    };
    let runtime_descriptor = AuthorizedSourceDescriptor {
        project: local_project(),
        category: MemorySourceCategory::RuntimeProvenance,
        locator: MemorySourceLocator::RuntimeRecords {
            record_type: crate::project_memory::domain::MemoryStatusToken::new("terminal").unwrap(),
        },
        fingerprint: memory_digest(&runtime_content).unwrap(),
        byte_len: runtime_content.len() as u64,
    };
    let mut local_manifest = AuthorizedSourceManifest {
        project: local_project(),
        policy_digest: policy.digest().unwrap(),
        source_set_digest: Digest::new("sha256", "11").unwrap(),
        extractor_set_digest: built_in_extractor_set_digest().unwrap(),
        sources: vec![descriptor, runtime_descriptor],
    };
    local_manifest.source_set_digest = local_manifest.computed_source_set_digest().unwrap();
    let source = FakeMemorySource {
        manifest: local_manifest,
        contents: BTreeMap::from([
            (MemorySourceCategory::ApprovedOutcome, content),
            (MemorySourceCategory::RuntimeProvenance, runtime_content),
        ]),
    };
    let storage_dir = tempfile::tempdir().unwrap();
    let mut objects = object_store(&storage_dir.path().join("objects"));
    let mut manifest = package_memory_source(
        &source,
        remote_project(),
        &policy,
        packaging_limits(),
        &mut objects,
    )
    .unwrap();
    let graph_ref = RemoteGraphSnapshotRef {
        repository: remote_repository(),
        snapshot_id: current_snapshot_id.clone(),
    };
    let baseline_ref = RemoteGraphSnapshotRef {
        repository: remote_repository(),
        snapshot_id: baseline_snapshot_id.clone(),
    };
    manifest.reference.repository_snapshot = Some(graph_ref.clone());
    manifest.reference.repository_origin_snapshots = vec![baseline_ref.clone()];
    let graph = StoredRemoteGraphSnapshot {
        record: RemoteGraphSnapshotRecord {
            snapshot: graph_ref.clone(),
            repository_identity: local_repository(),
            job: IndexJobRef {
                project: remote_project(),
                job_id: crate::distributed::identity::IndexJobId::new("graph-job").unwrap(),
                kind: IndexJobKind::RepositoryGraph,
            },
            build_id: BuildId::new("graph-build").unwrap(),
            extractor_set_digest: Digest::new("sha256", "22").unwrap(),
            fact_set_digest: Digest::new("sha256", "33").unwrap(),
            counts: RemoteFactCounts {
                primary: 1,
                relationships: 0,
                diagnostics: 0,
            },
            completed_at: Utc::now(),
        },
        nodes: vec![GraphNode {
            snapshot_id: graph_ref.snapshot_id.clone(),
            id: NodeId::new("important-node").unwrap(),
            kind: "symbol".to_string(),
            semantic_key: Some(SemanticKey::new("important-type").unwrap()),
            provenance: FactProvenance {
                extractor: ExtractorIdentity {
                    id: ExtractorId::new("test.graph").unwrap(),
                    version: "1".to_string(),
                    contract_version: 1,
                },
                evidence: Some(SourceEvidence {
                    path: RepoPath::new("src/lib.rs").unwrap(),
                    content_identity: Digest::new("sha256", "44").unwrap(),
                    span: None,
                }),
                resolution: ResolutionState::Resolved,
                confidence: Confidence::Exact,
            },
            properties: BTreeMap::new(),
        }],
        edges: Vec::new(),
        diagnostics: Vec::new(),
    };
    let mut baseline_graph = graph.clone();
    baseline_graph.record.snapshot = baseline_ref.clone();
    baseline_graph.record.job.job_id =
        crate::distributed::identity::IndexJobId::new("baseline-graph-job").unwrap();
    baseline_graph.record.build_id = BuildId::new("baseline-graph-build").unwrap();
    baseline_graph.nodes[0].snapshot_id = baseline_snapshot_id.clone();
    baseline_graph.nodes[0].id = NodeId::new("baseline-node").unwrap();
    baseline_graph.nodes[0]
        .provenance
        .evidence
        .as_mut()
        .unwrap()
        .content_identity = Digest::new("sha256", "43").unwrap();
    let mut deleted_node = baseline_graph.nodes[0].clone();
    deleted_node.id = NodeId::new("deleted-node").unwrap();
    deleted_node.semantic_key = None;
    deleted_node.provenance.evidence.as_mut().unwrap().path =
        RepoPath::new("src/deleted.rs").unwrap();
    deleted_node
        .provenance
        .evidence
        .as_mut()
        .unwrap()
        .content_identity = Digest::new("sha256", "45").unwrap();
    baseline_graph.nodes.push(deleted_node);
    baseline_graph.record.counts.primary = 2;
    let mut jobs = coordinator(&storage_dir.path().join("jobs.db"));
    let running = running_job(
        &mut jobs,
        IndexJobKind::ProjectMemory,
        IndexInputRef::Memory(manifest.reference.clone()),
        IndexSemantics {
            semantic_config_digest: policy.digest().unwrap(),
            model_version: NonZeroU32::new(MEMORY_MODEL_VERSION).unwrap(),
            extractor_set_digest: built_in_extractor_set_digest().unwrap(),
        },
    );
    let request = execution_request(running);
    let mut missing_origin_facts =
        fact_store(&storage_dir.path().join("missing-origin-linked-facts.db"));
    assert_eq!(
        StatelessIndexWorker::new(worker_limits()).execute_with_repository_snapshots(
            &request,
            &jobs,
            &objects,
            &mut missing_origin_facts,
            Some(&graph),
            &[],
        ),
        Err(WorkerError::InvalidInput)
    );
    let mut facts = fact_store(&storage_dir.path().join("linked-facts.db"));

    StatelessIndexWorker::new(worker_limits())
        .execute_with_repository_snapshots(
            &request,
            &jobs,
            &objects,
            &mut facts,
            Some(&graph),
            &[baseline_graph],
        )
        .unwrap();

    let batches = facts.load_for_ingestion(&request.job.job).unwrap();
    assert!(batches.iter().all(|batch| {
        matches!(
            &batch.header.target,
            FactTarget::ProjectMemory {
                repository_links: Some(target),
                ..
            } if target.graph == graph_ref
                && target.origin_graphs == vec![baseline_ref.clone()]
                && target.link_set.repository_snapshot_id.as_ref() == Some(&graph_ref.snapshot_id)
        )
    }));
    let relationships = batches
        .iter()
        .flat_map(|batch| match &batch.payload {
            FactBatchPayload::ProjectMemory { relationships, .. } => relationships.as_slice(),
            FactBatchPayload::RepositoryGraph { .. } => &[],
        })
        .collect::<Vec<_>>();
    assert!(relationships.iter().any(|relationship| matches!(
        relationship.target,
        MemoryRelationshipTarget::RepositoryPath {
            snapshot_id: Some(ref snapshot_id),
            ..
        } if snapshot_id == &graph_ref.snapshot_id
    )));
    assert!(relationships.iter().any(|relationship| matches!(
        relationship.target,
        MemoryRelationshipTarget::RepositoryNode {
            ref snapshot_id,
            ref node_id,
            ref semantic_key,
            ..
        } if snapshot_id == &graph_ref.snapshot_id && node_id.as_str() == "important-node"
            && semantic_key.as_ref().is_some_and(|key| key.as_str() == "important-type")
    )));
    assert_eq!(
        relationships
            .iter()
            .filter(|relationship| {
                relationship.kind == crate::project_memory::domain::MemoryRelationshipKind::Touches
                    && relationship.provenance.source_category
                        == MemorySourceCategory::RuntimeProvenance
                    && matches!(
                        &relationship.target,
                        MemoryRelationshipTarget::RepositoryPath {
                            path,
                            snapshot_id: Some(snapshot_id),
                            ..
                        } if path.as_str() == "src/lib.rs"
                            && snapshot_id == &current_snapshot_id
                    )
            })
            .count(),
        2
    );
    assert_eq!(
        relationships
            .iter()
            .filter(|relationship| {
                relationship.kind == crate::project_memory::domain::MemoryRelationshipKind::Touches
                    && relationship.provenance.source_category
                        == MemorySourceCategory::RuntimeProvenance
                    && relationship.provenance.resolution == MemoryResolutionState::Stale
                    && matches!(
                        &relationship.target,
                        MemoryRelationshipTarget::RepositoryPath {
                            path,
                            snapshot_id: Some(snapshot_id),
                            ..
                        } if path.as_str() == "src/deleted.rs"
                            && snapshot_id == &baseline_snapshot_id
                    )
            })
            .count(),
        2
    );
}
