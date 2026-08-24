use std::{
    collections::BTreeSet,
    fs,
    num::{NonZeroU32, NonZeroU64},
    path::{Path, PathBuf},
};

use chrono::Utc;
use rusqlite::{Connection, params};
use sha2::{Digest as _, Sha256};

use super::{
    DISTRIBUTED_CONTROL_PROTOCOL_VERSION, DISTRIBUTED_MAINTENANCE_PROTOCOL_VERSION,
    coordinator::IndexJobCoordinator,
    coordinator_sqlite::{CoordinatorError, CoordinatorLimits, SqliteIndexJobCoordinator},
    fact_store::FactBatchStore,
    fact_store_sqlite::{FactStoreError, FactStoreQuota, SqliteFactBatchStore},
    identity::{
        CredentialId, DeletionId, FactShardId, MemoryManifestId, PrincipalId,
        RemoteGraphSnapshotRef, RemoteProjectId, RemoteProjectRef, RemoteRepositoryId,
        RemoteRepositoryRef, RepositoryManifestId, RequestId, TenantId, TenantObjectRef,
    },
    maintenance::{InspectRemoteDeletionRequest, RemoteDeleteRequest, RemoteMaintenanceApi},
    maintenance_sqlite::SqliteRemoteMaintenance,
    object_store::{
        EncryptedFilesystemObjectStore, ObjectStoreError, ObjectStoreQuota, TenantObjectStore,
    },
    protocol::{
        FactBatch, FactBatchPayload, FactTarget, IndexInputRef, IndexJobKind, IndexJobRef,
        IndexJobSpec, IndexSemantics, InspectIndexJobRequest, RemoteErrorCode,
        SubmitIndexJobRequest,
    },
    publication_sqlite::{RemoteStoreLimits, SqliteRemotePublicationStore},
    security::{
        AuditCounter, AuthorizationContext, AuthorizationScope, CredentialClass, DeleteDataRequest,
        DeletionState, DeletionTarget, RetentionClass,
    },
};
use crate::{
    project_memory::domain::{MemoryRevisionId, ProjectId, ProjectNamespace, ProjectRef},
    repository_graph::domain::{
        BuildId, Digest, RepositoryId, RepositoryNamespace, RepositoryRef, SnapshotId,
    },
};

const KEY: [u8; 32] = [73; 32];
const SOURCE: &[u8] = b"same private source bytes";

