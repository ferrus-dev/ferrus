//! Tests for review.

use super::*;

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
