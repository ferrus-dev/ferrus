//! Tests for authorization.

use super::*;

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
    let mut facts = SqliteFactBatchStore::open_uncoordinated_for_test(
        &fixture.fact_path,
        KEY,
        fact_quota(),
        true,
    )
    .unwrap();
    assert!(matches!(
        facts.put(
            &batch(&fixture.graph_job_a, "after-deletion"),
            &fact_authority(),
            fact_write_deadline(),
        ),
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