struct Fixture {
    directory: tempfile::TempDir,
    control_path: PathBuf,
    fact_path: PathBuf,
    object_root: PathBuf,
    project_a: RemoteProjectRef,
    project_b: RemoteProjectRef,
    repository_a: RemoteRepositoryRef,
    graph_job_a: IndexJobRef,
    memory_job_a: IndexJobRef,
    object_a: TenantObjectRef,
    object_b: TenantObjectRef,
    maintenance: SqliteRemoteMaintenance,
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

fn repository(project: &RemoteProjectRef) -> RemoteRepositoryRef {
    RemoteRepositoryRef {
        project: project.clone(),
        repository_id: RemoteRepositoryId::new("repository").unwrap(),
    }
}

fn digest_bytes(content: &[u8]) -> Digest {
    let value = Sha256::digest(content)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Digest::new("sha256", value).unwrap()
}

fn digest(value: &str) -> Digest {
    Digest::new("sha256", value).unwrap()
}

fn coordinator_limits() -> CoordinatorLimits {
    CoordinatorLimits {
        max_attempts: NonZeroU32::new(3).unwrap(),
        lease_ttl_ms: NonZeroU64::new(60_000).unwrap(),
        max_job_duration_ms: NonZeroU64::new(120_000).unwrap(),
    }
}

fn object_quota() -> ObjectStoreQuota {
    ObjectStoreQuota {
        max_objects_per_project: NonZeroU64::new(100).unwrap(),
        max_bytes_per_project: NonZeroU64::new(1024 * 1024).unwrap(),
        max_object_bytes: NonZeroU64::new(1024 * 1024).unwrap(),
    }
}

fn fact_quota() -> FactStoreQuota {
    FactStoreQuota {
        max_batches_per_project: NonZeroU64::new(100).unwrap(),
        max_bytes_per_project: NonZeroU64::new(1024 * 1024).unwrap(),
        max_batch_bytes: NonZeroU64::new(1024 * 1024).unwrap(),
    }
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

fn submit_job(
    control_path: &Path,
    project: &RemoteProjectRef,
    repository: &RemoteRepositoryRef,
    object: &TenantObjectRef,
    suffix: &str,
) -> IndexJobRef {
    let input = IndexInputRef::Repository(super::identity::RepositoryManifestRef {
        repository: repository.clone(),
        repository_identity: RepositoryRef {
            namespace: RepositoryNamespace::new("remote:test").unwrap(),
            repository_id: RepositoryId::new("root").unwrap(),
        },
        manifest_id: RepositoryManifestId::new(format!("manifest-{suffix}")).unwrap(),
        manifest_digest: object.content_identity.clone(),
        source_policy_digest: digest("22"),
        expected_snapshot_id: SnapshotId::new(format!("snapshot-{suffix}")).unwrap(),
        manifest_object: object.clone(),
    });
    let job = IndexJobSpec::new(
        IndexJobKind::RepositoryGraph,
        input,
        IndexSemantics {
            semantic_config_digest: digest("33"),
            model_version: NonZeroU32::new(1).unwrap(),
            extractor_set_digest: digest("44"),
        },
    )
    .unwrap();
    SqliteIndexJobCoordinator::open(control_path, coordinator_limits())
        .unwrap()
        .submit(
            &SubmitIndexJobRequest {
                protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                request_id: RequestId::new(format!("submit-{suffix}")).unwrap(),
                project: project.clone(),
                job,
            },
            Utc::now(),
        )
        .unwrap()
        .job
}

fn submit_memory_job(
    control_path: &Path,
    project: &RemoteProjectRef,
    object: &TenantObjectRef,
    suffix: &str,
) -> IndexJobRef {
    let input = IndexInputRef::Memory(super::identity::MemoryManifestRef {
        project: project.clone(),
        project_identity: local_project(),
        manifest_id: MemoryManifestId::new(format!("memory-manifest-{suffix}")).unwrap(),
        manifest_digest: object.content_identity.clone(),
        memory_policy_digest: digest("22"),
        expected_revision_id: MemoryRevisionId::new(format!("memory-{suffix}")).unwrap(),
        manifest_object: object.clone(),
        repository_snapshot: None,
    });
    let job = IndexJobSpec::new(
        IndexJobKind::ProjectMemory,
        input,
        IndexSemantics {
            semantic_config_digest: digest("33"),
            model_version: NonZeroU32::new(1).unwrap(),
            extractor_set_digest: digest("44"),
        },
    )
    .unwrap();
    SqliteIndexJobCoordinator::open(control_path, coordinator_limits())
        .unwrap()
        .submit(
            &SubmitIndexJobRequest {
                protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                request_id: RequestId::new(format!("submit-memory-{suffix}")).unwrap(),
                project: project.clone(),
                job,
            },
            Utc::now(),
        )
        .unwrap()
        .job
}

fn batch(job: &IndexJobRef, suffix: &str) -> FactBatch {
    FactBatch::new(
        job.clone(),
        FactTarget::RepositoryGraph {
            snapshot: RemoteGraphSnapshotRef {
                repository: repository(&job.project),
                snapshot_id: SnapshotId::new(format!("snapshot-{suffix}")).unwrap(),
            },
            repository_identity: RepositoryRef {
                namespace: RepositoryNamespace::new("remote:test").unwrap(),
                repository_id: RepositoryId::new("root").unwrap(),
            },
            build_id: BuildId::new(format!("build-{suffix}")).unwrap(),
        },
        FactShardId::new("all").unwrap(),
        0,
        digest("55"),
        true,
        FactBatchPayload::RepositoryGraph {
            nodes: Vec::new(),
            edges: Vec::new(),
            diagnostics: Vec::new(),
        },
    )
    .unwrap()
}

fn seed_publications(control_path: &Path, jobs: &[(&IndexJobRef, &IndexJobRef)]) {
    let connection = Connection::open(control_path).unwrap();
    connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .unwrap();
    for (graph_job, memory_job) in jobs {
        let suffix = graph_job.project.tenant_id.as_str();
        let snapshot = format!("snapshot-{suffix}");
        let revision = format!("memory-{suffix}");
        connection
            .execute(
                "INSERT INTO remote_immutable_revisions (
                    tenant_id, project_id, domain, repository_id, target_id,
                    job_id, job_kind, build_id, extractor_digest_algorithm,
                    extractor_digest_value, fact_digest_algorithm, fact_digest_value,
                    primary_count, relationship_count, diagnostic_count, completed_at_ms
                 ) VALUES (?1, ?2, 'repository_graph', 'repository', ?3, ?4,
                           'repository_graph', 'build', 'sha256', '44', 'sha256', '55',
                           1, 0, 0, ?5)",
                params![
                    graph_job.project.tenant_id.as_str(),
                    graph_job.project.project_id.as_str(),
                    snapshot,
                    graph_job.job_id.as_str(),
                    Utc::now().timestamp_millis()
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO remote_graph_views (
                    tenant_id, project_id, domain, repository_id, view_name,
                    snapshot_id, job_id, generation
                 ) VALUES (?1, ?2, 'repository_graph', 'repository', 'canonical', ?3, ?4, 1)",
                params![
                    graph_job.project.tenant_id.as_str(),
                    graph_job.project.project_id.as_str(),
                    snapshot,
                    graph_job.job_id.as_str()
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO remote_published_targets (
                    tenant_id, project_id, domain, repository_id, target_id,
                    first_published_at_ms
                 ) VALUES (?1, ?2, 'repository_graph', 'repository', ?3, ?4)",
                params![
                    graph_job.project.tenant_id.as_str(),
                    graph_job.project.project_id.as_str(),
                    snapshot,
                    Utc::now().timestamp_millis()
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO remote_encrypted_facts (
                    tenant_id, project_id, domain, repository_id, target_id,
                    fact_kind, fact_id, byte_len, nonce, ciphertext
                 ) VALUES (?1, ?2, 'repository_graph', 'repository', ?3,
                           'node', 'node', 1, ?4, ?5)",
                params![
                    graph_job.project.tenant_id.as_str(),
                    graph_job.project.project_id.as_str(),
                    snapshot,
                    vec![0u8; 12],
                    vec![0u8; 16]
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO remote_immutable_revisions (
                    tenant_id, project_id, domain, repository_id, target_id,
                    job_id, job_kind, build_id, extractor_digest_algorithm,
                    extractor_digest_value, fact_digest_algorithm, fact_digest_value,
                    primary_count, relationship_count, diagnostic_count, completed_at_ms
                 ) VALUES (?1, ?2, 'project_memory', '', ?3, ?4,
                           'project_memory', 'memory-build', 'sha256', '44', 'sha256', '55',
                           1, 0, 0, ?5)",
                params![
                    memory_job.project.tenant_id.as_str(),
                    memory_job.project.project_id.as_str(),
                    revision,
                    memory_job.job_id.as_str(),
                    Utc::now().timestamp_millis()
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO remote_memory_views (
                    tenant_id, project_id, domain, repository_id, view_name,
                    revision_id, job_id, generation
                 ) VALUES (?1, ?2, 'project_memory', '', 'project', ?3, ?4, 1)",
                params![
                    memory_job.project.tenant_id.as_str(),
                    memory_job.project.project_id.as_str(),
                    revision,
                    memory_job.job_id.as_str()
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO remote_published_targets (
                    tenant_id, project_id, domain, repository_id, target_id,
                    first_published_at_ms
                 ) VALUES (?1, ?2, 'project_memory', '', ?3, ?4)",
                params![
                    memory_job.project.tenant_id.as_str(),
                    memory_job.project.project_id.as_str(),
                    revision,
                    Utc::now().timestamp_millis()
                ],
            )
            .unwrap();
    }
}

fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let control_path = directory.path().join("control.db");
    let fact_path = directory.path().join("facts.db");
    let object_root = directory.path().join("objects");
    let project_a = project("tenant-a");
    let project_b = project("tenant-b");
    let repository_a = repository(&project_a);
    let repository_b = repository(&project_b);
    let (object_a, object_b) = {
        let mut objects =
            EncryptedFilesystemObjectStore::open(&object_root, KEY, object_quota(), true).unwrap();
        let identity = digest_bytes(SOURCE);
        let object_a = objects
            .put_verified(&project_a, &identity, SOURCE)
            .unwrap()
            .object;
        let object_b = objects
            .put_verified(&project_b, &identity, SOURCE)
            .unwrap()
            .object;
        (object_a, object_b)
    };
    let graph_job_a = submit_job(&control_path, &project_a, &repository_a, &object_a, "a");
    let graph_job_b = submit_job(&control_path, &project_b, &repository_b, &object_b, "b");
    let memory_job_a = submit_memory_job(&control_path, &project_a, &object_a, "a");
    let memory_job_b = submit_memory_job(&control_path, &project_b, &object_b, "b");
    drop(
        SqliteRemotePublicationStore::open(&control_path, KEY, publication_limits(), true).unwrap(),
    );
    seed_publications(
        &control_path,
        &[(&graph_job_a, &memory_job_a), (&graph_job_b, &memory_job_b)],
    );
    {
        let mut facts = SqliteFactBatchStore::open(&fact_path, KEY, fact_quota(), true).unwrap();
        facts.put(&batch(&graph_job_a, "a")).unwrap();
        facts.put(&batch(&graph_job_b, "b")).unwrap();
    }
    let maintenance =
        SqliteRemoteMaintenance::open(&control_path, &fact_path, &object_root).unwrap();
    Fixture {
        directory,
        control_path,
        fact_path,
        object_root,
        project_a,
        project_b,
        repository_a,
        graph_job_a,
        memory_job_a,
        object_a,
        object_b,
        maintenance,
    }
}

fn administrator(project: &RemoteProjectRef) -> AuthorizationContext {
    AuthorizationContext::for_class(
        PrincipalId::new("administrator").unwrap(),
        CredentialId::new("admin-credential").unwrap(),
        CredentialClass::TenantAdministrator,
        AuthorizationScope::Tenant(project.tenant_id.clone()),
    )
}

fn full_request(project: &RemoteProjectRef, deletion: &str, request: &str) -> RemoteDeleteRequest {
    RemoteDeleteRequest {
        protocol_version: DISTRIBUTED_MAINTENANCE_PROTOCOL_VERSION,
        request_id: RequestId::new(request).unwrap(),
        deletion: DeleteDataRequest::new(
            DeletionId::new(deletion).unwrap(),
            DeletionTarget::Project(project.clone()),
            RetentionClass::ALL.into_iter().collect::<BTreeSet<_>>(),
            Utc::now(),
        )
        .unwrap(),
    }
}

fn table_count(path: &Path, table: &str, project: &RemoteProjectRef) -> u64 {
    let count = Connection::open(path)
        .unwrap()
        .query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE tenant_id = ?1 AND project_id = ?2"),
            params![project.tenant_id.as_str(), project.project_id.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    u64::try_from(count).unwrap()
}

#[test]
fn project_deletion_is_idempotent_audited_and_tenant_isolated() {
    let mut fixture = fixture();
    let request = full_request(&fixture.project_a, "delete-a", "delete-a-first");
    let result = fixture
        .maintenance
        .delete(&administrator(&fixture.project_a), &request, Utc::now())
        .unwrap();
    assert_eq!(result.state, DeletionState::Complete);
    for (counter, expected) in [
        (AuditCounter::Objects, 1),
        (AuditCounter::FactBatches, 1),
        (AuditCounter::Snapshots, 1),
        (AuditCounter::Revisions, 1),
        (AuditCounter::Jobs, 2),
    ] {
        assert_eq!(result.counters.get(&counter), Some(&expected));
    }
    assert!(result.audit_event_id.is_some());

    for table in [
        "distributed_index_jobs",
        "remote_immutable_revisions",
        "remote_published_targets",
        "remote_graph_views",
        "remote_memory_views",
    ] {
        assert_eq!(
            table_count(&fixture.control_path, table, &fixture.project_a),
            0
        );
        assert!(table_count(&fixture.control_path, table, &fixture.project_b) > 0);
    }
    assert_eq!(
        table_count(
            &fixture.fact_path,
            "unpublished_fact_batches",
            &fixture.project_a
        ),
        0
    );
    assert_eq!(
        table_count(
            &fixture.fact_path,
            "unpublished_fact_batches",
            &fixture.project_b
        ),
        1
    );
    let objects =
        EncryptedFilesystemObjectStore::open(&fixture.object_root, KEY, object_quota(), true)
            .unwrap();
    assert!(matches!(
        objects.read_verified(&fixture.object_a),
        Err(ObjectStoreError::ObjectUnavailable)
    ));
    assert_eq!(objects.read_verified(&fixture.object_b).unwrap(), SOURCE);
    drop(objects);

    let object_database = fixture.object_root.join("object-store.db");
    for path in [
        fixture.control_path.as_path(),
        fixture.fact_path.as_path(),
        object_database.as_path(),
    ] {
        assert_eq!(
            table_count(path, "project_deletion_tombstones", &fixture.project_a),
            1
        );
        assert_eq!(
            table_count(path, "project_deletion_tombstones", &fixture.project_b),
            0
        );
    }

    let mut objects =
        EncryptedFilesystemObjectStore::open(&fixture.object_root, KEY, object_quota(), true)
            .unwrap();
    assert!(matches!(
        objects.put_verified(
            &fixture.project_a,
            &digest_bytes(b"new source after deletion"),
            b"new source after deletion",
        ),
        Err(ObjectStoreError::ProjectDeleted)
    ));
    let mut facts =
        SqliteFactBatchStore::open(&fixture.fact_path, KEY, fact_quota(), true).unwrap();
    assert!(matches!(
        facts.put(&batch(&fixture.graph_job_a, "after-deletion")),
        Err(FactStoreError::ProjectDeleted)
    ));

    let repeated = full_request(&fixture.project_a, "delete-a-retry", "delete-a-retry");
    let repeated = fixture
        .maintenance
        .delete(&administrator(&fixture.project_a), &repeated, Utc::now())
        .unwrap();
    assert_eq!(repeated.deletion_id, result.deletion_id);
    assert_eq!(repeated.counters, result.counters);
    assert_eq!(repeated.audit_event_id, result.audit_event_id);

    let repository_scoped_admin = AuthorizationContext::for_class(
        PrincipalId::new("repository-administrator").unwrap(),
        CredentialId::new("repository-admin-credential").unwrap(),
        CredentialClass::TenantAdministrator,
        AuthorizationScope::Repository(fixture.repository_a.clone()),
    );
    assert!(
        fixture
            .maintenance
            .inspect_deletion(
                &repository_scoped_admin,
                &InspectRemoteDeletionRequest {
                    protocol_version: DISTRIBUTED_MAINTENANCE_PROTOCOL_VERSION,
                    request_id: RequestId::new("inspect-wrong-target").unwrap(),
                    deletion_id: result.deletion_id.clone(),
                    target: DeletionTarget::Repository(fixture.repository_a.clone()),
                },
            )
            .unwrap()
            .is_none()
    );

    let audit: Vec<u8> = Connection::open(&fixture.control_path)
        .unwrap()
        .query_row(
            "SELECT record_json FROM remote_audit_records
             WHERE tenant_id = ?1 AND project_id = ?2",
            params![
                fixture.project_a.tenant_id.as_str(),
                fixture.project_a.project_id.as_str()
            ],
            |row| row.get(0),
        )
        .unwrap();
    let audit = String::from_utf8(audit).unwrap();
    assert!(!audit.contains("same private source"));
    assert!(!audit.contains("/Users/"));
    assert!(!audit.contains("access_token"));
}

#[test]
fn authorization_precedes_validation_and_foreign_lookup() {
    let mut fixture = fixture();
    let query_agent = AuthorizationContext::for_class(
        PrincipalId::new("query-agent").unwrap(),
        CredentialId::new("query-credential").unwrap(),
        CredentialClass::QueryAgent,
        AuthorizationScope::Project(fixture.project_a.clone()),
    );
    let mut request = full_request(&fixture.project_b, "delete-b", "foreign-delete");
    request.protocol_version += 1;
    let error = fixture
        .maintenance
        .delete(&query_agent, &request, Utc::now())
        .unwrap_err();
    assert_eq!(error.code, RemoteErrorCode::Unauthorized);
    assert!(!error.retryable);
    assert_eq!(
        table_count(
            &fixture.control_path,
            "distributed_index_jobs",
            &fixture.project_b
        ),
        2
    );
}

#[test]
fn failed_cross_store_deletion_resumes_without_losing_progress() {
    let mut fixture = fixture();
    let saved_facts = fixture.directory.path().join("facts.saved");
    fs::rename(&fixture.fact_path, &saved_facts).unwrap();
    let request = full_request(&fixture.project_a, "delete-resume", "delete-resume-first");
    let error = fixture
        .maintenance
        .delete(&administrator(&fixture.project_a), &request, Utc::now())
        .unwrap_err();
    assert_eq!(error.code, RemoteErrorCode::TemporarilyUnavailable);
    assert!(error.retryable);
    let inspected = fixture
        .maintenance
        .inspect_deletion(
            &administrator(&fixture.project_a),
            &InspectRemoteDeletionRequest {
                protocol_version: DISTRIBUTED_MAINTENANCE_PROTOCOL_VERSION,
                request_id: RequestId::new("inspect-failed").unwrap(),
                deletion_id: request.deletion.deletion_id.clone(),
                target: request.deletion.target.clone(),
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(inspected.state, DeletionState::Failed);
    assert_eq!(inspected.counters.get(&AuditCounter::Objects), Some(&1));

    fs::rename(&saved_facts, &fixture.fact_path).unwrap();
    let completed = fixture
        .maintenance
        .delete(&administrator(&fixture.project_a), &request, Utc::now())
        .unwrap();
    assert_eq!(completed.state, DeletionState::Complete);
    assert_eq!(completed.counters.get(&AuditCounter::Objects), Some(&1));
    assert_eq!(completed.counters.get(&AuditCounter::FactBatches), Some(&1));
    assert_eq!(
        table_count(
            &fixture.control_path,
            "distributed_index_jobs",
            &fixture.project_b
        ),
        2
    );
}

fn job_count_for_kind(path: &Path, project: &RemoteProjectRef, kind: &str) -> u64 {
    let count = Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM distributed_index_jobs
             WHERE tenant_id = ?1 AND project_id = ?2 AND kind = ?3",
            params![
                project.tenant_id.as_str(),
                project.project_id.as_str(),
                kind
            ],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    u64::try_from(count).unwrap()
}

fn partial_project_request(
    project: &RemoteProjectRef,
    deletion: &str,
    coverage: RetentionClass,
) -> RemoteDeleteRequest {
    RemoteDeleteRequest {
        protocol_version: DISTRIBUTED_MAINTENANCE_PROTOCOL_VERSION,
        request_id: RequestId::new(format!("request-{deletion}")).unwrap(),
        deletion: DeleteDataRequest::new(
            DeletionId::new(deletion).unwrap(),
            DeletionTarget::Project(project.clone()),
            BTreeSet::from([coverage]),
            Utc::now(),
        )
        .unwrap(),
    }
}

fn partial_repository_request(
    repository: &RemoteRepositoryRef,
    deletion: &str,
    coverage: RetentionClass,
) -> RemoteDeleteRequest {
    RemoteDeleteRequest {
        protocol_version: DISTRIBUTED_MAINTENANCE_PROTOCOL_VERSION,
        request_id: RequestId::new(format!("request-{deletion}")).unwrap(),
        deletion: DeleteDataRequest::new(
            DeletionId::new(deletion).unwrap(),
            DeletionTarget::Repository(repository.clone()),
            BTreeSet::from([coverage]),
            Utc::now(),
        )
        .unwrap(),
    }
}

#[test]
fn partial_project_deletion_quiesces_writers_until_completion() {
    let fixture = fixture();
    let request = partial_project_request(
        &fixture.project_a,
        "quiesce-partial-project",
        RetentionClass::PublishedGraphSnapshot,
    );
    assert_deletion_quiesces_writers(fixture, request, "partial-project");
}

#[test]
fn repository_deletion_quiesces_writers_until_completion() {
    let fixture = fixture();
    let request = partial_repository_request(
        &fixture.repository_a,
        "quiesce-repository",
        RetentionClass::PublishedGraphSnapshot,
    );
    assert_deletion_quiesces_writers(fixture, request, "repository");
}

fn assert_deletion_quiesces_writers(
    mut fixture: Fixture,
    request: RemoteDeleteRequest,
    request_suffix: &str,
) {
    let mut coordinator =
        SqliteIndexJobCoordinator::open(&fixture.control_path, coordinator_limits()).unwrap();
    let graph_job = coordinator
        .inspect(&InspectIndexJobRequest {
            protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            request_id: RequestId::new(format!("inspect-before-{request_suffix}-deletion"))
                .unwrap(),
            job: fixture.graph_job_a.clone(),
        })
        .unwrap()
        .unwrap();
    let submit = SubmitIndexJobRequest {
        protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        request_id: RequestId::new(format!("submit-during-{request_suffix}-deletion")).unwrap(),
        project: fixture.project_a.clone(),
        job: graph_job.spec,
    };
    fixture
        .maintenance
        .begin_for_test(&request.deletion, Utc::now())
        .unwrap();

    assert_eq!(
        table_count(
            &fixture.control_path,
            "project_deletion_tombstones",
            &fixture.project_a
        ),
        1
    );
    assert!(matches!(
        coordinator.submit(&submit, Utc::now()),
        Err(CoordinatorError::ProjectDeleted)
    ));

    fixture
        .maintenance
        .delete(&administrator(&fixture.project_a), &request, Utc::now())
        .unwrap();

    assert_eq!(
        table_count(
            &fixture.control_path,
            "project_deletion_tombstones",
            &fixture.project_a
        ),
        0
    );
    coordinator.submit(&submit, Utc::now()).unwrap();
}

#[test]
fn graph_only_project_deletion_preserves_memory_job_linkage() {
    let mut fixture = fixture();
    let request = partial_project_request(
        &fixture.project_a,
        "delete-only-graph",
        RetentionClass::PublishedGraphSnapshot,
    );
    let result = fixture
        .maintenance
        .delete(&administrator(&fixture.project_a), &request, Utc::now())
        .unwrap();

    assert_eq!(result.counters.get(&AuditCounter::Snapshots), Some(&1));
    assert_eq!(result.counters.get(&AuditCounter::Jobs), Some(&1));
    assert_eq!(
        job_count_for_kind(
            &fixture.control_path,
            &fixture.project_a,
            "repository_graph"
        ),
        0
    );
    assert_eq!(
        job_count_for_kind(&fixture.control_path, &fixture.project_a, "project_memory"),
        1
    );
    assert!(
        SqliteIndexJobCoordinator::open(&fixture.control_path, coordinator_limits())
            .unwrap()
            .inspect(&super::protocol::InspectIndexJobRequest {
                protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                request_id: RequestId::new("inspect-retained-memory-job").unwrap(),
                job: fixture.memory_job_a.clone(),
            })
            .unwrap()
            .is_some()
    );
    assert_eq!(
        table_count(
            &fixture.control_path,
            "remote_immutable_revisions",
            &fixture.project_a
        ),
        1
    );
}

#[test]
fn memory_only_project_deletion_preserves_graph_job_linkage() {
    let mut fixture = fixture();
    let request = partial_project_request(
        &fixture.project_a,
        "delete-only-memory",
        RetentionClass::PublishedMemoryRevision,
    );
    let result = fixture
        .maintenance
        .delete(&administrator(&fixture.project_a), &request, Utc::now())
        .unwrap();

    assert_eq!(result.counters.get(&AuditCounter::Revisions), Some(&1));
    assert_eq!(result.counters.get(&AuditCounter::Jobs), Some(&1));
    assert_eq!(
        job_count_for_kind(
            &fixture.control_path,
            &fixture.project_a,
            "repository_graph"
        ),
        1
    );
    assert_eq!(
        job_count_for_kind(&fixture.control_path, &fixture.project_a, "project_memory"),
        0
    );
    assert!(
        SqliteIndexJobCoordinator::open(&fixture.control_path, coordinator_limits())
            .unwrap()
            .inspect(&super::protocol::InspectIndexJobRequest {
                protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                request_id: RequestId::new("inspect-retained-graph-job").unwrap(),
                job: fixture.graph_job_a.clone(),
            })
            .unwrap()
            .is_some()
    );
    assert_eq!(
        table_count(
            &fixture.control_path,
            "remote_immutable_revisions",
            &fixture.project_a
        ),
        1
    );
}

#[test]
fn repository_job_mapping_survives_until_separate_fact_coverage_completes() {
    let mut fixture = fixture();
    let snapshot_request = partial_repository_request(
        &fixture.repository_a,
        "delete-repository-snapshot-first",
        RetentionClass::PublishedGraphSnapshot,
    );
    let snapshot_result = fixture
        .maintenance
        .delete(
            &administrator(&fixture.project_a),
            &snapshot_request,
            Utc::now(),
        )
        .unwrap();

    assert_eq!(
        snapshot_result.counters.get(&AuditCounter::Snapshots),
        Some(&1)
    );
    assert_eq!(snapshot_result.counters.get(&AuditCounter::Jobs), Some(&0));
    assert_eq!(
        job_count_for_kind(
            &fixture.control_path,
            &fixture.project_a,
            "repository_graph"
        ),
        1
    );
    assert_eq!(
        table_count(
            &fixture.fact_path,
            "unpublished_fact_batches",
            &fixture.project_a
        ),
        1
    );

    let fact_request = partial_repository_request(
        &fixture.repository_a,
        "delete-repository-facts-second",
        RetentionClass::UnpublishedFact,
    );
    let fact_result = fixture
        .maintenance
        .delete(
            &administrator(&fixture.project_a),
            &fact_request,
            Utc::now(),
        )
        .unwrap();

    assert_eq!(
        fact_result.counters.get(&AuditCounter::FactBatches),
        Some(&1)
    );
    assert_eq!(fact_result.counters.get(&AuditCounter::Jobs), Some(&1));
    assert_eq!(
        job_count_for_kind(
            &fixture.control_path,
            &fixture.project_a,
            "repository_graph"
        ),
        0
    );
    assert_eq!(
        table_count(
            &fixture.fact_path,
            "unpublished_fact_batches",
            &fixture.project_a
        ),
        0
    );
}

#[test]
fn repository_job_mapping_survives_until_snapshot_coverage_completes() {
    let mut fixture = fixture();
    let fact_request = partial_repository_request(
        &fixture.repository_a,
        "delete-repository-facts-first",
        RetentionClass::UnpublishedFact,
    );
    fixture
        .maintenance
        .delete(
            &administrator(&fixture.project_a),
            &fact_request,
            Utc::now(),
        )
        .unwrap();

    assert_eq!(
        job_count_for_kind(
            &fixture.control_path,
            &fixture.project_a,
            "repository_graph"
        ),
        1
    );
    assert_eq!(
        table_count(
            &fixture.fact_path,
            "unpublished_fact_batches",
            &fixture.project_a
        ),
        0
    );

    let snapshot_request = partial_repository_request(
        &fixture.repository_a,
        "delete-repository-snapshot-second",
        RetentionClass::PublishedGraphSnapshot,
    );
    let snapshot_result = fixture
        .maintenance
        .delete(
            &administrator(&fixture.project_a),
            &snapshot_request,
            Utc::now(),
        )
        .unwrap();

    assert_eq!(
        snapshot_result.counters.get(&AuditCounter::Snapshots),
        Some(&1)
    );
    assert_eq!(snapshot_result.counters.get(&AuditCounter::Jobs), Some(&1));
    assert_eq!(
        job_count_for_kind(
            &fixture.control_path,
            &fixture.project_a,
            "repository_graph"
        ),
        0
    );
}

#[test]
fn repository_deletion_preserves_memory_and_shared_source_objects() {
    let mut fixture = fixture();
    let request = RemoteDeleteRequest {
        protocol_version: DISTRIBUTED_MAINTENANCE_PROTOCOL_VERSION,
        request_id: RequestId::new("delete-repository").unwrap(),
        deletion: DeleteDataRequest::new(
            DeletionId::new("delete-repository").unwrap(),
            DeletionTarget::Repository(fixture.repository_a.clone()),
            BTreeSet::from([
                RetentionClass::UnpublishedFact,
                RetentionClass::PublishedGraphSnapshot,
                RetentionClass::QueryCache,
                RetentionClass::AuditRecord,
            ]),
            Utc::now(),
        )
        .unwrap(),
    };
    fixture
        .maintenance
        .delete(&administrator(&fixture.project_a), &request, Utc::now())
        .unwrap();
    let connection = Connection::open(&fixture.control_path).unwrap();
    let domain_count = |domain: &str| -> u64 {
        let count = connection
            .query_row(
                "SELECT COUNT(*) FROM remote_immutable_revisions
                 WHERE tenant_id = ?1 AND project_id = ?2 AND domain = ?3",
                params![
                    fixture.project_a.tenant_id.as_str(),
                    fixture.project_a.project_id.as_str(),
                    domain
                ],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        u64::try_from(count).unwrap()
    };
    assert_eq!(domain_count("repository_graph"), 0);
    assert_eq!(domain_count("project_memory"), 1);
    let objects =
        EncryptedFilesystemObjectStore::open(&fixture.object_root, KEY, object_quota(), true)
            .unwrap();
    assert_eq!(objects.read_verified(&fixture.object_a).unwrap(), SOURCE);
    drop(objects);

    let unsafe_request = RemoteDeleteRequest {
        protocol_version: DISTRIBUTED_MAINTENANCE_PROTOCOL_VERSION,
        request_id: RequestId::new("delete-repository-source").unwrap(),
        deletion: DeleteDataRequest::new(
            DeletionId::new("delete-repository-source").unwrap(),
            DeletionTarget::Repository(fixture.repository_a.clone()),
            RetentionClass::ALL.into_iter().collect(),
            Utc::now(),
        )
        .unwrap(),
    };
    assert_eq!(
        fixture
            .maintenance
            .delete(
                &administrator(&fixture.project_a),
                &unsafe_request,
                Utc::now()
            )
            .unwrap_err()
            .code,
        RemoteErrorCode::InvalidRequest
    );
}
