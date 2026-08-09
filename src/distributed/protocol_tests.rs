use super::*;
use crate::distributed::identity::{
    MemoryManifestId, ObjectId, RemoteProjectId, RemoteRepositoryId, RepositoryManifestId,
    TenantId, TenantObjectRef,
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

fn repository_input(tenant: &str) -> IndexInputRef {
    IndexInputRef::Repository(RepositoryManifestRef {
        repository: repository(tenant),
        manifest_id: RepositoryManifestId::new("manifest").unwrap(),
        manifest_digest: digest("11"),
        source_policy_digest: digest("22"),
        manifest_object: TenantObjectRef {
            project: project(tenant),
            object_id: ObjectId::new("11").unwrap(),
            content_identity: digest("11"),
        },
    })
}

fn repository(tenant: &str) -> RemoteRepositoryRef {
    RemoteRepositoryRef {
        project: project(tenant),
        repository_id: RemoteRepositoryId::new("repo").unwrap(),
    }
}

fn graph_job(tenant: &str) -> IndexJobRef {
    IndexJobRef {
        project: project(tenant),
        job_id: IndexJobId::new("job").unwrap(),
        kind: IndexJobKind::RepositoryGraph,
    }
}

fn graph_target(tenant: &str) -> FactTarget {
    FactTarget::RepositoryGraph {
        snapshot: RemoteGraphSnapshotRef {
            repository: repository(tenant),
            snapshot_id: SnapshotId::new("snapshot").unwrap(),
        },
        build_id: BuildId::new("build").unwrap(),
    }
}

fn memory_input(tenant: &str) -> IndexInputRef {
    IndexInputRef::Memory(MemoryManifestRef {
        project: project(tenant),
        manifest_id: MemoryManifestId::new("manifest").unwrap(),
        manifest_digest: digest("11"),
        memory_policy_digest: digest("22"),
        manifest_object: TenantObjectRef {
            project: project(tenant),
            object_id: ObjectId::new("11").unwrap(),
            content_identity: digest("11"),
        },
    })
}

fn semantics() -> IndexSemantics {
    IndexSemantics {
        semantic_config_digest: digest("33"),
        model_version: NonZeroU32::new(1).unwrap(),
        extractor_set_digest: digest("44"),
    }
}

#[test]
fn idempotency_is_deterministic_kind_specific_and_tenant_scoped() {
    let first = IndexJobSpec::new(
        IndexJobKind::RepositoryGraph,
        repository_input("tenant-a"),
        semantics(),
    )
    .unwrap();
    let repeated = IndexJobSpec::new(
        IndexJobKind::RepositoryGraph,
        repository_input("tenant-a"),
        semantics(),
    )
    .unwrap();
    let foreign = IndexJobSpec::new(
        IndexJobKind::RepositoryGraph,
        repository_input("tenant-b"),
        semantics(),
    )
    .unwrap();
    let memory = IndexJobSpec::new(
        IndexJobKind::ProjectMemory,
        memory_input("tenant-a"),
        semantics(),
    )
    .unwrap();
    assert_eq!(first.idempotency_key, repeated.idempotency_key);
    assert_ne!(first.idempotency_key, foreign.idempotency_key);
    assert_ne!(first.idempotency_key, memory.idempotency_key);
    assert!(first.validate().is_ok());
}

#[test]
fn mismatched_job_kinds_and_modified_keys_fail_closed() {
    assert_eq!(
        IndexJobSpec::new(
            IndexJobKind::ProjectMemory,
            repository_input("tenant-a"),
            semantics(),
        ),
        Err(DistributedProtocolError::JobInputMismatch)
    );
    let mut spec = IndexJobSpec::new(
        IndexJobKind::RepositoryGraph,
        repository_input("tenant-a"),
        semantics(),
    )
    .unwrap();
    spec.idempotency_key = digest("00");
    assert_eq!(
        spec.validate(),
        Err(DistributedProtocolError::IdempotencyMismatch)
    );
}

#[test]
fn job_state_machine_is_reclaimable_but_terminal_states_are_final() {
    assert!(IndexJobState::Queued.can_transition_to(IndexJobState::Leased));
    assert!(IndexJobState::Running.can_transition_to(IndexJobState::Queued));
    assert!(IndexJobState::Publishing.can_transition_to(IndexJobState::Complete));
    assert!(IndexJobState::Publishing.can_transition_to(IndexJobState::Cancelled));
    assert!(!IndexJobState::Complete.can_transition_to(IndexJobState::Queued));
    assert!(IndexJobState::Cancelled.is_terminal());
}

#[test]
fn fact_batches_are_idempotent_and_detect_tampering() {
    let payload = FactBatchPayload::RepositoryGraph {
        nodes: Vec::new(),
        edges: Vec::new(),
        diagnostics: Vec::new(),
    };
    let first = FactBatch::new(
        graph_job("tenant-a"),
        graph_target("tenant-a"),
        FactShardId::new("shard").unwrap(),
        0,
        digest("44"),
        true,
        payload.clone(),
    )
    .unwrap();
    let repeated = FactBatch::new(
        graph_job("tenant-a"),
        graph_target("tenant-a"),
        FactShardId::new("shard").unwrap(),
        0,
        digest("44"),
        true,
        payload,
    )
    .unwrap();
    assert_eq!(first.header.batch_id, repeated.header.batch_id);
    let mut tampered = first;
    tampered.header.payload_digest = digest("00");
    assert_eq!(
        tampered.validate(),
        Err(DistributedProtocolError::FactBatchIdentityMismatch)
    );
}

#[test]
fn graph_and_memory_publication_contracts_cannot_cross_domains() {
    let valid = PublishGraphRequest {
        protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        request_id: RequestId::new("publish").unwrap(),
        job: graph_job("tenant-a"),
        worker_id: WorkerId::new("worker").unwrap(),
        lease_generation: NonZeroU64::new(1).unwrap(),
        repository: repository("tenant-a"),
        view_name: PublishedViewName::new("canonical").unwrap(),
        snapshot_id: SnapshotId::new("snapshot").unwrap(),
        expected: None,
    };
    assert!(valid.validate().is_ok());
    let mut foreign = valid.clone();
    foreign.repository = repository("tenant-b");
    assert_eq!(
        foreign.validate(),
        Err(DistributedProtocolError::PublicationMismatch)
    );

    let memory = PublishMemoryRequest {
        protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        request_id: RequestId::new("publish-memory").unwrap(),
        job: graph_job("tenant-a"),
        worker_id: WorkerId::new("worker").unwrap(),
        lease_generation: NonZeroU64::new(1).unwrap(),
        project: project("tenant-a"),
        view_name: MemoryViewName::new("canonical").unwrap(),
        revision_id: MemoryRevisionId::new("revision").unwrap(),
        expected: None,
    };
    assert_eq!(
        memory.validate(),
        Err(DistributedProtocolError::PublicationMismatch)
    );
}

#[test]
fn query_envelopes_reject_scope_and_version_mismatches() {
    let repository = RemoteRepositoryRef {
        project: project("tenant-a"),
        repository_id: RemoteRepositoryId::new("repo").unwrap(),
    };
    let mut request = RemoteQueryRequest {
        protocol_version: DISTRIBUTED_QUERY_PROTOCOL_VERSION,
        request_id: RequestId::new("request").unwrap(),
        project: project("tenant-b"),
        target: RemoteQueryTarget::Repository(RemoteGraphSnapshotRef {
            repository,
            snapshot_id: SnapshotId::new("snapshot").unwrap(),
        }),
        body: (),
    };
    assert_eq!(
        request.validate(),
        Err(DistributedProtocolError::QueryScopeMismatch)
    );
    request.project = project("tenant-a");
    request.protocol_version += 1;
    assert_eq!(
        request.validate(),
        Err(DistributedProtocolError::UnsupportedVersion)
    );
}

#[test]
fn query_envelopes_reject_cross_project_federated_targets() {
    let memory_project = project("tenant-a");
    let foreign_repository = RemoteRepositoryRef {
        project: RemoteProjectRef {
            tenant_id: memory_project.tenant_id.clone(),
            project_id: RemoteProjectId::new("foreign-project").unwrap(),
        },
        repository_id: RemoteRepositoryId::new("repo").unwrap(),
    };
    let request = RemoteQueryRequest {
        protocol_version: DISTRIBUTED_QUERY_PROTOCOL_VERSION,
        request_id: RequestId::new("federated-request").unwrap(),
        project: memory_project.clone(),
        target: RemoteQueryTarget::Federated(FederatedViewRef {
            graph: RemoteGraphSnapshotRef {
                repository: foreign_repository,
                snapshot_id: SnapshotId::new("snapshot").unwrap(),
            },
            memory: RemoteMemoryRevisionRef {
                project: memory_project,
                revision_id: MemoryRevisionId::new("revision").unwrap(),
            },
        }),
        body: (),
    };

    assert_eq!(
        request.validate(),
        Err(DistributedProtocolError::QueryScopeMismatch)
    );
}
