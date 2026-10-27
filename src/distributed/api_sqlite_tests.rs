//! Remote API tests for authorization ordering, pinned queries, budgets, and verified content.

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
        object_store::{ObjectStoreProtection, ObjectStoreQuota, TenantObjectStore},
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
            AuthorizedSourceDescriptor, AuthorizedSourceManifest, MemoryBuildId, MemoryConfidence,
            MemoryEntityData, MemoryEntityId, MemoryEvidenceLocator, MemoryExtractorId,
            MemoryExtractorIdentity, MemoryIndexTimestamps, MemoryProvenance, MemoryRecordId,
            MemoryRelationshipId, MemoryRelationshipKind, MemoryResolutionState,
            MemorySourceCategory, MemorySourceLocator, MemoryStatusToken, MemoryText, ProjectId,
            ProjectNamespace, ProjectRef,
        },
        policy::{
            MEMORY_POLICY_SCHEMA_VERSION, MemoryContentAccess, MemoryPolicy,
            MemorySourceSensitivity,
        },
    },
    repository_graph::{
        domain::{
            BuildId, Confidence, Digest, EdgeId, EdgeTarget, ExtractorId, ExtractorIdentity,
            FactProvenance, GraphEdge, GraphNode, PublishedViewName, RepoPath, RepositoryId,
            RepositoryNamespace, RepositoryRef, ResolutionState, SemanticKey, SnapshotId,
            SourceEvidence, SourceKind, SourcePosition, SourceRevision, SourceRevisionId,
            SourceSpan,
        },
        ports::{SourceFileDescriptor, SourceFileMode, canonical_source_manifest_digest},
    },
};

const KEY: [u8; 32] = [71; 32];
const MEMORY_SOURCE_TEXT: &str = "unrelated milestone\napproved memory outcome\nunrelated decision";
const MEMORY_SNIPPET_TEXT: &str = "approved memory outcome";

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
    let mut body = RepositorySourceManifestBody {
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
    let source_files = body
        .files
        .iter()
        .map(|file| SourceFileDescriptor {
            path: file.path.clone(),
            content_identity: file.content_identity.clone(),
            byte_len: file.byte_len,
            file_mode: file.file_mode,
        })
        .collect::<Vec<_>>();
    body.source_revision.manifest_digest =
        canonical_source_manifest_digest(&source_files, &body.source_policy_digest).unwrap();
    let encoded = serde_json::to_vec(&body).unwrap();
    let manifest_digest = sha256(&encoded);
    let manifest_object = objects
        .put_verified(&project(), &manifest_digest, &encoded)
        .unwrap()
        .object;
    let expected_snapshot_id = crate::repository_graph::index::snapshot_identity_from_revision(
        &body.source_revision,
        &body.extractor_set_digest,
    )
    .unwrap();
    let manifest = RepositorySourceManifest {
        reference: super::super::identity::RepositoryManifestRef {
            repository: repository(),
            repository_identity: local_repository(),
            manifest_id: RepositoryManifestId::new(manifest_digest.value()).unwrap(),
            manifest_digest,
            source_policy_digest: digest("11"),
            expected_snapshot_id,
            manifest_object,
        },
        body,
    };
    manifest.validate::<(), ()>().unwrap();
    manifest
}

