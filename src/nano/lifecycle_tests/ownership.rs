//! Tests for ownership.

use super::*;

#[tokio::test]
async fn permanent_provider_failure_or_retry_exhaustion_stops_dispatch_without_check_retries() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    for (kind, retryable, reason, code) in [
        (
            ProviderErrorKind::Unsupported,
            false,
            EndReason::ProviderProtocol,
            "nano_provider_protocol",
        ),
        (
            ProviderErrorKind::Authentication,
            false,
            EndReason::ProviderFailed,
            "nano_provider_failed",
        ),
        (
            ProviderErrorKind::Timeout,
            true,
            EndReason::Limit(LimitKind::Retries),
            "nano_limit_retries",
        ),
    ] {
        let f = Fixture::new().await;
        let session = FerrusSession::bind(f.launch()).await.unwrap();
        let (tools, journal) = native(&f, session.clone(), RUN);
        let end = managed::run(
            session.clone(),
            identity(RUN),
            Limits {
                retries: 0,
                ..Default::default()
            },
            RejectedProvider(kind, retryable),
            tools,
            journal,
            &Cancellation::default(),
        )
        .await
        .unwrap();
        assert_eq!(end.reason, reason);
        assert!(end.durable);
        let context = session.status().await.unwrap();
        assert_eq!(context.status, "failed");
        assert_eq!(context.check_retries, 0);
        let state: (String, Option<String>, Option<String>) = f
            .connection()
            .query_row(
                "SELECT failure_reason,claimed_by,lease_until FROM tasks WHERE id=?1",
                [TASK],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, (code.into(), None, None));
        assert!(f.events().iter().any(|(kind, _)| kind == code));
        assert!(matches!(
            session.claim().await.unwrap(),
            project::ReadyTaskClaim::NoAvailable
        ));
        let (_, records) =
            FileJournal::recover(&f.data.join("nano/sessions").join(RUN), Quotas::default())
                .unwrap();
        assert!(
            matches!(&records.last().unwrap().event, SessionEvent::Ended {reason: actual} if actual == &reason)
        );
    }
}

#[tokio::test]
async fn terminal_provider_failure_requires_current_executor_ownership() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    for update in [
        "UPDATE tasks SET claimed_by='executor:other:1'",
        "UPDATE tasks SET claimed_by='executor:nano:1',status='reviewing'",
        "UPDATE tasks SET status='awaiting_human'",
    ] {
        f.connection().execute(update, []).unwrap();
        let tasks = project::list_tasks().await.unwrap();
        let events = f.events();
        assert!(
            session
                .fail_stopped(&EndReason::ProviderProtocol, false)
                .await
                .is_err()
        );
        f.assert_no_effect(tasks, events).await;
    }
}

#[tokio::test]
async fn completed_exec_intent_with_running_command_blocks_redispatch() {
    use std::io::Write;

    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-running");
    previous_intent(&f, "old-running", "exec", None, true);
    let directory = f.data.join("nano/sessions/old-running");
    let (mut journal, _) = FileJournal::recover_open(&directory, Quotas::default()).unwrap();
    journal
        .append(
            SessionEvent::ToolResult {
                call_id: "call-1".into(),
                outcome: ToolOutcome::Success(json!({"process_id":"old-running-p1"})),
            },
            &journal.state().budget.clone(),
        )
        .unwrap();
    drop(journal);
    let commands_dir = directory.join("commands");
    crate::nano::private::directory(&commands_dir, true).unwrap();
    let snapshot = commands::Snapshot {
        process_id: "old-running-p1".into(),
        backend: "trusted_local".into(),
        completion: commands::Completion::Running,
        stdout: commands::OutputRef {
            handle: "old-running-p1-stdout".into(),
            bytes: 0,
        },
        stderr: commands::OutputRef {
            handle: "old-running-p1-stderr".into(),
            bytes: 0,
        },
        output_complete: false,
        mutation_scope: "unknown".into(),
    };
    let mut state =
        crate::nano::private::file(&commands_dir.join("old-running-p1.json"), true).unwrap();
    state
        .write_all(&serde_json::to_vec(&snapshot).unwrap())
        .unwrap();
    state.sync_all().unwrap();
    drop(state);
    for stream in ["stdout", "stderr"] {
        drop(
            crate::nano::private::file(
                &commands_dir.join(format!("old-running-p1-{stream}")),
                true,
            )
            .unwrap(),
        );
    }

    let workspace =
        workspace::Workspace::new(session.workspace(), workspace::Limits::default()).unwrap();

    assert!(
        crate::nano::resume::recover_previous(&session, &workspace)
            .await
            .is_err()
    );
    assert_eq!(session.status().await.unwrap().status, "failed");
}

#[tokio::test]
async fn managed_heartbeat_renews_during_stalled_inference_then_stops_on_loss() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    configure(&f, &[], 3);
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    let (tools, journal) = native(&f, session.clone(), "managed-heartbeat");
    let mut provider = script(vec![]);
    provider.stall = true;
    let cancellation = Cancellation::default();
    let execution = managed::run(
        session.clone(),
        identity("managed-heartbeat"),
        Limits::default(),
        provider,
        tools,
        journal,
        &cancellation,
    );
    let lose = async {
        wait_for_renewal(&f).await;
        assert!(
            f.events()
                .iter()
                .any(|(kind, _)| kind == "task_lease_renewed")
        );
        f.connection()
            .execute("UPDATE tasks SET claimed_by = 'executor:other:1'", [])
            .unwrap();
    };
    let (result, _) = tokio::join!(execution, lose);
    assert_eq!(result.unwrap().reason, EndReason::AuthorityLost);
    assert!(session.authorize().await.is_err());
}

#[tokio::test]
async fn managed_claim_failure_prevents_the_first_inference() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    f.connection()
        .execute("UPDATE tasks SET claimed_by = 'executor:other:1'", [])
        .unwrap();
    let (tools, journal) = native(&f, session.clone(), "managed-unavailable");
    let provider = script(vec![]);
    let starts = provider.starts.clone();
    assert!(
        managed::run(
            session,
            identity("managed-unavailable"),
            Limits::default(),
            provider,
            tools,
            journal,
            &Cancellation::default()
        )
        .await
        .is_err()
    );
    assert_eq!(starts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn managed_lease_loss_stops_a_long_check_without_recording_a_pass() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let command = if cfg!(windows) {
        "ping -n 30 127.0.0.1 > nul"
    } else {
        "sleep 30"
    };
    configure(&f, &[command], 3);
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    let (tools, journal) = native(&f, session.clone(), "managed-long-check");
    let provider = script(vec![ToolCall {
        provider_call_id: "check".into(),
        name: "check".into(),
        arguments: "{}".into(),
    }]);
    let cancellation = Cancellation::default();
    let execute = managed::run(
        session.clone(),
        identity("managed-long-check"),
        Limits::default(),
        provider,
        tools,
        journal,
        &cancellation,
    );
    let steal = async {
        wait_for_renewal(&f).await;
        assert!(
            f.events()
                .iter()
                .any(|(kind, _)| kind == "task_lease_renewed")
        );
        f.connection()
            .execute("UPDATE tasks SET claimed_by = 'executor:other:1'", [])
            .unwrap();
    };
    let (result, _) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(execute, steal)
    })
    .await
    .unwrap();
    assert_eq!(result.unwrap().reason, EndReason::AuthorityLost);
    assert!(!f.events().iter().any(|(kind, _)| kind == "check_passed"));
    assert_eq!(session.status().await.unwrap().check_retries, 0);
}
