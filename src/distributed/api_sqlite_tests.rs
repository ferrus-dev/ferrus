use std::num::{NonZeroU32, NonZeroU64};

use super::*;
use crate::{
    distributed::{
        DISTRIBUTED_FACT_PROTOCOL_VERSION, DISTRIBUTED_SOURCE_MANIFEST_VERSION,
        api::{RemotePageRequest, RemoteQueryBudget},
        coordinator::{AdvanceIndexJobRequest, ClaimIndexJobRequest},
        coordinator_sqlite::CoordinatorLimits,
        identity::{
            CredentialId, FactShardId, FederatedViewRef, IndexJobId, MemoryManifestId, PrincipalId,
            RemoteProjectId, RemoteRepositoryId, RemoteRepositoryRef, RepositoryManifestId,
            TenantId,
        },
        object_store::{ObjectStoreProtection, ObjectStoreQuota},
        protocol::{
            FactBatch, FactBatchPayload, FactTarget, IndexJobKind, IndexJobSpec, IndexSemantics,
            SubmitIndexJobRequest,
        },
        publication::RemotePublicationStore,
        publication_sqlite::RemoteStoreLimits,
        security::{AuthorizationScope, CredentialClass},
        source::{
            MemorySourceObject, PackagingSummary, REMOTE_REPOSITORY_SOURCE_POLICY_VERSION,
            RepositoryFileRole, RepositorySourceObject,
        },
    },
    project_memory::{
        domain::{
            MemoryBuildId, MemoryConfidence, MemoryEntityData, MemoryEntityId, MemoryExtractorId,
            MemoryExtractorIdentity, MemoryIndexTimestamps, MemoryProvenance, MemoryRelationshipId,
            MemoryRelationshipKind, MemoryResolutionState, MemoryRevisionId, MemorySourceCategory,
            MemorySourceLocator, MemoryStatusToken, MemoryText, ProjectId, ProjectNamespace,
            ProjectRef,
        },
        policy::{MEMORY_POLICY_SCHEMA_VERSION, MemoryContentAccess, MemorySourceSensitivity},
    },
    repository_graph::{
        domain::{
            BuildId, Confidence, Digest, EdgeId, EdgeTarget, ExtractorId, ExtractorIdentity,
            FactProvenance, GraphEdge, GraphNode, PublishedViewName, RepoPath, RepositoryId,
            RepositoryNamespace, RepositoryRef, ResolutionState, SemanticKey, SnapshotId,
            SourceEvidence, SourceKind, SourceRevision, SourceRevisionId,
        },
        ports::SourceFileMode,
    },
};

const KEY: [u8; 32] = [71; 32];

struct Fixture {
    directory: tempfile::TempDir,
    control_path: std::path::PathBuf,
    object_root: std::path::PathBuf,
    query: SqliteRemoteQueryApi,
    project: RemoteProjectRef,
    repository: RemoteRepositoryRef,
    graph: RemoteGraphSnapshotRef,
    memory: RemoteMemoryRevisionRef,
    graph_node: NodeId,
    source_text: String,
}

fn digest(value: &str) -> Digest {
    Digest::new("sha256", value).unwrap()
}

fn sha256(bytes: &[u8]) -> Digest {
    let value = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Digest::new("sha256", value).unwrap()
}

fn project() -> RemoteProjectRef {
    RemoteProjectRef {
        tenant_id: TenantId::new("tenant-a").unwrap(),
        project_id: RemoteProjectId::new("project").unwrap(),
    }
}

fn repository() -> RemoteRepositoryRef {
    RemoteRepositoryRef {
        project: project(),
        repository_id: RemoteRepositoryId::new("repository").unwrap(),
    }
}

fn local_repository() -> RepositoryRef {
    RepositoryRef {
        namespace: RepositoryNamespace::new("remote:test").unwrap(),
        repository_id: RepositoryId::new("root").unwrap(),
    }
}

fn local_project() -> ProjectRef {
    ProjectRef {
        namespace: ProjectNamespace::new("remote:test").unwrap(),
        project_id: ProjectId::new("project").unwrap(),
    }
}

fn coordinator(path: &std::path::Path) -> SqliteIndexJobCoordinator {
    SqliteIndexJobCoordinator::open(
        path,
        CoordinatorLimits {
            max_attempts: NonZeroU32::new(3).unwrap(),
            lease_ttl_ms: NonZeroU64::new(60_000).unwrap(),
            max_job_duration_ms: NonZeroU64::new(120_000).unwrap(),
        },
    )
    .unwrap()
}

fn object_quota() -> ObjectStoreQuota {
    ObjectStoreQuota {
        max_objects_per_project: NonZeroU64::new(100).unwrap(),
        max_bytes_per_project: NonZeroU64::new(16 * 1024 * 1024).unwrap(),
        max_object_bytes: NonZeroU64::new(1024 * 1024).unwrap(),
    }
}

fn object_store(root: &std::path::Path) -> EncryptedFilesystemObjectStore {
    let store = EncryptedFilesystemObjectStore::open(root, KEY, object_quota(), true).unwrap();
    assert_eq!(
        store.protection(),
        ObjectStoreProtection {
            authenticated_transport: true,
            encrypted_at_rest: true,
        }
    );
    store
}

fn publication_limits() -> RemoteStoreLimits {
    RemoteStoreLimits {
        max_snapshots_per_project: NonZeroU64::new(100).unwrap(),
        max_facts_per_project: NonZeroU64::new(10_000).unwrap(),
        max_bytes_per_project: NonZeroU64::new(16 * 1024 * 1024).unwrap(),
        max_facts_per_snapshot: NonZeroU64::new(1_000).unwrap(),
        max_fact_bytes: NonZeroU64::new(1024 * 1024).unwrap(),
    }
}

