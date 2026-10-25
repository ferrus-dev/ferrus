//! Tests for cancellation.

use super::*;

#[tokio::test]
async fn uncommitted_submit_cannot_hide_interrupted_check_commands() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-submit");
    previous_intent(&f, "old-submit", "submit", None, true);
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
async fn managed_waits_keep_heartbeat_and_cancel_as_paused() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    configure(&f, &[], 3);
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    let (tools, journal) = native(&f, session.clone(), "managed-wait");
    let provider = script(vec![ToolCall {
        provider_call_id: "ask".into(),
        name: "ask_human".into(),
        arguments: json!({"question":"Need a decision"}).to_string(),
    }]);
    let starts = provider.starts.clone();
    let cancellation = Cancellation::default();
    let execution = managed::run(
        session.clone(),
        identity("managed-wait"),
        Limits::default(),
        provider,
        tools,
        journal,
        &cancellation,
    );
    let cancel = async {
        wait_for_renewal(&f).await;
        assert_eq!(session.status().await.unwrap().status, "awaiting_human");
        assert!(
            f.events()
                .iter()
                .any(|(kind, _)| kind == "task_lease_renewed")
        );
        cancellation.cancel();
    };
    let (result, _) = tokio::join!(execution, cancel);
    assert_eq!(result.unwrap().reason, EndReason::Paused);
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    assert!(f.root.join(".ferrus/runs/t-001/QUESTION.md").exists());
}

#[tokio::test]
async fn managed_check_quiesces_writers_and_allows_later_commands() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    let (native, _journal) = native(&f, session.clone(), "managed-writers");
    let mut tools = ManagedTools::new(session, native, Cancellation::default());
    let command = if cfg!(windows) {
        "ping -n 30 127.0.0.1 > nul"
    } else {
        "sleep 30"
    };
    let started = tools
        .native
        .coding
        .commands
        .exec(
            commands::ExecRequest {
                command: command.into(),
                cwd: ".".into(),
                timeout_ms: 30_000,
            },
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        tools
            .execute(&call("check", json!({})), &Cancellation::default())
            .await,
        ToolOutcome::Success(_)
    ));
    let stopped = tools
        .native
        .coding
        .commands
        .read_process(&started.process_id, 0)
        .await
        .unwrap();
    assert!(!stopped.potentially_writing());
    assert!(
        tools
            .native
            .coding
            .commands
            .exec(
                commands::ExecRequest {
                    command: "echo still-open".into(),
                    cwd: ".".into(),
                    timeout_ms: 1000
                },
                &Cancellation::default()
            )
            .await
            .is_ok()
    );
    assert!(tools.shutdown().await);
}

#[tokio::test]
async fn managed_interrupt_reconciles_committed_submit_before_delivery() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    let (native, _journal) = native(&f, session.clone(), "managed-race");
    let stop = Cancellation::default();
    let mut tools = ManagedTools::new(session.clone(), native, stop.clone());
    let committing = session.clone();
    tools.pending = Some(tokio::spawn(async move {
        let result = lifecycle::submit(&committing, "Original submission".into(), &stop).await?;
        // Hold delivery after the real SQLite commit until the engine interrupts.
        stop.cancelled().await;
        Ok(result)
    }));
    tokio::time::timeout(Duration::from_secs(5), async {
        while session.status().await.unwrap().status != "reviewing" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let events = f.events();
    assert!(
        matches!(tools.interrupted().await, Some(ToolOutcome::Success(value)) if value["status"] == "submitted")
    );
    assert_eq!(tools.end_reason(), Some(EndReason::Submitted));
    assert!(tools.shutdown().await);
    assert_eq!(f.events(), events);
    assert_eq!(
        std::fs::read_to_string(f.root.join(".ferrus/runs/t-001/SUBMISSION.md")).unwrap(),
        "Original submission"
    );
}

#[tokio::test]
async fn shutdown_refreshes_an_overlay_deferred_by_an_active_writer() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = git_session(&f, true).await;
    let previous = session
        .status()
        .await
        .unwrap()
        .repository_view
        .view_snapshot_id;
    let (mut tools, _journal) = native(&f, session.clone(), "writer-at-shutdown");
    let command = if cfg!(windows) {
        r"echo pub struct ShutdownEdit;> src\lib.rs & ping -n 30 127.0.0.1 > nul"
    } else {
        "printf 'pub struct ShutdownEdit;\\n' > src/lib.rs; sleep 30"
    };
    let started = tools
        .execute(
            &call(
                "exec",
                json!({"command":command, "cwd":".", "timeout_ms":30_000}),
            ),
            &Cancellation::default(),
        )
        .await;
    assert!(matches!(started, ToolOutcome::Success(_)));
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if std::fs::read_to_string(f.root.join("src/lib.rs"))
                .is_ok_and(|source| source.contains("ShutdownEdit"))
            {
                break;
            }

            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("command must write before shutdown");
    assert!(tools.coding.commands.potentially_active_writers() > 0);
    assert_eq!(
        session
            .status()
            .await
            .unwrap()
            .repository_view
            .view_snapshot_id,
        previous
    );
    assert!(tools.shutdown().await);
    assert_ne!(
        session
            .status()
            .await
            .unwrap()
            .repository_view
            .view_snapshot_id,
        previous
    );
}
