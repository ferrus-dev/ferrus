//! Tests for checks.

use super::*;

#[tokio::test]
async fn managed_checks_collect_superseded_graphs_from_the_bound_project() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = git_session(&f, true).await;
    let config = std::fs::read_to_string(f.root.join("ferrus.toml")).unwrap();
    std::fs::write(
        f.root.join("ferrus.toml"),
        format!("{config}\n[repository_graph.retention]\nmax_snapshots = 0\n"),
    )
    .unwrap();
    let baseline = session
        .status()
        .await
        .unwrap()
        .repository_view
        .baseline_snapshot_id
        .unwrap();
    let unrelated = tempfile::tempdir().unwrap();
    std::env::set_current_dir(unrelated.path()).unwrap();
    std::fs::write(f.root.join("src/lib.rs"), "pub struct FirstEdit;\n").unwrap();
    assert_eq!(
        lifecycle::check(&session, &Cancellation::default())
            .await
            .unwrap()["status"],
        "passed"
    );
    let previous = session
        .status()
        .await
        .unwrap()
        .repository_view
        .view_snapshot_id
        .unwrap();
    assert_ne!(previous, baseline);
    let sidecar = Connection::open(f.data.join("repo-graph.db")).unwrap();
    // Retain the baseline through the live task reference alone, not a publication.
    sidecar
        .execute(
            "DELETE FROM published_views WHERE view_name = 'canonical'",
            [],
        )
        .unwrap();
    let previous_content: String = sidecar
        .query_row(
            "SELECT content_digest FROM files WHERE snapshot_id = ?1 AND path = 'src/lib.rs'",
            [previous.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    std::fs::write(f.root.join("src/lib.rs"), "pub struct SecondEdit;\n").unwrap();
    assert_eq!(
        lifecycle::check(&session, &Cancellation::default())
            .await
            .unwrap()["status"],
        "passed"
    );
    let current = session
        .status()
        .await
        .unwrap()
        .repository_view
        .view_snapshot_id
        .unwrap();
    for (id, retained) in [(&baseline, true), (&previous, false), (&current, true)] {
        let exists: bool = sidecar
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM snapshots WHERE id = ?1)",
                [id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, retained, "snapshot {id:?}");
    }

    let fragments: i64 = sidecar
        .query_row(
            "SELECT count(*) FROM fragment_cache WHERE path = 'src/lib.rs' AND content_digest = ?1",
            [previous_content],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(fragments, 0);
    assert_eq!(session.status().await.unwrap().status, "executing");
    assert!(!unrelated.path().join(".ferrus").exists());
}

#[tokio::test]
async fn managed_graph_maintenance_failure_preserves_a_successful_refresh() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = git_session(&f, true).await;
    let previous = session.status().await.unwrap().repository_view;
    // Invalid retention metadata must fail maintenance closed without undoing publication.
    f.connection()
        .execute(
            "UPDATE project_runtime_state SET canonical_graph_snapshot_id = '' WHERE row_id = 1",
            [],
        )
        .unwrap();
    assert!(
        project::repository_graph_retention_references_at(&f.data.join("ferrus.db"))
            .await
            .is_err()
    );
    std::fs::write(f.root.join("src/lib.rs"), "pub struct CheckedEdit;\n").unwrap();
    assert_eq!(
        lifecycle::check(&session, &Cancellation::default())
            .await
            .unwrap()["status"],
        "passed"
    );
    let context = session.status().await.unwrap();
    assert_eq!(context.status, "executing");
    assert_eq!(
        context.repository_view.status,
        project::RepositoryViewStatus::Available
    );
    assert_ne!(
        context.repository_view.view_snapshot_id,
        previous.view_snapshot_id
    );
}

#[tokio::test]
async fn managed_graph_freeze_failure_is_diagnostic_only() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = git_session(&f, false).await;
    let result = lifecycle::submit(&session, "notes".into(), &Cancellation::default())
        .await
        .unwrap();
    assert_eq!(result["status"], "submitted");
    assert_eq!(session.status().await.unwrap().status, "reviewing");
    assert!(
        f.events()
            .iter()
            .any(|(kind, _)| kind == "repository_view_freeze_failed")
    );
    assert_eq!(session.status().await.unwrap().check_retries, 0);
}

#[tokio::test]
async fn managed_engine_ends_failed_when_check_retries_are_exhausted() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    configure(&f, &["exit 1"], 1);
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    let (tools, journal) = native(&f, session.clone(), "managed-exhausted");
    let provider = script(vec![ToolCall {
        provider_call_id: "check".into(),
        name: "check".into(),
        arguments: "{}".into(),
    }]);
    let result = managed::run(
        session.clone(),
        identity("managed-exhausted"),
        Limits::default(),
        provider,
        tools,
        journal,
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(result.reason, EndReason::TaskFailed);
    assert_eq!(result.budget.model_turns, 1);
    assert_eq!(session.status().await.unwrap().status, "failed");
}