fn query_limits() -> RemoteQueryLimits {
    RemoteQueryLimits {
        max_results: NonZeroU32::new(50).unwrap(),
        max_bytes: NonZeroU64::new(256 * 1024).unwrap(),
        max_depth: NonZeroU32::new(4).unwrap(),
        max_duration_ms: NonZeroU64::new(2_000).unwrap(),
        max_diagnostics: NonZeroU32::new(10).unwrap(),
        max_snippet_bytes: NonZeroU64::new(4_096).unwrap(),
    }
}

fn query_budget(max_results: u32) -> RemoteQueryBudget {
    RemoteQueryBudget {
        max_results: NonZeroU32::new(max_results).unwrap(),
        max_bytes: NonZeroU64::new(128 * 1024).unwrap(),
        max_depth: NonZeroU32::new(3).unwrap(),
        max_duration_ms: NonZeroU64::new(1_000).unwrap(),
        max_diagnostics: NonZeroU32::new(5).unwrap(),
        max_snippet_bytes: NonZeroU64::new(1_024).unwrap(),
    }
}

fn auth(class: CredentialClass, scope: AuthorizationScope) -> AuthorizationContext {
    AuthorizationContext::for_class(
        PrincipalId::new("principal").unwrap(),
        CredentialId::new("credential").unwrap(),
        class,
        scope,
    )
}

fn repository_manifest(
    objects: &mut EncryptedFilesystemObjectStore,
    source_text: &str,
) -> super::RepositorySourceManifest {
    let content_identity = sha256(source_text.as_bytes());
    let source_object = objects
        .put_verified(&project(), &content_identity, source_text.as_bytes())
        .unwrap()
        .object;
    let body = RepositorySourceManifestBody {
        protocol_version: DISTRIBUTED_SOURCE_MANIFEST_VERSION,
        repository: repository(),
        source_policy_digest: digest("11"),
        source_revision: SourceRevision {
            id: SourceRevisionId::new("source-revision").unwrap(),
            repository: local_repository(),
            source_kind: SourceKind::CommittedTree,
            base_revision: Some(digest("12")),
            manifest_digest: digest("13"),
            analysis_config_digest: digest("14"),
            dirty: false,
            includes_untracked: false,
        },
        extractor_set_digest: digest("44"),
        policy_schema_version: REMOTE_REPOSITORY_SOURCE_POLICY_VERSION,
        files: vec![RepositorySourceObject {
            path: RepoPath::new("src/lib.rs").unwrap(),
            content_identity: content_identity.clone(),
            byte_len: source_text.len() as u64,
            file_mode: SourceFileMode::Regular,
            file_role: RepositoryFileRole::Source,
            object: source_object,
        }],
        summary: PackagingSummary {
            included_objects: 1,
            total_bytes: source_text.len() as u64,
            source_diagnostic_codes: BTreeMap::new(),
        },
    };
    let encoded = serde_json::to_vec(&body).unwrap();
    let manifest_digest = sha256(&encoded);
    let manifest_object = objects
        .put_verified(&project(), &manifest_digest, &encoded)
        .unwrap()
        .object;
    let manifest = RepositorySourceManifest {
        reference: super::super::identity::RepositoryManifestRef {
            repository: repository(),
            manifest_id: RepositoryManifestId::new(manifest_digest.value()).unwrap(),
            manifest_digest,
            source_policy_digest: digest("11"),
            manifest_object,
        },
        body,
    };
    manifest.validate::<(), ()>().unwrap();
    manifest
}

fn memory_manifest(objects: &mut EncryptedFilesystemObjectStore) -> MemorySourceManifest {
    let content = b"approved memory outcome";
    let content_identity = sha256(content);
    let source_object = objects
        .put_verified(&project(), &content_identity, content)
        .unwrap()
        .object;
    let body = MemorySourceManifestBody {
        protocol_version: DISTRIBUTED_SOURCE_MANIFEST_VERSION,
        project: project(),
        memory_policy_digest: digest("21"),
        project_identity: local_project(),
        source_set_digest: digest("22"),
        extractor_set_digest: digest("44"),
        policy_schema_version: MEMORY_POLICY_SCHEMA_VERSION,
        sources: vec![MemorySourceObject {
            category: MemorySourceCategory::ApprovedOutcome,
            locator: MemorySourceLocator::TrackedFile {
                path: RepoPath::new("docs/spec.md").unwrap(),
            },
            source_fingerprint: digest("23"),
            sanitized_byte_len: content.len() as u64,
            sensitivity: MemorySourceSensitivity::Curated,
            content_access: MemoryContentAccess::CuratedSections,
            object: source_object,
        }],
        summary: PackagingSummary {
            included_objects: 1,
            total_bytes: content.len() as u64,
            source_diagnostic_codes: BTreeMap::new(),
        },
    };
    let encoded = serde_json::to_vec(&body).unwrap();
    let manifest_digest = sha256(&encoded);
    let manifest_object = objects
        .put_verified(&project(), &manifest_digest, &encoded)
        .unwrap()
        .object;
    let manifest = MemorySourceManifest {
        reference: super::super::identity::MemoryManifestRef {
            project: project(),
            manifest_id: MemoryManifestId::new(manifest_digest.value()).unwrap(),
            manifest_digest,
            memory_policy_digest: digest("21"),
            manifest_object,
        },
        body,
    };
    manifest.validate::<(), ()>().unwrap();
    manifest
}

