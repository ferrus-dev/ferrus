//! Tests for review.

use super::*;

#[tokio::test]
async fn committed_submit_with_lost_response_is_confirmed_after_rejection() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-submit");
    previous_intent(&f, "old-submit", "submit", None, true);
    f.connection()
        .execute(
            "UPDATE tasks SET status = 'addressing' WHERE id = ?1",
            [TASK],
        )
        .unwrap();
    f.connection()
        .execute(
            "INSERT INTO events (run_id, type, payload_json, created_at) \
         VALUES ('old-submit', 'submitted', '{}', '2026-01-01')",
            [],
        )
        .unwrap();
    let run_dir = f.root.join(".ferrus/runs/t-001");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("SUBMISSION.md"), "Ready for review.").unwrap();
    let workspace =
        workspace::Workspace::new(session.workspace(), workspace::Limits::default()).unwrap();

    assert!(
        crate::nano::resume::recover_previous(&session, &workspace)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(session.status().await.unwrap().status, "addressing");
    let (_, records) =
        FileJournal::recover(&f.data.join("nano/sessions/old-submit"), Quotas::default()).unwrap();
    assert!(matches!(
        &records.last().unwrap().event,
        SessionEvent::Ended {
            reason: EndReason::Submitted
        }
    ));
    assert_eq!(
        f.events()
            .iter()
            .filter(|(kind, _)| kind == "submitted")
            .count(),
        1
    );
}

#[tokio::test]
async fn managed_operations_reject_foreign_expired_and_wrong_role_bindings() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    for sql in [
        "UPDATE tasks SET claimed_by = 'executor:other:1'",
        "UPDATE tasks SET claimed_by = 'executor:nano:1', lease_until = '2000-01-01T00:00:00Z'",
        "UPDATE runs SET role = 'supervisor'",
    ] {
        f.connection().execute(sql, []).unwrap();
        let events = f.events();
        assert!(
            lifecycle::check(&session, &Cancellation::default())
                .await
                .is_err()
        );
        assert!(
            lifecycle::submit(&session, "notes".into(), &Cancellation::default())
                .await
                .is_err()
        );
        assert!(
            lifecycle::ask(&session, true, "question".into())
                .await
                .is_err()
        );
        assert_eq!(f.events(), events);
    }
}

#[tokio::test]
async fn managed_tool_surface_has_no_approval_or_identity_arguments() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    let (native, _journal) = native(&f, session.clone(), "managed-tools");
    let mut tools = ManagedTools::new(session, native, Cancellation::default());
    assert!(
        !tools
            .descriptors()
            .iter()
            .any(|tool| tool.name == "approve")
    );
    assert!(
        tools
            .validate("submit", &json!({"content":"done", "task_id":"foreign"}))
            .is_err()
    );
    assert!(
        tools
            .validate("check", &json!({"workspace":"elsewhere"}))
            .is_err()
    );
    assert!(
        tools
            .validate("consult", &json!({"question":"Missing template"}))
            .is_err()
    );
    assert!(matches!(
        tools
            .execute(&call("check", json!({})), &Cancellation::default())
            .await,
        ToolOutcome::Success(_)
    ));
    assert!(tools.shutdown().await);
}

#[tokio::test]
async fn managed_submit_rejects_source_changes_during_gates() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    for with_git in [false, true] {
        let f = Fixture::new().await;
        configure(&f, &["echo changed >> source.txt"], 3);
        let session = if with_git {
            git_session(&f, false).await
        } else {
            let session = FerrusSession::bind(f.launch()).await.unwrap();
            session.claim().await.unwrap();
            session
        };
        std::fs::write(f.root.join("source.txt"), "before").unwrap();
        let error = lifecycle::submit(&session, "notes".into(), &Cancellation::default())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Workspace changed"), "{error:#}");
        assert_eq!(session.status().await.unwrap().status, "executing");
        assert!(!f.root.join(".ferrus/runs/t-001/SUBMISSION.md").exists());
    }
}

