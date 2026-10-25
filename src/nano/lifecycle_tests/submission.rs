//! Tests for submission.

use super::*;

#[cfg(feature = "nano-openai")]
#[tokio::test]
#[ignore = "requires a private config and a local model; executes native tools in a temporary project"]
async fn live_lm_studio_managed_graph_edit_check_and_submit() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let path = std::env::var_os("FERRUS_NANO_SMOKE_CONFIG").expect("Set FERRUS_NANO_SMOKE_CONFIG");
    let config = crate::nano::config::Config::load(std::path::Path::new(&path)).unwrap();
    let (endpoint, settings) = config.validate().unwrap();
    assert!(matches!(
        endpoint.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]")
    ));
    #[cfg(feature = "nano-mcp")]
    assert!(
        config.mcp_config_file.is_none(),
        "This smoke executes only native tools"
    );
    let f = Fixture::new().await;
    configure(&f, &["git diff --check"], 3);
    // Optional copied workload. Fixed paths stay in a disposable project; never
    // launch the agent in the original project or copy its runtime database.
    let case = std::env::var_os("FERRUS_NANO_SMOKE_CASE_DIR").map(std::path::PathBuf::from);
    let original_html = if let Some(case) = &case {
        assert!(case.is_absolute());
        for path in ["static/index.html", "docs/spec.md", "AGENTS.md"] {
            let source = case.join(path);
            if source.exists() {
                assert!(std::fs::metadata(&source).unwrap().len() <= 32 * 1024);
                let target = f.root.join(path);
                std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                std::fs::copy(source, target).unwrap();
            }
        }

        Some(std::fs::read_to_string(f.root.join("static/index.html")).unwrap())
    } else {
        None
    };
    let session = git_session(&f, true).await;
    let task = if let Some(case) = &case {
        std::fs::read_to_string(case.join("TASK.md")).unwrap()
    } else {
        "First locate Baseline using repository_search. In src/lib.rs rename the Baseline struct to Ready, keeping the rest of the file unchanged. Use native apply_patch for the edit. Run the Ferrus check tool and submit the change using the submit tool. Do not commit or ask for consultation.\n".into()
    };
    assert!(task.len() <= 32 * 1024);
    std::fs::write(f.root.join(".ferrus/tasks/t-001.md"), task).unwrap();
    let (tools, journal) = native(&f, session.clone(), RUN);
    let start = tokio::time::Instant::now();
    let end = managed::run(
        session.clone(),
        identity(RUN),
        Limits {
            elapsed_ms: if case.is_some() { 600_000 } else { 180_000 },
            model_turns: if case.is_some() { 32 } else { 12 },
            tokens: config.session_tokens,
            ..Limits::default()
        },
        crate::nano::providers::openai::OpenAi::new(config).unwrap(),
        tools,
        journal,
        &Cancellation::default(),
    )
    .await
    .unwrap();
    println!(
        "model={} elapsed_ms={} end={:?} turns={} tool_calls={} input={} output={}",
        settings.model,
        start.elapsed().as_millis(),
        end.reason,
        end.budget.model_turns,
        end.budget.tool_calls,
        end.budget.reported_input_tokens,
        end.budget.reported_output_tokens
    );
    let (_, records) =
        FileJournal::recover(&f.data.join("nano/sessions").join(RUN), Quotas::default()).unwrap();
    if let Some(path) = std::env::var_os("FERRUS_NANO_SMOKE_JOURNAL_OUTPUT") {
        use std::io::Write;
        let mut output = crate::nano::private::file(std::path::Path::new(&path), true).unwrap();
        for record in &records {
            writeln!(output, "{}", serde_json::to_string(record).unwrap()).unwrap();
        }
    }

    let calls: Vec<_> = records
        .iter()
        .filter_map(|record| match &record.event {
            SessionEvent::ToolIntent { call, .. } => Some(call.name.as_str()),
            _ => None,
        })
        .collect();
    println!("calls={calls:?}");
    let failures: Vec<_> = records
        .iter()
        .filter_map(|record| match &record.event {
            SessionEvent::ToolResult {
                outcome: ToolOutcome::Failed(error),
                ..
            } => Some(error),
            _ => None,
        })
        .collect();
    println!("tool_failures={failures:?}");
    assert_eq!(end.reason, EndReason::Submitted);
    assert!(end.durable);
    assert_eq!(session.status().await.unwrap().status, "reviewing");
    if let Some(original) = original_html {
        assert_ne!(
            std::fs::read_to_string(f.root.join("static/index.html")).unwrap(),
            original
        );
    } else {
        assert_eq!(
            std::fs::read_to_string(f.root.join("src/lib.rs")).unwrap(),
            "pub struct Ready;\n"
        );
        assert!(calls.contains(&"repository_search"));
    }

    for required in ["apply_patch", "check", "submit"] {
        assert!(calls.contains(&required), "Missing native call: {required}");
    }
}