fn publishing_job(
    coordinator: &mut SqliteIndexJobCoordinator,
    input: IndexInputRef,
    kind: IndexJobKind,
    unique: &str,
) -> IndexJobRecord {
    let now = Utc::now();
    let spec = IndexJobSpec::new(
        kind,
        input,
        IndexSemantics {
            semantic_config_digest: digest("33"),
            model_version: NonZeroU32::new(1).unwrap(),
            extractor_set_digest: digest("44"),
        },
    )
    .unwrap();
    coordinator
        .submit(
            &SubmitIndexJobRequest {
                protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                request_id: RequestId::new(format!("submit-{unique}")).unwrap(),
                project: project(),
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
                project: project(),
                kind,
                worker_id: super::super::identity::WorkerId::new(format!("worker-{unique}"))
                    .unwrap(),
            },
            now,
        )
        .unwrap()
        .unwrap();
    let advance = |record: &IndexJobRecord, phase: &str| AdvanceIndexJobRequest {
        protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        request_id: RequestId::new(format!("{phase}-{unique}")).unwrap(),
        job: record.job.clone(),
        worker_id: record.lease.as_ref().unwrap().worker_id.clone(),
        lease_generation: record.lease.as_ref().unwrap().generation,
    };
    let running = coordinator.start(&advance(&leased, "start"), now).unwrap();
    coordinator
        .begin_publication(&advance(&running, "publish"), now)
        .unwrap()
}

fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let control_path = directory.path().join("control.db");
    let object_root = directory.path().join("objects");
    let mut objects = object_store(&object_root);
    let repository_manifest = repository_manifest(&mut objects, "pub struct ImportantType;\n");
    let memory_manifest = memory_manifest(&mut objects);
    let mut coordinator_backend = coordinator(&control_path);
    let graph_job = publishing_job(
        &mut coordinator_backend,
        IndexInputRef::Repository(repository_manifest.reference.clone()),
        IndexJobKind::RepositoryGraph,
        "graph",
    );
    let snapshot_id = SnapshotId::new("snapshot-query").unwrap();
    let node_id = NodeId::new("node-important").unwrap();
    let child_id = NodeId::new("node-child").unwrap();
    let provenance = FactProvenance {
        extractor: ExtractorIdentity {
            id: ExtractorId::new("test.extractor").unwrap(),
            version: "1".to_string(),
            contract_version: 1,
        },
        evidence: Some(SourceEvidence {
            path: RepoPath::new("src/lib.rs").unwrap(),
            content_identity: repository_manifest.body.files[0].content_identity.clone(),
            span: None,
        }),
        resolution: ResolutionState::Resolved,
        confidence: Confidence::Exact,
    };
    let graph_batch = FactBatch::new(
        graph_job.job.clone(),
        FactTarget::RepositoryGraph {
            snapshot: RemoteGraphSnapshotRef {
                repository: repository(),
                snapshot_id: snapshot_id.clone(),
            },
            build_id: BuildId::new("build-graph").unwrap(),
        },
        FactShardId::new("graph-all").unwrap(),
        0,
        digest("44"),
        true,
        FactBatchPayload::RepositoryGraph {
            nodes: vec![
                GraphNode {
                    snapshot_id: snapshot_id.clone(),
                    id: node_id.clone(),
                    kind: "struct".to_string(),
                    semantic_key: Some(SemanticKey::new("important-type").unwrap()),
                    provenance: provenance.clone(),
                    properties: BTreeMap::new(),
                },
                GraphNode {
                    snapshot_id: snapshot_id.clone(),
                    id: child_id.clone(),
                    kind: "field".to_string(),
                    semantic_key: Some(SemanticKey::new("important-type-child").unwrap()),
                    provenance: provenance.clone(),
                    properties: BTreeMap::new(),
                },
            ],
            edges: vec![GraphEdge {
                snapshot_id: snapshot_id.clone(),
                id: EdgeId::new("edge-child").unwrap(),
                kind: "contains".to_string(),
                source: node_id.clone(),
                target: EdgeTarget::Node(child_id),
                provenance,
                properties: BTreeMap::new(),
            }],
            diagnostics: Vec::new(),
        },
    )
    .unwrap();
    assert_eq!(
        graph_batch.header.protocol_version,
        DISTRIBUTED_FACT_PROTOCOL_VERSION
    );
    let mut publication =
        SqliteRemotePublicationStore::open(&control_path, KEY, publication_limits(), true).unwrap();
    let graph_request = super::super::protocol::PublishGraphRequest {
        protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        request_id: RequestId::new("publish-graph").unwrap(),
        job: graph_job.job.clone(),
        worker_id: graph_job.lease.as_ref().unwrap().worker_id.clone(),
        lease_generation: graph_job.lease.as_ref().unwrap().generation,
        repository: repository(),
        view_name: PublishedViewName::new("canonical").unwrap(),
        snapshot_id: snapshot_id.clone(),
        expected: None,
    };
    publication
        .publish_graph(&graph_request, &[graph_batch], Utc::now())
        .unwrap();

    let memory_job = publishing_job(
        &mut coordinator_backend,
        IndexInputRef::Memory(memory_manifest.reference.clone()),
        IndexJobKind::ProjectMemory,
        "memory",
    );
    let revision_id = MemoryRevisionId::new("memory-query").unwrap();
    let entity = MemoryEntity {
        project: local_project(),
        memory_revision_id: revision_id.clone(),
        id: MemoryEntityId::new("memory-important").unwrap(),
        data: MemoryEntityData::Outcome {
            text: MemoryText::new("Important approved outcome").unwrap(),
        },
        provenance: MemoryProvenance {
            source_category: MemorySourceCategory::ApprovedOutcome,
            source_locator: MemorySourceLocator::TrackedFile {
                path: RepoPath::new("docs/spec.md").unwrap(),
            },
            source_fingerprint: digest("23"),
            extractor: MemoryExtractorIdentity::current(
                MemoryExtractorId::new("memory.test").unwrap(),
                MemoryStatusToken::new("v1").unwrap(),
            ),
            evidence: crate::project_memory::domain::MemoryEvidenceLocator::Record(
                crate::project_memory::domain::MemoryRecordId::new("outcome").unwrap(),
            ),
            resolution: MemoryResolutionState::Resolved,
            confidence: MemoryConfidence::Exact,
            timestamps: MemoryIndexTimestamps {
                source_observed_at: Utc::now(),
                indexed_at: Utc::now(),
            },
        },
    };
    let memory_batch = FactBatch::new(
        memory_job.job.clone(),
        FactTarget::ProjectMemory {
            revision: RemoteMemoryRevisionRef {
                project: project(),
                revision_id: revision_id.clone(),
            },
            build_id: MemoryBuildId::new("build-memory").unwrap(),
        },
        FactShardId::new("memory-all").unwrap(),
        0,
        digest("44"),
        true,
        FactBatchPayload::ProjectMemory {
            entities: vec![entity],
            relationships: Vec::new(),
            diagnostics: Vec::new(),
        },
    )
    .unwrap();
    let memory_request = super::super::protocol::PublishMemoryRequest {
        protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        request_id: RequestId::new("publish-memory").unwrap(),
        job: memory_job.job.clone(),
        worker_id: memory_job.lease.as_ref().unwrap().worker_id.clone(),
        lease_generation: memory_job.lease.as_ref().unwrap().generation,
        project: project(),
        view_name: crate::project_memory::domain::MemoryViewName::new("project").unwrap(),
        revision_id: revision_id.clone(),
        expected: None,
    };
    publication
        .publish_memory(&memory_request, &[memory_batch], Utc::now())
        .unwrap();

    let query = SqliteRemoteQueryApi::new(
        SqliteRemotePublicationStore::open(&control_path, KEY, publication_limits(), true).unwrap(),
        coordinator(&control_path),
        object_store(&object_root),
        query_limits(),
    );
    Fixture {
        directory,
        control_path,
        object_root,
        query,
        project: project(),
        repository: repository(),
        graph: RemoteGraphSnapshotRef {
            repository: repository(),
            snapshot_id,
        },
        memory: RemoteMemoryRevisionRef {
            project: project(),
            revision_id,
        },
        graph_node: node_id,
        source_text: "pub struct ImportantType;\n".to_string(),
    }
}

