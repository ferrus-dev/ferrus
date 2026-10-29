//! Tests for memory.

use super::*;

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
            .inspect(&super::super::protocol::InspectIndexJobRequest {
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
            .inspect(&super::super::protocol::InspectIndexJobRequest {
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