#[tokio::test]
async fn truncated_reasoning_response_continues_to_submit_with_usage_preserved() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    let (tools, journal) = native(&f, session.clone(), RUN);
    let provider = Script {
        responses: VecDeque::from([
            ProviderEvent::Completed {
                response: ModelResponse {
                    finish: FinishReason::Length,
                    text: String::new(),
                    calls: Vec::new(),
                    continuation: Some(json!({"reasoning_content":"unfinished reasoning"})),
                },
                usage: Some(Usage {
                    input_tokens: 10,
                    output_tokens: 4096,
                    reported: true,
                }),
            },
            ProviderEvent::Completed {
                response: ModelResponse {
                    finish: FinishReason::ToolCalls,
                    text: String::new(),
                    calls: vec![ToolCall {
                        provider_call_id: "submit".into(),
                        name: "submit".into(),
                        arguments: json!({"content":"## Summary\nDone"}).to_string(),
                    }],
                    continuation: None,
                },
                usage: Some(Usage {
                    input_tokens: 10,
                    output_tokens: 20,
                    reported: true,
                }),
            },
        ]),
        starts: Arc::new(AtomicUsize::new(0)),
        stall: false,
    };
    let end = managed::run(
        session.clone(),
        identity(RUN),
        Limits::default(),
        provider,
        tools,
        journal,
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(end.reason, EndReason::Submitted);
    assert_eq!(end.budget.reported_output_tokens, 4116);
    assert_eq!(end.budget.tool_calls, 1);
    assert!(end.durable);
    let state: (String, Option<String>, Option<String>) = f
        .connection()
        .query_row(
            "SELECT status,failure_reason,claimed_by FROM tasks WHERE id=?1",
            [TASK],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(state, ("reviewing".into(), None, None));
    assert!(matches!(
        session.claim().await.unwrap(),
        project::ReadyTaskClaim::NoAvailable
    ));
}

#[tokio::test]
async fn completed_submit_starts_a_new_phase_without_inheriting_usage() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-submitted");
    previous_intent(&f, "old-submitted", "submit", None, true);
    finish_previous_submit(&f, "old-submitted");
    f.connection()
        .execute(
            "UPDATE tasks SET status = 'addressing' WHERE id = ?1",
            [TASK],
        )
        .unwrap();
    let journal = f.data.join("nano/sessions/old-submitted/events.jsonl");
    let before = std::fs::read(&journal).unwrap();
    let workspace =
        workspace::Workspace::new(session.workspace(), workspace::Limits::default()).unwrap();

    assert!(
        crate::nano::resume::recover_previous(&session, &workspace)
            .await
            .is_err()
    );
    f.connection()
        .execute(
            "INSERT INTO events (run_id, type, payload_json, created_at) \
             VALUES ('old-submitted', 'submitted', '{}', '2026-01-01')",
            [],
        )
        .unwrap();
    assert!(
        crate::nano::resume::recover_previous(&session, &workspace)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(std::fs::read(journal).unwrap(), before);
    assert_eq!(session.status().await.unwrap().status, "addressing");
}

#[tokio::test]
async fn inconsistent_submitted_artifact_is_not_certified() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-mismatch");
    previous_intent(&f, "old-mismatch", "submit", None, true);
    f.connection()
        .execute(
            "INSERT INTO events (run_id, type, payload_json, created_at) \
         VALUES ('old-mismatch', 'submitted', '{}', '2026-01-01')",
            [],
        )
        .unwrap();
    let run_dir = f.root.join(".ferrus/runs/t-001");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("SUBMISSION.md"), "changed after commit").unwrap();
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
async fn managed_submit_runs_both_gates_and_never_repeats_a_handoff() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    // Runtime output is deliberately outside the source tree checked for changes.
    configure(&f, &["echo gate >> .ferrus/gates.txt"], 3);
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    let result = lifecycle::submit(
        &session,
        "## Summary\nImplemented".into(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(result["status"], "submitted");
    assert_eq!(
        std::fs::read_to_string(f.root.join(".ferrus/gates.txt"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    assert_eq!(session.status().await.unwrap().status, "reviewing");
    let events = f.events();
    let committed = events
        .iter()
        .find(|(kind, _)| kind == "submission_committed")
        .unwrap();
    assert_eq!(
        committed.1["check_commands_sha256"],
        crate::checks::commands_sha256(&["echo gate >> .ferrus/gates.txt".into()]).unwrap()
    );
    assert!(
        lifecycle::submit(&session, "replacement".into(), &Cancellation::default())
            .await
            .is_err()
    );
    assert_eq!(f.events(), events);
    assert!(
        std::fs::read_to_string(f.root.join(".ferrus/runs/t-001/SUBMISSION.md"))
            .unwrap()
            .contains("Implemented")
    );
}

#[tokio::test]
async fn managed_submit_failure_keeps_work_open_without_submission_artifacts() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    configure(&f, &["exit 1"], 3);
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    let result = lifecycle::submit(&session, "notes".into(), &Cancellation::default())
        .await
        .unwrap();
    assert_eq!(result["status"], "failed");
    assert_eq!(session.status().await.unwrap().status, "executing");
    assert_eq!(session.status().await.unwrap().check_retries, 1);
    assert!(!f.root.join(".ferrus/runs/t-001/SUBMISSION.md").exists());
}

#[tokio::test]
async fn managed_engine_stops_at_submit_before_later_calls_or_inference() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    let (tools, journal) = native(&f, session.clone(), "managed-submit");
    let provider = script(vec![
        ToolCall {
            provider_call_id: "submit".into(),
            name: "submit".into(),
            arguments: json!({"content":"## Summary\nDone"}).to_string(),
        },
        ToolCall {
            provider_call_id: "later".into(),
            name: "ask_human".into(),
            arguments: json!({"question":"Must never execute"}).to_string(),
        },
    ]);
    let starts = provider.starts.clone();
    let result = managed::run(
        session.clone(),
        identity("managed-submit"),
        Limits::default(),
        provider,
        tools,
        journal,
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(result.reason, EndReason::Submitted);
    assert!(result.durable);
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    assert_eq!(result.budget.tool_calls, 1);
    assert_eq!(session.status().await.unwrap().status, "reviewing");
    assert!(!f.root.join(".ferrus/runs/t-001/QUESTION.md").exists());
}

#[tokio::test]
async fn managed_submit_freezes_and_pins_the_exact_graph_tree() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = git_session(&f, true).await;
    std::fs::write(f.root.join("src/lib.rs"), "pub struct Submitted;\n").unwrap();
    let checked = crate::repository_graph::source::capture_worktree_tree(&f.root).unwrap();
    let result = lifecycle::submit(
        &session,
        "## Summary\nChanges".into(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(result["status"], "submitted");
    let context = session.status().await.unwrap();
    assert_eq!(
        context.repository_view.lifecycle,
        crate::repository_graph::domain::TaskViewLifecycle::FrozenSubmitted
    );
    assert_eq!(
        context.repository_view.frozen_source_tree,
        Some(checked.clone())
    );
    assert!(
        f.events()
            .iter()
            .any(|(kind, _)| kind == "repository_view_frozen")
    );
    let refs = git(&f, &["for-each-ref", "--format=%(refname) %(objectname)"]);
    assert!(refs.contains("refs/ferrus/reviews/"));
    assert!(refs.contains(checked.value()));
    std::fs::write(f.root.join("src/lib.rs"), "pub struct Later;\n").unwrap();
    assert_eq!(
        session.status().await.unwrap().repository_view,
        context.repository_view
    );
}

#[tokio::test]
async fn managed_final_gate_failure_preserves_submit_retry_accounting() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let command = if cfg!(windows) {
        "if exist .ferrus\\first-gate.txt (exit /b 1) else (echo passed > .ferrus\\first-gate.txt)"
    } else {
        "if test -f .ferrus/first-gate.txt; then exit 1; else echo passed > .ferrus/first-gate.txt; fi"
    };
    configure(&f, &[command], 3);
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    let result = lifecycle::submit(&session, "notes".into(), &Cancellation::default())
        .await
        .unwrap();
    assert_eq!(result["status"], "failed");
    assert_eq!(result["retries"], 1);
    let context = session.status().await.unwrap();
    assert_eq!(context.status, "executing");
    assert_eq!(
        context.failure_reason,
        Some(format!("Commands failed: {command}"))
    );
    let events = f.events();
    assert_eq!(
        events
            .iter()
            .filter(|(kind, _)| kind == "check_passed")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|(kind, _)| kind == "submit_check_failed")
            .count(),
        1
    );
    assert!(!f.root.join(".ferrus/runs/t-001/SUBMISSION.md").exists());
}

#[tokio::test]
async fn managed_isolated_submit_writes_canonical_artifacts_from_the_checked_worktree() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let original = git_session(&f, false).await;
    git(&f, &["add", "ferrus.toml", "src/lib.rs"]);
    git(
        &f,
        &[
            "-c",
            "user.name=Nano Test",
            "-c",
            "user.email=nano@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "baseline",
        ],
    );
    let workspace = f.data.join("worktrees").join(TASK);
    // Git for Windows rejects verbatim absolute paths when creating a worktree.
    // The git helper runs in f.root; keep the runtime binding absolute below.
    let relative_workspace = workspace.strip_prefix(&f.root).unwrap();
    git(
        &f,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            relative_workspace.to_str().unwrap(),
            "HEAD",
        ],
    );
    std::fs::create_dir_all(workspace.join(".ferrus")).unwrap();
    std::fs::copy(
        f.root.join(".ferrus/project.toml"),
        workspace.join(".ferrus/project.toml"),
    )
    .unwrap();
    f.connection()
        .execute(
            "UPDATE runs SET workspace_path = ?1 WHERE id = ?2",
            [workspace.to_str().unwrap(), RUN],
        )
        .unwrap();
    let mut launch = f.launch();
    launch.workspace = workspace.clone();
    launch.baseline_tree = original.baseline_tree().map(str::to_owned);
    let session = FerrusSession::bind(launch).await.unwrap();
    std::fs::write(workspace.join("src/lib.rs"), "pub struct IsolatedChange;\n").unwrap();
    let unrelated = tempfile::tempdir().unwrap();
    std::env::set_current_dir(unrelated.path()).unwrap();
    let result = lifecycle::submit(
        &session,
        "Isolated submission".into(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(result["status"], "submitted");
    let patch = std::fs::read_to_string(f.root.join(".ferrus/runs/t-001/PATCH.diff")).unwrap();
    assert!(patch.contains("+pub struct IsolatedChange;"));
    assert!(!patch.contains("project.toml"));
    assert!(!workspace.join(".ferrus/runs/t-001/SUBMISSION.md").exists());
    assert!(
        std::fs::read_to_string(f.root.join("src/lib.rs"))
            .unwrap()
            .contains("Baseline")
    );
}