fn query_request(
    fixture: &Fixture,
    target: RemoteQueryTarget,
    operation: RemoteQueryOperation,
    max_results: u32,
    cursor: Option<RemotePageCursor>,
) -> RemoteQueryRequest<RemoteQueryBody> {
    RemoteQueryRequest {
        protocol_version: DISTRIBUTED_QUERY_PROTOCOL_VERSION,
        request_id: RequestId::new("query").unwrap(),
        project: fixture.project.clone(),
        target,
        body: RemoteQueryBody {
            budget: query_budget(max_results),
            page: RemotePageRequest { cursor },
            operation,
        },
    }
}

#[test]
fn query_agent_is_read_only_and_denied_before_control_lookup() {
    let fixture = fixture();
    let query_agent = auth(
        CredentialClass::QueryAgent,
        AuthorizationScope::Project(fixture.project.clone()),
    );
    let unknown = InspectIndexJobRequest {
        protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        request_id: RequestId::new("inspect-unknown").unwrap(),
        job: super::super::protocol::IndexJobRef {
            project: fixture.project.clone(),
            job_id: IndexJobId::new("unknown").unwrap(),
            kind: IndexJobKind::RepositoryGraph,
        },
    };
    let control = SqliteRemoteControlApi::new(coordinator(&fixture.control_path));
    let error = control.inspect_build(&query_agent, &unknown).unwrap_err();
    assert_eq!(error.code, RemoteErrorCode::Unauthorized);

    let foreign = auth(
        CredentialClass::Coordinator,
        AuthorizationScope::Project(RemoteProjectRef {
            tenant_id: TenantId::new("tenant-b").unwrap(),
            project_id: RemoteProjectId::new("project").unwrap(),
        }),
    );
    let error = control.inspect_build(&foreign, &unknown).unwrap_err();
    assert_eq!(error.code, RemoteErrorCode::Unauthorized);
}

#[test]
fn project_operator_can_submit_inspect_and_cancel_a_build() {
    let fixture = fixture();
    let mut objects = object_store(&fixture.object_root);
    let manifest = repository_manifest(&mut objects, "pub struct LaterType;\n");
    let spec = IndexJobSpec::new(
        IndexJobKind::RepositoryGraph,
        IndexInputRef::Repository(manifest.reference),
        IndexSemantics {
            semantic_config_digest: digest("91"),
            model_version: NonZeroU32::new(1).unwrap(),
            extractor_set_digest: digest("44"),
        },
    )
    .unwrap();
    let submit = SubmitIndexJobRequest {
        protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        request_id: RequestId::new("control-submit").unwrap(),
        project: fixture.project.clone(),
        job: spec,
    };
    let operator = auth(
        CredentialClass::ProjectOperator,
        AuthorizationScope::Project(fixture.project.clone()),
    );
    let query_agent = auth(
        CredentialClass::QueryAgent,
        AuthorizationScope::Project(fixture.project.clone()),
    );
    let mut control = SqliteRemoteControlApi::new(coordinator(&fixture.control_path));
    assert_eq!(
        control
            .submit_build(&query_agent, &submit, Utc::now())
            .unwrap_err()
            .code,
        RemoteErrorCode::Unauthorized
    );
    let submitted = control
        .submit_build(&operator, &submit, Utc::now())
        .unwrap()
        .body;
    assert_eq!(
        submitted.state,
        super::super::protocol::IndexJobState::Queued
    );
    let inspected = control
        .inspect_build(
            &operator,
            &InspectIndexJobRequest {
                protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                request_id: RequestId::new("control-inspect").unwrap(),
                job: submitted.job.clone(),
            },
        )
        .unwrap()
        .body;
    assert_eq!(inspected.job, submitted.job);
    let cancelled = control
        .cancel_build(
            &operator,
            &CancelIndexJobRequest {
                protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                request_id: RequestId::new("control-cancel").unwrap(),
                job: submitted.job,
                expected_state: Some(super::super::protocol::IndexJobState::Queued),
            },
            Utc::now(),
        )
        .unwrap()
        .body;
    assert_eq!(
        cancelled.state,
        super::super::protocol::IndexJobState::Cancelled
    );
}