fn memory_manifest(objects: &mut EncryptedFilesystemObjectStore) -> MemorySourceManifest {
    let content = MEMORY_SOURCE_TEXT.as_bytes();
    let policy_digest = MemoryPolicy::default().digest().unwrap();
    let content_identity = sha256(content);
    let source_object = objects
        .put_verified(&project(), &content_identity, content)
        .unwrap()
        .object;
    let source_descriptor = AuthorizedSourceDescriptor {
        project: local_project(),
        category: MemorySourceCategory::ApprovedOutcome,
        locator: MemorySourceLocator::TrackedFile {
            path: RepoPath::new("docs/spec.md").unwrap(),
        },
        fingerprint: digest("23"),
        byte_len: content.len() as u64,
    };
    let mut authorized_manifest = AuthorizedSourceManifest {
        project: local_project(),
        policy_digest: policy_digest.clone(),
        source_set_digest: digest("22"),
        extractor_set_digest: digest("44"),
        sources: vec![source_descriptor],
    };
    authorized_manifest.source_set_digest =
        authorized_manifest.computed_source_set_digest().unwrap();
    let expected_revision_id = authorized_manifest.revision_id().unwrap();
    let body = MemorySourceManifestBody {
        protocol_version: DISTRIBUTED_SOURCE_MANIFEST_VERSION,
        project: project(),
        memory_policy_digest: policy_digest.clone(),
        project_identity: local_project(),
        source_set_digest: authorized_manifest.source_set_digest,
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
            project_identity: body.project_identity.clone(),
            manifest_id: MemoryManifestId::new(manifest_digest.value()).unwrap(),
            manifest_digest,
            memory_policy_digest: policy_digest,
            expected_revision_id,
            manifest_object,
            repository_snapshot: None,
            repository_origin_snapshots: Vec::new(),
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

fn memory_evidence_span() -> SourceSpan {
    let start = MEMORY_SOURCE_TEXT
        .find(MEMORY_SNIPPET_TEXT)
        .expect("memory snippet belongs to the fixture source");
    SourceSpan {
        start: SourcePosition {
            byte_offset: start as u64,
            line: Some(2),
            column: Some(1),
        },
        end: SourcePosition {
            byte_offset: (start + MEMORY_SNIPPET_TEXT.len()) as u64,
            line: Some(2),
            column: Some((MEMORY_SNIPPET_TEXT.len() + 1) as u32),
        },
    }
}

fn fixture() -> Fixture {
    fixture_with_memory_evidence(MemoryEvidenceLocator::Span(memory_evidence_span()))
}

fn fixture_with_memory_evidence(memory_evidence: MemoryEvidenceLocator) -> Fixture {
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
    let snapshot_id = repository_manifest.reference.expected_snapshot_id.clone();
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
            repository_identity: local_repository(),
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
    let revision_id = memory_manifest.reference.expected_revision_id.clone();
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
            evidence: memory_evidence,
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
            project_identity: local_project(),
            build_id: MemoryBuildId::new("build-memory").unwrap(),
            repository_links: None,
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

#[path = "api_sqlite_tests/authorization.rs"]
mod authorization_tests;
#[path = "api_sqlite_tests/cancellation.rs"]
mod cancellation_tests;
#[path = "api_sqlite_tests/context.rs"]
mod context_tests;
#[path = "api_sqlite_tests/memory.rs"]
mod memory_tests;
#[path = "api_sqlite_tests/pagination.rs"]
mod pagination_tests;
#[path = "api_sqlite_tests/review.rs"]
mod review_tests;
#[path = "api_sqlite_tests/snapshots.rs"]
mod snapshots_tests;

#[test]
fn fixture_keeps_remote_state_outside_local_runtime_databases() {
    let fixture = fixture();
    assert!(fixture.control_path.exists());
    assert!(fixture.object_root.join("object-store.db").exists());
    assert!(!fixture.directory.path().join("ferrus.db").exists());
    assert!(!fixture.directory.path().join("repo-graph.db").exists());
    assert!(!fixture.directory.path().join("project-memory.db").exists());
}

#[test]
fn depth_limited_remote_traversal_reports_depth_truncation() {
    let fixture = fixture();
    let authorization = auth(
        CredentialClass::QueryAgent,
        AuthorizationScope::Repository(fixture.repository.clone()),
    );
    let operations = [
        RemoteQueryOperation::Neighborhood {
            roots: vec![fixture.graph_node.clone()],
            direction: EdgeDirection::Outgoing,
            edge_kinds: Vec::new(),
        },
        RemoteQueryOperation::Context {
            seeds: vec![RemoteContextSeed::GraphNode(fixture.graph_node.clone())],
            direction: EdgeDirection::Outgoing,
            graph_edge_kinds: Vec::new(),
            memory_relationship_kinds: Vec::new(),
            include_unresolved: false,
            include_external: false,
            include_snippets: false,
        },
    ];

    for operation in operations {
        let mut request = query_request(
            &fixture,
            RemoteQueryTarget::Repository(fixture.graph.clone()),
            operation,
            10,
            None,
        );
        request.body.budget.max_depth = NonZeroU32::new(1).unwrap();

        let response = fixture.query.query(&authorization, &request).unwrap();

        assert_eq!(response.body.page.explored_depth, 1);
        assert_eq!(
            response.body.page.truncation,
            Some(RemoteTruncationReason::Depth)
        );
    }
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