#[tokio::test]
#[cfg(feature = "nano-openai")]
async fn direct_mutations_and_running_commands_share_the_canonical_approval_lock() {
    use crate::server::tools::approve::acquire_canonical_approval_lock_at;
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let _managed = git_session(&f, false).await;
    project::record_task_status("current", ".ferrus/TASK.md", TaskStatus::Unknown)
        .await
        .unwrap();
    project::record_run_started_for_task_with_workspace(
        "direct-lock",
        "executor",
        "executor:nano:1",
        std::process::id(),
        Some("current"),
        f.root.to_string_lossy().into_owned(),
    )
    .await
    .unwrap();
    let binding = crate::nano::binding::Binding::Interactive {
        definition: None,
        project_root: f.root.clone(),
        project_id: "test-project".into(),
        data_dir: f.data.clone(),
        agent_id: "executor:nano:1".into(),
        run_id: "direct-lock".into(),
    };
    let journal = FileJournal::create(&f.data, "direct-lock", Quotas::default()).unwrap();
    let coding = CodingTools {
        workspace: workspace::Workspace::new(&f.root, workspace::Limits::default()).unwrap(),
        commands: commands::Commands::trusted_local(
            &f.root,
            "direct-lock",
            journal.directory(),
            commands::Limits::default(),
        )
        .unwrap(),
    };
    let mut native =
        NativeTools::new(binding.clone(), coding, instructions::Limits::default()).unwrap();
    native.working_set_enabled = false;
    let patch = baseline_patch(&native, "DirectLockedEdit");
    let mut tools =
        crate::nano::interactive::InteractiveTools::new(native, binding, Cancellation::default());
    let lock_path = f.data.join("canonical-approval.lock");
    let approval = acquire_canonical_approval_lock_at(&lock_path, "t-review")
        .await
        .unwrap();
    let before = std::fs::read(f.root.join("src/lib.rs")).unwrap();
    let cancelled = Cancellation::default();
    let signal = cancelled.clone();
    let cancel = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        signal.cancel();
    });
    assert_eq!(
        tools.execute(&patch, &cancelled).await,
        ToolOutcome::Failed(ToolError::Interrupted)
    );
    cancel.await.unwrap();
    assert_eq!(std::fs::read(f.root.join("src/lib.rs")).unwrap(), before);
    drop(approval);
    assert!(matches!(
        tools.execute(&patch, &Cancellation::default()).await,
        ToolOutcome::Success(_)
    ));
    assert!(!lock_path.exists());
    let short = ValidatedCall {
        call_id: "short".into(),
        provider_call_id: "short".into(),
        name: "exec".into(),
        arguments: json!({"command":"echo done", "cwd":".", "timeout_ms":60000}),
    };
    assert!(matches!(
        tools.execute(&short, &Cancellation::default()).await,
        ToolOutcome::Success(_)
    ));
    // The process guard must release without another model turn or tool call.
    let approval = tokio::time::timeout(
        Duration::from_secs(5),
        acquire_canonical_approval_lock_at(&lock_path, "t-review"),
    )
    .await
    .unwrap()
    .unwrap();
    drop(approval);
    #[cfg(unix)]
    let command = "sleep 30";
    #[cfg(windows)]
    let command = "ping -n 30 127.0.0.1 >nul";
    let exec = ValidatedCall {
        call_id: "exec".into(),
        provider_call_id: "exec".into(),
        name: "exec".into(),
        arguments: json!({"command":command, "cwd":".", "timeout_ms":60000}),
    };
    assert!(matches!(
        tools.execute(&exec, &Cancellation::default()).await,
        ToolOutcome::Success(_)
    ));
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            acquire_canonical_approval_lock_at(&lock_path, "t-review")
        )
        .await
        .is_err()
    );
    assert!(tools.shutdown().await);
    let approval = tokio::time::timeout(
        Duration::from_secs(5),
        acquire_canonical_approval_lock_at(&lock_path, "t-review"),
    )
    .await
    .unwrap()
    .unwrap();
    drop(approval);
    assert!(!lock_path.exists());
}