#[test]
fn latest_search_is_resolved_once_and_pagination_is_snapshot_bound() {
    let fixture = fixture();
    let authorization = auth(
        CredentialClass::QueryAgent,
        AuthorizationScope::Project(fixture.project.clone()),
    );
    let operation = RemoteQueryOperation::Search {
        text: "important".to_string(),
        graph_kinds: Vec::new(),
        graph_paths: Vec::new(),
        memory_kinds: Vec::new(),
    };
    let request = query_request(
        &fixture,
        RemoteQueryTarget::FederatedView {
            repository: fixture.repository.clone(),
            graph_view: PublishedViewName::new("canonical").unwrap(),
            memory_view: crate::project_memory::domain::MemoryViewName::new("project").unwrap(),
        },
        operation.clone(),
        1,
        None,
    );
    let first = fixture.query.query(&authorization, &request).unwrap();
    assert_eq!(
        first.resolved_target,
        RemoteQueryTarget::Federated(
            FederatedViewRef::new(fixture.graph.clone(), fixture.memory.clone()).unwrap()
        )
    );
    let RemoteQueryData::Search(data) = first.body.data else {
        panic!("search response expected");
    };
    assert_eq!(data.items.len(), 1);
    let cursor = first.body.page.next_cursor.unwrap();
    let changed_query = query_request(
        &fixture,
        first.resolved_target.clone(),
        RemoteQueryOperation::Search {
            text: "different".to_string(),
            graph_kinds: Vec::new(),
            graph_paths: Vec::new(),
            memory_kinds: Vec::new(),
        },
        1,
        Some(cursor.clone()),
    );
    assert_eq!(
        fixture
            .query
            .query(&authorization, &changed_query)
            .unwrap_err()
            .code,
        RemoteErrorCode::StaleCursor
    );
    let pinned = query_request(&fixture, first.resolved_target, operation, 1, Some(cursor));
    let second = fixture.query.query(&authorization, &pinned).unwrap();
    let RemoteQueryData::Search(data) = second.body.data else {
        panic!("search response expected");
    };
    assert!(!data.items.is_empty());
}

#[test]
fn context_snippets_are_manifest_scoped_hash_verified_and_bounded() {
    let fixture = fixture();
    let authorization = auth(
        CredentialClass::QueryAgent,
        AuthorizationScope::Repository(fixture.repository.clone()),
    );
    let request = query_request(
        &fixture,
        RemoteQueryTarget::Repository(fixture.graph.clone()),
        RemoteQueryOperation::Context {
            seeds: vec![RemoteContextSeed::GraphNode(fixture.graph_node.clone())],
            direction: EdgeDirection::Both,
            graph_edge_kinds: Vec::new(),
            memory_relationship_kinds: Vec::new(),
            include_unresolved: false,
            include_external: false,
            include_snippets: true,
        },
        10,
        None,
    );
    let response = fixture.query.query(&authorization, &request).unwrap();
    let RemoteQueryData::Context(data) = response.body.data else {
        panic!("context response expected");
    };
    assert_eq!(data.graph_nodes.len(), 2);
    assert_eq!(data.graph_edges.len(), 1);
    assert_eq!(data.snippets.len(), 1);
    let RemoteVerifiedSnippet::Repository {
        text,
        verified_content_identity,
        ..
    } = &data.snippets[0]
    else {
        panic!("repository snippet expected");
    };
    assert_eq!(text, &fixture.source_text);
    assert_eq!(
        verified_content_identity,
        &sha256(fixture.source_text.as_bytes())
    );
}

#[test]
fn memory_context_returns_only_authorized_sanitized_manifest_content() {
    let fixture = fixture();
    let authorization = auth(
        CredentialClass::QueryAgent,
        AuthorizationScope::Project(fixture.project.clone()),
    );
    let request = query_request(
        &fixture,
        RemoteQueryTarget::Memory(fixture.memory.clone()),
        RemoteQueryOperation::Context {
            seeds: vec![RemoteContextSeed::MemoryEntity(
                MemoryEntityId::new("memory-important").unwrap(),
            )],
            direction: EdgeDirection::Both,
            graph_edge_kinds: Vec::new(),
            memory_relationship_kinds: Vec::new(),
            include_unresolved: false,
            include_external: false,
            include_snippets: true,
        },
        10,
        None,
    );
    let response = fixture.query.query(&authorization, &request).unwrap();
    let RemoteQueryData::Context(data) = response.body.data else {
        panic!("context response expected");
    };
    assert_eq!(data.memory_entities.len(), 1);
    assert_eq!(data.snippets.len(), 1);
    let RemoteVerifiedSnippet::Memory {
        text,
        verified_fingerprint,
        ..
    } = &data.snippets[0]
    else {
        panic!("memory snippet expected");
    };
    assert_eq!(text, "approved memory outcome");
    assert_eq!(verified_fingerprint, &digest("23"));
}

