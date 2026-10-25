//! Tests for cancellation.

use super::*;

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
            &super::super::super::protocol::CancelIndexJobRequest {
                protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                request_id: RequestId::new("cancel").unwrap(),
                job: job.job.clone(),
                expected_state: Some(super::super::super::protocol::IndexJobState::Publishing),
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
