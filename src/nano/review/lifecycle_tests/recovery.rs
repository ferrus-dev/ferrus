//! Tests for recovery.

use super::*;

#[tokio::test]
async fn relaunched_native_reviewer_renews_its_inherited_lease_before_heartbeat() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let executor = git_session(&f, true).await;
    lifecycle::submit(&executor, "Submission".into(), &Cancellation::default())
        .await
        .unwrap();
    let first = reviewer(&f, "crashed-review", &f.root).await;
    let abandoned_source = first.source.path.clone();
    assert!(abandoned_source.is_dir());
    project::record_run_finished(&first.scope.run_id, 1)
        .await
        .unwrap();
    let lease = (chrono::Utc::now() + chrono::Duration::seconds(10)).to_rfc3339();
    f.connection()
        .execute(
            "UPDATE tasks SET lease_until=?1 WHERE id=?2",
            rusqlite::params![lease, TASK],
        )
        .unwrap();
    let resumed = reviewer(&f, "recovered-review", &f.root).await;
    assert!(!abandoned_source.exists());
    assert!(resumed.source.path.is_dir());
    for human_wait in [false, true] {
        if human_wait {
            f.connection().execute("UPDATE tasks SET status='awaiting_human',awaiting_human_status='reviewing',awaiting_human_by=?1,lease_until=?2 WHERE id=?3", rusqlite::params![resumed.scope.agent_id,lease,TASK]).unwrap();
            project::start_supervisor_session(&resumed.scope, true, resumed.ttl)
                .await
                .unwrap();
        }

        resumed.authorize().await.unwrap();
        let renewed: String = f
            .connection()
            .query_row("SELECT lease_until FROM tasks WHERE id=?1", [TASK], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(
            chrono::DateTime::parse_from_rfc3339(&renewed).unwrap()
                > chrono::Utc::now() + chrono::Duration::seconds(resumed.ttl as i64 - 5)
        );
    }
}

#[tokio::test]
async fn native_recovery_refuses_to_repeat_an_unknown_approval() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let executor = git_session(&f, true).await;
    lifecycle::submit(&executor, "Submission".into(), &Cancellation::default())
        .await
        .unwrap();
    let first = reviewer(&f, "interrupted-review", &f.root).await;
    let (native, journal) = review_native(&f, &first);
    let cancel = Cancellation::default();
    let end = review::run(
        first,
        Limits::default(),
        script(vec![
            tool("context", "review_pending", json!({})),
            tool("approve", "approve", json!({})),
        ]),
        native,
        InterruptedApproval {
            inner: journal,
            pending: None,
        },
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(end.reason, EndReason::JournalFailed);
    project::record_run_finished("interrupted-review", 0)
        .await
        .unwrap();
    let next = reviewer(&f, "recovered-review", &f.root).await;
    let (native, journal) = review_native(&f, &next);
    let provider = script(vec![tool("approve", "approve", json!({}))]);
    let starts = provider.starts.clone();
    let error = review::run(
        next,
        Limits::default(),
        provider,
        native,
        journal,
        &Cancellation::default(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("manual reconciliation"));
    assert_eq!(starts.load(Ordering::SeqCst), 0);
    assert_eq!(
        project::list_tasks()
            .await
            .unwrap()
            .into_iter()
            .find(|task| task.id == TASK)
            .unwrap()
            .failure_reason
            .as_deref(),
        Some("nano_effect_unknown")
    );
}

#[tokio::test]
async fn native_approval_with_lost_receipt_does_not_reopen_or_repeat_the_completed_task() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let executor = isolated(&f).await;
    std::fs::write(
        executor.workspace().join("src/lib.rs"),
        "pub struct Final;\n",
    )
    .unwrap();
    lifecycle::submit(&executor, "Final change".into(), &Cancellation::default())
        .await
        .unwrap();
    let session = reviewer(&f, "lost-receipt-review", &f.root).await;
    let (native, journal) = review_native(&f, &session);
    let end = review::run(
        session,
        Limits::default(),
        script(vec![
            tool("context", "review_pending", json!({})),
            tool("approve", "approve", json!({})),
        ]),
        native,
        LostDecisionReceipt { inner: journal },
        &Cancellation::default(),
    )
    .await
    .unwrap();
    let records = std::fs::read_to_string(
        f.data
            .join("nano/sessions/lost-receipt-review/events.jsonl"),
    )
    .unwrap();
    assert_eq!(end.reason, EndReason::JournalFailed, "{records}");
    assert_eq!(
        project::list_tasks()
            .await
            .unwrap()
            .into_iter()
            .find(|task| task.id == TASK)
            .unwrap()
            .status,
        "complete"
    );
    assert!(
        std::fs::read_to_string(f.root.join("src/lib.rs"))
            .unwrap()
            .contains("Final")
    );
    project::record_run_finished("lost-receipt-review", 1)
        .await
        .unwrap();
    project::record_run_started_for_task_with_workspace(
        "no-replay",
        "supervisor",
        "supervisor:nano:t-001",
        std::process::id(),
        Some(TASK),
        f.root.to_string_lossy().into_owned(),
    )
    .await
    .unwrap();
    let launch = LaunchContext {
        project_root: f.root.clone(),
        workspace: f.root.clone(),
        agent_id: "supervisor:nano:t-001".into(),
        task_id: TASK.into(),
        run_id: "no-replay".into(),
        baseline_tree: None,
    };
    assert!(ReviewSession::bind(launch).await.is_err());
}