#[test]
fn memory_context_traversal_honors_relationship_direction() {
    let fixture = fixture();
    let store =
        SqliteRemotePublicationStore::open(&fixture.control_path, KEY, publication_limits(), true)
            .unwrap();
    let mut memory = store.memory_revision(&fixture.memory).unwrap().unwrap();
    let mut source = memory.entities[0].clone();
    source.id = MemoryEntityId::new("memory-source").unwrap();
    let mut target = memory.entities[0].clone();
    target.id = MemoryEntityId::new("memory-target").unwrap();
    memory.entities = vec![source.clone(), target.clone()];
    memory.relationships = vec![MemoryRelationship {
        project: source.project.clone(),
        memory_revision_id: source.memory_revision_id.clone(),
        id: MemoryRelationshipId::new("memory-direction-edge").unwrap(),
        kind: MemoryRelationshipKind::Concerns,
        source: source.id.clone(),
        target: MemoryRelationshipTarget::MemoryEntity {
            entity_id: target.id.clone(),
        },
        provenance: source.provenance,
    }];
    let loaded = LoadedTarget {
        graph: None,
        memory: Some(memory),
    };
    let traverse = |seed: &MemoryEntityId, direction| {
        let (units, _) = context_units(
            &loaded,
            &[RemoteContextSeed::MemoryEntity(seed.clone())],
            direction,
            &[],
            &[],
            false,
            false,
            1,
            Instant::now(),
            Duration::from_secs(1),
        );
        let entities = units
            .iter()
            .filter_map(|unit| match unit {
                ContextUnit::MemoryEntity(entity) => Some(entity.id.clone()),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        let relationships = units
            .iter()
            .filter(|unit| matches!(unit, ContextUnit::MemoryRelationship(_)))
            .count();
        (entities, relationships)
    };

    assert_eq!(
        traverse(&source.id, EdgeDirection::Outgoing),
        (BTreeSet::from([source.id.clone(), target.id.clone()]), 1)
    );
    assert_eq!(
        traverse(&source.id, EdgeDirection::Incoming),
        (BTreeSet::from([source.id.clone()]), 0)
    );
    assert_eq!(
        traverse(&target.id, EdgeDirection::Outgoing),
        (BTreeSet::from([target.id.clone()]), 0)
    );
    assert_eq!(
        traverse(&target.id, EdgeDirection::Incoming),
        (BTreeSet::from([source.id, target.id]), 1)
    );
}

#[test]
fn authorization_denies_foreign_queries_without_disclosing_target_existence() {
    let fixture = fixture();
    let authorization = auth(
        CredentialClass::QueryAgent,
        AuthorizationScope::Project(RemoteProjectRef {
            tenant_id: TenantId::new("tenant-b").unwrap(),
            project_id: RemoteProjectId::new("project").unwrap(),
        }),
    );
    let request = query_request(
        &fixture,
        RemoteQueryTarget::Repository(fixture.graph.clone()),
        RemoteQueryOperation::Status,
        10,
        None,
    );
    let error = fixture.query.query(&authorization, &request).unwrap_err();
    assert_eq!(error.code, RemoteErrorCode::Unauthorized);
    assert!(!error.retryable);
}

#[test]
fn explicit_queries_reject_immutable_targets_without_publication_visibility() {
    let fixture = fixture();
    rusqlite::Connection::open(&fixture.control_path)
        .unwrap()
        .execute(
            "DELETE FROM remote_published_targets
             WHERE tenant_id = ?1 AND project_id = ?2",
            rusqlite::params![
                fixture.project.tenant_id.as_str(),
                fixture.project.project_id.as_str()
            ],
        )
        .unwrap();
    let authorization = auth(
        CredentialClass::QueryAgent,
        AuthorizationScope::Project(fixture.project.clone()),
    );

    for target in [
        RemoteQueryTarget::Repository(fixture.graph.clone()),
        RemoteQueryTarget::Memory(fixture.memory.clone()),
    ] {
        let request = query_request(&fixture, target, RemoteQueryOperation::Status, 10, None);
        assert_eq!(
            fixture
                .query
                .query(&authorization, &request)
                .unwrap_err()
                .code,
            RemoteErrorCode::NotFound
        );
    }
}

#[test]
fn stale_cursor_and_tiny_response_budget_fail_without_reissuing_pages() {
    let fixture = fixture();
    let authorization = auth(
        CredentialClass::QueryAgent,
        AuthorizationScope::Project(fixture.project.clone()),
    );
    let mut request = query_request(
        &fixture,
        RemoteQueryTarget::Repository(fixture.graph.clone()),
        RemoteQueryOperation::Search {
            text: "important".to_string(),
            graph_kinds: Vec::new(),
            graph_paths: Vec::new(),
            memory_kinds: Vec::new(),
        },
        1,
        Some(RemotePageCursor::new("deadbeef.0").unwrap()),
    );
    let error = fixture.query.query(&authorization, &request).unwrap_err();
    assert_eq!(error.code, RemoteErrorCode::StaleCursor);

    request.body.page.cursor = None;
    request.body.budget.max_bytes = NonZeroU64::new(1).unwrap();
    let error = fixture.query.query(&authorization, &request).unwrap_err();
    assert_eq!(error.code, RemoteErrorCode::BudgetExceeded);
}

#[test]
fn verified_content_store_outage_is_retryable_and_privacy_safe() {
    let fixture = fixture();
    let source_identity = sha256(fixture.source_text.as_bytes());
    let source_path = fixture
        .object_root
        .join("objects")
        .join(fixture.project.tenant_id.as_str())
        .join(fixture.project.project_id.as_str())
        .join(format!("{}.enc", source_identity.value()));
    std::fs::remove_file(source_path).unwrap();
    let authorization = auth(
        CredentialClass::QueryAgent,
        AuthorizationScope::Repository(fixture.repository.clone()),
    );
    let request = query_request(
        &fixture,
        RemoteQueryTarget::Repository(fixture.graph.clone()),
        RemoteQueryOperation::Context {
            seeds: vec![RemoteContextSeed::GraphNode(fixture.graph_node.clone())],
            direction: EdgeDirection::Both,
            graph_edge_kinds: Vec::new(),
            memory_relationship_kinds: Vec::new(),
            include_unresolved: false,
            include_external: false,
            include_snippets: true,
        },
        10,
        None,
    );
    let error = fixture.query.query(&authorization, &request).unwrap_err();
    assert_eq!(error.code, RemoteErrorCode::TemporarilyUnavailable);
    assert!(error.retryable);
    assert_eq!(
        serde_json::to_value(&error).unwrap(),
        serde_json::json!({
            "protocol_version": DISTRIBUTED_QUERY_PROTOCOL_VERSION,
            "request_id": "query",
            "code": "temporarily_unavailable",
            "retryable": true
        })
    );
}

#[test]
fn fixture_keeps_remote_state_outside_local_runtime_databases() {
    let fixture = fixture();
    assert!(fixture.control_path.exists());
    assert!(fixture.object_root.join("object-store.db").exists());
    assert!(!fixture.directory.path().join("ferrus.db").exists());
    assert!(!fixture.directory.path().join("repo-graph.db").exists());
    assert!(!fixture.directory.path().join("project-memory.db").exists());
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AdapterContractObservation {
    target: String,
    result_ids: Vec<String>,
    every_result_has_provenance: bool,
    protocol_version: u32,
}

fn assert_snapshot_query_contract(
    adapter: &str,
    first: AdapterContractObservation,
    second: AdapterContractObservation,
) {
    assert_eq!(first, second, "{adapter} query is not deterministic");
    assert!(!first.target.is_empty(), "{adapter} did not pin a target");
    assert!(
        !first.result_ids.is_empty(),
        "{adapter} returned no contract fixture results"
    );
    assert!(
        first.every_result_has_provenance,
        "{adapter} dropped provenance"
    );
    assert!(
        first.protocol_version > 0,
        "{adapter} omitted an explicit protocol version"
    );
}

#[test]
fn local_and_remote_sqlite_adapters_share_snapshot_query_semantics() {
    use std::{fs, process::Command};

    use crate::{
        project_memory::{
            domain::{MemoryQueryText, MemoryViewName},
            index::{MemoryIndexOptions, MemoryIndexer},
            policy::MemoryPolicy,
            ports::MemoryQuery,
            query::{
                MemoryPageRequest, MemoryQueryScope, MemoryRevisionSelector, MemorySearchRequest,
            },
            query_sqlite::{SqliteMemoryQuery, default_budget as memory_budget},
            source::LocalMemorySource,
            sqlite::MemorySidecar,
        },
        repository_graph::{
            config::RepositoryGraphConfig,
            domain::{BuildId, PublishedViewName, RepositoryId, RepositoryNamespace},
            index::{IndexCoordinator, IndexRequest, active_extractor_identities},
            ports::GraphQuery,
            query::{PageRequest, QueryScope, SearchRequest, SnapshotSelector},
            query_sqlite::{SqliteGraphQuery, default_budget as graph_budget},
            source::{FilesystemRepositorySource, SourceDiscoveryContext},
            sqlite::{OpenSidecarResult, open_for_build_at},
        },
    };

    let local_root = tempfile::tempdir().unwrap();
    fs::create_dir_all(local_root.path().join("src")).unwrap();
    fs::write(
        local_root.path().join("Cargo.toml"),
        "[package]\nname='parity'\nversion='0.1.0'\n",
    )
    .unwrap();
    fs::write(
        local_root.path().join("src/lib.rs"),
        "pub struct ImportantType;\n",
    )
    .unwrap();
    let repository = crate::repository_graph::domain::RepositoryRef {
        namespace: RepositoryNamespace::new("local:parity").unwrap(),
        repository_id: RepositoryId::new("root").unwrap(),
    };
    let graph_config = RepositoryGraphConfig::default();
    let identities = active_extractor_identities(&graph_config).unwrap();
    let context =
        SourceDiscoveryContext::from_config(repository.clone(), &graph_config, &identities)
            .unwrap();
    let source = FilesystemRepositorySource::discover(local_root.path(), context).unwrap();
    let graph_data = tempfile::tempdir().unwrap();
    let OpenSidecarResult::Ready(mut sidecar) =
        open_for_build_at(&graph_data.path().join("repo-graph.db")).unwrap()
    else {
        panic!("new parity sidecar unexpectedly requires rebuild");
    };
    let snapshot = IndexCoordinator::new(&mut sidecar)
        .index(
            &source,
            &graph_config,
            IndexRequest {
                build_id: BuildId::new("build-parity").unwrap(),
                view_name: PublishedViewName::new("canonical").unwrap(),
                force_full: false,
            },
        )
        .unwrap()
        .snapshot;
    let local_graph = SqliteGraphQuery::new(&sidecar, graph_config.query_limits.clone(), None);
    let local_graph_request = SearchRequest {
        scope: QueryScope::current(
            repository,
            SnapshotSelector::Snapshot(snapshot.id),
            graph_budget(&graph_config.query_limits).unwrap(),
        ),
        text: "ImportantType".to_string(),
        node_kinds: Vec::new(),
        paths: Vec::new(),
        page: PageRequest { cursor: None },
    };
    let observe_local_graph = || {
        let response = local_graph.search(&local_graph_request).unwrap();
        AdapterContractObservation {
            target: response.snapshot_id.to_string(),
            result_ids: response
                .data
                .hits
                .iter()
                .map(|hit| hit.node_id.to_string())
                .collect(),
            every_result_has_provenance: response
                .data
                .hits
                .iter()
                .all(|hit| hit.provenance.evidence.is_some()),
            protocol_version: response.wire_version,
        }
    };
    assert_snapshot_query_contract(
        "local repository SQLite",
        observe_local_graph(),
        observe_local_graph(),
    );

    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(local_root.path())
            .status()
            .unwrap()
            .success()
    );
    fs::create_dir_all(local_root.path().join("docs/specs")).unwrap();
    fs::write(
        local_root.path().join("docs/specs/parity.md"),
        "# Parity\n\n- [x] #5.6 Adapter parity\n\nID: parity\nDepends on: none\n\n## Outcome\n\nDelivered important bounded retrieval.\n",
    )
    .unwrap();
    assert!(
        Command::new("git")
            .args(["add", "--", "docs/specs/parity.md"])
            .current_dir(local_root.path())
            .status()
            .unwrap()
            .success()
    );
    let memory_data = tempfile::tempdir().unwrap();
    let memory_project = ProjectRef {
        namespace: ProjectNamespace::new("local:parity").unwrap(),
        project_id: ProjectId::new("project").unwrap(),
    };
    let memory_source = LocalMemorySource::discover_at(
        local_root.path().to_path_buf(),
        memory_data.path().to_path_buf(),
        memory_project.clone(),
        RepoPath::new("docs/specs").unwrap(),
        MemoryPolicy::default(),
    )
    .unwrap();
    let mut memory_sidecar = MemorySidecar::open_at(memory_data.path()).unwrap();
    MemoryIndexer::new(&memory_source, &mut memory_sidecar)
        .unwrap()
        .index(MemoryIndexOptions::default())
        .unwrap();
    let memory_limits = graph_config.query_limits.clone();
    let local_memory = SqliteMemoryQuery::new(&memory_sidecar, memory_limits.clone());
    let local_memory_request = MemorySearchRequest {
        scope: MemoryQueryScope::current(
            memory_project,
            MemoryRevisionSelector::Published(MemoryViewName::new("project").unwrap()),
            memory_budget(&memory_limits).unwrap(),
        ),
        text: MemoryQueryText::new("important").unwrap(),
        entity_kinds: Vec::new(),
        source_categories: Vec::new(),
        page: MemoryPageRequest { cursor: None },
    };
    let observe_local_memory = || {
        let response = local_memory.search(local_memory_request.clone()).unwrap();
        AdapterContractObservation {
            target: response.revision_id.to_string(),
            result_ids: response
                .hits
                .iter()
                .map(|hit| hit.entity.id.to_string())
                .collect(),
            every_result_has_provenance: response
                .hits
                .iter()
                .all(|hit| serde_json::to_vec(&hit.entity.provenance).is_ok()),
            protocol_version: response.wire_version,
        }
    };
    assert_snapshot_query_contract(
        "local memory SQLite",
        observe_local_memory(),
        observe_local_memory(),
    );

    let remote = fixture();
    let authorization = auth(
        CredentialClass::QueryAgent,
        AuthorizationScope::Project(remote.project.clone()),
    );
    let observe_remote = |target: RemoteQueryTarget, text: &str| {
        let response = remote
            .query
            .query(
                &authorization,
                &query_request(
                    &remote,
                    target,
                    RemoteQueryOperation::Search {
                        text: text.to_string(),
                        graph_kinds: Vec::new(),
                        graph_paths: Vec::new(),
                        memory_kinds: Vec::new(),
                    },
                    10,
                    None,
                ),
            )
            .unwrap();
        let RemoteQueryData::Search(data) = response.body.data else {
            panic!("remote parity search response expected");
        };
        AdapterContractObservation {
            target: serde_json::to_string(&response.resolved_target).unwrap(),
            result_ids: data
                .items
                .iter()
                .map(|item| match item {
                    RemoteSearchItem::Repository { node, .. } => node.id.to_string(),
                    RemoteSearchItem::Memory { entity, .. } => entity.id.to_string(),
                })
                .collect(),
            every_result_has_provenance: data.items.iter().all(|item| match item {
                RemoteSearchItem::Repository { node, .. } => node.provenance.evidence.is_some(),
                RemoteSearchItem::Memory { entity, .. } => {
                    serde_json::to_vec(&entity.provenance).is_ok()
                }
            }),
            protocol_version: response.protocol_version,
        }
    };
    assert_snapshot_query_contract(
        "remote repository SQLite",
        observe_remote(
            RemoteQueryTarget::Repository(remote.graph.clone()),
            "important",
        ),
        observe_remote(
            RemoteQueryTarget::Repository(remote.graph.clone()),
            "important",
        ),
    );
    assert_snapshot_query_contract(
        "remote memory SQLite",
        observe_remote(
            RemoteQueryTarget::Memory(remote.memory.clone()),
            "important",
        ),
        observe_remote(
            RemoteQueryTarget::Memory(remote.memory.clone()),
            "important",
        ),
    );
}
