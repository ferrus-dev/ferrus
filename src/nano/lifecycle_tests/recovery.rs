//! Tests for recovery.

use super::*;

#[tokio::test]
async fn evaluation_launch_captures_task_checks_and_prelaunch_graph_state() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let suite: serde_json::Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/nano_eval/suite.json")).unwrap();
    let task = suite["workloads"]["local_bug_fix"]["task"]
        .as_str()
        .unwrap();
    std::fs::write(f.root.join(".ferrus/tasks/t-001.md"), format!("{task}\n")).unwrap();
    configure(&f, &["cargo test"], 3);

    let disabled = crate::nano::capture_evaluation_launch("local_bug_fix", &f.root, TASK)
        .await
        .unwrap();
    assert_eq!(disabled["cache"], "disabled");
    assert_eq!(
        disabled["check_commands_sha256"],
        crate::checks::commands_sha256(&["cargo test".into()]).unwrap()
    );

    let config = std::fs::read_to_string(f.root.join("ferrus.toml")).unwrap();
    std::fs::write(
        f.root.join("ferrus.toml"),
        format!("{config}\n[repository_graph]\nenabled = true\n"),
    )
    .unwrap();
    let cold = crate::nano::capture_evaluation_launch("local_bug_fix", &f.root, TASK)
        .await
        .unwrap();
    assert_eq!(cold["cache"], "cold");
    assert_eq!(cold["task_sha256"], disabled["task_sha256"]);
    std::fs::write(f.root.join(".ferrus/tasks/t-001.md"), "trivial task\n").unwrap();
    let changed = crate::nano::capture_evaluation_launch("local_bug_fix", &f.root, TASK)
        .await
        .unwrap();
    assert_ne!(changed["task_sha256"], cold["task_sha256"]);
}

#[cfg(feature = "nano-openai")]
#[tokio::test]
#[ignore = "requires an explicitly selected Nano journal, private config, and local loaded model"]
async fn live_lm_studio_replays_a_stopped_request_without_executing_tools() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let config =
        std::env::var_os("FERRUS_NANO_SMOKE_CONFIG").expect("Set FERRUS_NANO_SMOKE_CONFIG");
    let journal =
        std::env::var_os("FERRUS_NANO_SMOKE_JOURNAL").expect("Set FERRUS_NANO_SMOKE_JOURNAL");
    let config = crate::nano::config::Config::load(std::path::Path::new(&config)).unwrap();
    let (endpoint, settings) = config.validate().unwrap();
    assert!(matches!(
        endpoint.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]")
    ));
    let bytes = std::fs::read(journal).unwrap();
    assert!(bytes.len() <= 16 * 1024 * 1024);
    let records: Vec<Record> = bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    let failed = records
        .iter()
        .position(|record| {
            matches!(
                &record.event,
                SessionEvent::ModelFailed {
                    error: Some(ProviderErrorKind::Timeout),
                    ..
                }
            ) || matches!(&record.event, SessionEvent::ModelCompleted { response, .. } if response.finish == FinishReason::Length)
        })
        .expect("No timed-out or length-limited request");
    let start = records[..failed]
        .iter()
        .rposition(|record| matches!(record.event, SessionEvent::ModelStarted { .. }))
        .unwrap();
    let replay = crate::nano::replay::Replay::from_records(&records[..start]).unwrap();
    let recorded_effort = records[..start]
        .iter()
        .find_map(|record| match &record.event {
            SessionEvent::Started {
                provider: Some(settings),
                ..
            } => Some(settings.reasoning_effort),
            _ => None,
        })
        .expect("Missing recorded provider settings");
    let composition = records[..start]
        .iter()
        .rev()
        .find_map(|record| match &record.event {
            SessionEvent::ContextComposed { composition } => Some(composition),
            _ => None,
        })
        .unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    let (native, _) = native(&f, session.clone(), RUN);
    let tools = ManagedTools::new(session, native, Cancellation::default());
    let mut provider = crate::nano::providers::openai::OpenAi::new(config).unwrap();
    let mut request = ModelRequest {
        messages: replay
            .projection
            .unwrap_or_default()
            .apply(&replay.messages)
            .unwrap(),
        tools: tools.descriptors(),
        max_output_tokens: std::env::var("FERRUS_NANO_SMOKE_OUTPUT_TOKENS")
            .ok()
            .map(|value| value.parse().unwrap())
            .unwrap_or(composition.output_tokens_reserved),
    };
    let effort_bytes = |effort: Option<crate::nano::provider::ReasoningEffort>| {
        effort.map_or(0, |effort| {
            serde_json::to_vec(&serde_json::json!({"reasoning_effort":effort}))
                .unwrap()
                .len() as i64
                - 1
        })
    };
    let expected_bytes = composition.input_tokens_estimated as i64
        + request.max_output_tokens.to_string().len() as i64
        - composition.output_tokens_reserved.to_string().len() as i64
        + effort_bytes(settings.reasoning_effort)
        - effort_bytes(recorded_effort);
    assert_eq!(
        provider.estimate_input_tokens(&request).unwrap(),
        expected_bytes as u64,
        "Recorded request and current catalog/settings differ; do not treat this as an exact replay"
    );
    if std::env::var_os("FERRUS_NANO_SMOKE_CONTINUE_TRUNCATED").is_some() {
        let SessionEvent::ModelCompleted { response, .. } = &records[failed].event else {
            panic!("Continuation requires a completed length-limited response");
        };
        assert_eq!(response.finish, FinishReason::Length);
        request.messages.extend(response.messages());
        request.max_output_tokens = settings
            .max_output_tokens
            .unwrap_or(u64::MAX)
            .min(settings.context_tokens.unwrap_or(u64::MAX) / 4)
            .min(Limits::default().response_bytes as u64);
    }

    if let Some(path) = std::env::var_os("FERRUS_NANO_SMOKE_REQUEST_OUTPUT") {
        use std::io::Write;
        let mut file = crate::nano::private::file(std::path::Path::new(&path), true).unwrap();
        file.write_all(&provider.body(request).unwrap()).unwrap();
        return;
    }

    let start = tokio::time::Instant::now();
    let response = tokio::time::timeout(Duration::from_secs(600), async {
        provider.start(request).await.unwrap();
        loop {
            match provider
                .next_event()
                .await
                .unwrap()
                .expect("No completed response")
            {
                ProviderEvent::Completed { response, usage } => break (response, usage),
                _ => continue,
            }
        }
    })
    .await
    .expect("Replay exceeded its elapsed allowance");
    println!(
        "model={} elapsed_ms={} finish={:?} calls={:?} usage={:?}",
        settings.model,
        start.elapsed().as_millis(),
        response.0.finish,
        response
            .0
            .calls
            .iter()
            .map(|call| &call.name)
            .collect::<Vec<_>>(),
        response.1
    );
    assert_eq!(response.0.finish, FinishReason::ToolCalls);
    assert!(!response.0.calls.is_empty());
    if let Some(path) = std::env::var_os("FERRUS_NANO_SMOKE_RESPONSE_OUTPUT") {
        use std::io::Write;
        crate::nano::private::file(std::path::Path::new(&path), true)
            .unwrap()
            .write_all(&serde_json::to_vec(&response.0).unwrap())
            .unwrap();
    }

    for call in response.0.calls {
        tools
            .validate(&call.name, &serde_json::from_str(&call.arguments).unwrap())
            .unwrap();
    }
}

#[tokio::test]
async fn exhausted_phase_budgets_stop_dispatch_and_preserve_the_durable_reason() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    for (kind, code) in [
        (LimitKind::Tokens, "nano_limit_tokens"),
        (LimitKind::ModelTurns, "nano_limit_model_turns"),
        (LimitKind::ToolCalls, "nano_limit_tool_calls"),
        (LimitKind::NoProgress, "nano_limit_no_progress"),
    ] {
        let f = Fixture::new().await;
        let session = FerrusSession::bind(f.launch()).await.unwrap();
        let (tools, journal) = native(&f, session.clone(), RUN);
        let mut limits = Limits::default();
        match kind {
            LimitKind::Tokens => limits.tokens = 1,
            LimitKind::ModelTurns => limits.model_turns = 1,
            LimitKind::ToolCalls => limits.tool_calls = 1,
            LimitKind::NoProgress => limits.no_progress = 1,
            _ => unreachable!(),
        }

        let provider = script(
            (0..2)
                .map(|i| ToolCall {
                    provider_call_id: format!("read-{i}"),
                    name: "read_file".into(),
                    arguments: json!({"path":"missing.txt"}).to_string(),
                })
                .collect(),
        );
        let end = managed::run(
            session.clone(),
            identity(RUN),
            limits,
            provider,
            tools,
            journal,
            &Cancellation::default(),
        )
        .await
        .unwrap();
        assert_eq!(end.reason, EndReason::Limit(kind));
        assert!(end.durable);
        let state: (String, String, Option<String>, u32) = f
            .connection()
            .query_row(
                "SELECT status,failure_reason,claimed_by,check_retries FROM tasks WHERE id=?1",
                [TASK],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(state, ("failed".into(), code.into(), None, 0));
        assert!(matches!(
            session.claim().await.unwrap(),
            project::ReadyTaskClaim::NoAvailable
        ));
        let (_, records) =
            FileJournal::recover(&f.data.join("nano/sessions").join(RUN), Quotas::default())
                .unwrap();
        assert!(
            matches!(&records.last().unwrap().event, SessionEvent::Ended { reason } if reason == &end.reason)
        );
    }
}

#[tokio::test]
async fn recovery_reconciles_a_sealed_provider_stop_before_new_inference() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    for (reason, retryable) in [
        (EndReason::ProviderProtocol, false),
        (EndReason::ProviderFailed, true),
        (EndReason::Limit(LimitKind::Tokens), true),
        (EndReason::Limit(LimitKind::Retries), true),
    ] {
        let f = Fixture::new().await;
        let session = FerrusSession::bind(f.launch()).await.unwrap();
        session.claim().await.unwrap();
        let previous = "old-provider-stop";
        previous_run(&f, previous);
        let mut journal = FileJournal::create(&f.data, previous, Quotas::default()).unwrap();
        let mut budget = Budget::default();
        journal
            .append(
                SessionEvent::Started {
                    identity: SessionIdentity {
                        session_id: previous.into(),
                        project_id: "test-project".into(),
                        task_id: Some(TASK.into()),
                        run_id: Some(previous.into()),
                    },
                    limits: Limits {
                        tokens: 20,
                        retries: 0,
                        ..Default::default()
                    },
                    input: "task".into(),
                    system_prompt: None,
                    launch_evidence: None,
                    inherited_budget: None,
                    provider: None,
                },
                &budget,
            )
            .unwrap();
        budget.model_turns = 1;
        budget.reserved_input_tokens = 10;
        budget.reserved_output_tokens = 10;
        journal
            .append(SessionEvent::ModelStarted { turn: 1 }, &budget)
            .unwrap();
        let usage = Usage {
            input_tokens: 10,
            output_tokens: 10,
            reported: false,
        };
        budget.reserved_input_tokens = 0;
        budget.reserved_output_tokens = 0;
        budget.charge(&usage);
        journal
            .append(
                SessionEvent::ModelFailed {
                    retryable,
                    usage,
                    error: Some(if retryable {
                        ProviderErrorKind::Transport
                    } else {
                        ProviderErrorKind::Unsupported
                    }),
                    diagnostic: Some(ProviderDiagnostic::Http {
                        status: if retryable { 503 } else { 400 },
                    }),
                },
                &budget,
            )
            .unwrap();
        let terminal = reason.managed_failure_code(retryable).is_some();
        journal
            .append(SessionEvent::Ended { reason }, &budget)
            .unwrap();
        drop(journal);
        let workspace =
            workspace::Workspace::new(session.workspace(), workspace::Limits::default()).unwrap();
        let recovery = crate::nano::resume::recover_previous(&session, &workspace).await;
        if !terminal {
            assert!(recovery.unwrap().is_some());
            assert_eq!(session.status().await.unwrap().status, "executing");
        } else {
            assert!(recovery.is_err());
            assert_eq!(session.status().await.unwrap().status, "failed");
        }

        let (_, records) = FileJournal::recover(
            &f.data.join("nano/sessions").join(previous),
            Quotas::default(),
        )
        .unwrap();
        assert_eq!(records.len(), 4);
    }
}

#[tokio::test]
async fn historical_runs_after_the_latest_started_journal_do_not_exhaust_the_scan() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-read");
    previous_intent(&f, "old-read", "read_file", None, true);
    let connection = f.connection();
    for ordinal in 1..=64 {
        connection.execute(
            "INSERT INTO runs (rowid, id, task_id, role, agent, status, started_at, updated_at, workspace_path) \
             VALUES (?1, ?2, ?3, 'executor', ?4, 'failed', '2026-01-01', '2026-01-01', ?5)",
            rusqlite::params![-ordinal, format!("older-{ordinal}"), TASK, AGENT, f.root.to_string_lossy()],
        ).unwrap();
    }

    let workspace =
        workspace::Workspace::new(session.workspace(), workspace::Limits::default()).unwrap();

    let recovered = crate::nano::resume::recover_previous(&session, &workspace)
        .await
        .unwrap()
        .unwrap();
    assert!(recovered.note.contains("old-read"));
}

#[tokio::test]
async fn recovery_rejects_a_stolen_lease_before_touching_the_old_journal() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-stale");
    previous_intent(&f, "old-stale", "exec", None, true);
    let old = f.data.join("nano/sessions/old-stale/events.jsonl");
    let bytes = std::fs::read(&old).unwrap();
    f.connection()
        .execute(
            "UPDATE tasks SET claimed_by = 'executor:other:1' WHERE id = ?1",
            [TASK],
        )
        .unwrap();
    let workspace =
        workspace::Workspace::new(session.workspace(), workspace::Limits::default()).unwrap();

    assert!(
        crate::nano::resume::recover_previous(&session, &workspace)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(old).unwrap(), bytes);
    assert_eq!(session.status().await.unwrap().status, "executing");
}

#[tokio::test]
async fn recovery_does_not_skip_a_newer_run_from_another_workspace() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-matching");
    previous_intent(&f, "old-matching", "exec", None, true);
    f.connection()
        .execute("UPDATE runs SET rowid = -1 WHERE id = 'old-matching'", [])
        .unwrap();
    f.connection().execute(
        "INSERT INTO runs (rowid, id, task_id, role, agent, status, started_at, updated_at, workspace_path) \
         VALUES (0, 'newer-elsewhere', ?1, 'executor', ?2, 'failed', '2026-01-02', '2026-01-02', '/elsewhere')",
        rusqlite::params![TASK, AGENT],
    ).unwrap();
    let journal = f.data.join("nano/sessions/old-matching/events.jsonl");
    let bytes = std::fs::read(&journal).unwrap();
    let workspace =
        workspace::Workspace::new(session.workspace(), workspace::Limits::default()).unwrap();

    assert!(
        crate::nano::resume::recover_previous(&session, &workspace)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(std::fs::read(journal).unwrap(), bytes);
}

#[tokio::test]
async fn recovery_does_not_skip_a_newer_run_from_another_executor() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-matching");
    previous_intent(&f, "old-matching", "exec", None, true);
    f.connection()
        .execute("UPDATE runs SET rowid = -1 WHERE id = 'old-matching'", [])
        .unwrap();
    f.connection()
        .execute(
            "INSERT INTO runs (rowid, id, task_id, role, agent, status, started_at, updated_at, workspace_path) \
             VALUES (0, 'newer-agent', ?1, 'executor', 'other-agent', 'failed', '2026-01-02', '2026-01-02', ?2)",
            rusqlite::params![TASK, f.root.to_string_lossy()],
        )
        .unwrap();
    let journal = f.data.join("nano/sessions/old-matching/events.jsonl");
    let bytes = std::fs::read(&journal).unwrap();
    let workspace =
        workspace::Workspace::new(session.workspace(), workspace::Limits::default()).unwrap();

    assert!(
        crate::nano::resume::recover_previous(&session, &workspace)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(std::fs::read(journal).unwrap(), bytes);
}

#[tokio::test]
async fn committed_submit_can_be_reconciled_after_legacy_journal_sealing() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-submit-sealed");
    previous_intent(&f, "old-submit-sealed", "submit", None, true);
    f.connection()
        .execute(
            "UPDATE tasks SET status = 'addressing' WHERE id = ?1",
            [TASK],
        )
        .unwrap();
    f.connection()
        .execute(
            "INSERT INTO events (run_id, type, payload_json, created_at) \
         VALUES ('old-submit-sealed', 'submitted', '{}', '2026-01-01')",
            [],
        )
        .unwrap();
    let run_dir = f.root.join(".ferrus/runs/t-001");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("SUBMISSION.md"), "Ready for review.").unwrap();
    let directory = f.data.join("nano/sessions/old-submit-sealed");
    FileJournal::recover(&directory, Quotas::default()).unwrap();
    let workspace =
        workspace::Workspace::new(session.workspace(), workspace::Limits::default()).unwrap();

    assert!(
        crate::nano::resume::recover_previous(&session, &workspace)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(session.status().await.unwrap().status, "addressing");
}

#[tokio::test]
async fn managed_journal_records_effective_launch_flags_and_baseline() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = git_session(&f, false).await;
    let (mut tools, journal) = native(&f, session.clone(), "launch-evidence");
    tools.native_context_enabled = false;
    tools.working_set_enabled = false;
    let end = managed::run(
        session.clone(),
        identity("launch-evidence"),
        Limits::default(),
        script(vec![]),
        tools,
        journal,
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(end.reason, EndReason::ModelFinished);
    let (_, records) = FileJournal::recover(
        &f.data.join("nano/sessions/launch-evidence"),
        Quotas::default(),
    )
    .unwrap();
    assert!(matches!(
        &records[0].event,
        SessionEvent::Started { launch_evidence: Some(evidence), .. }
            if evidence.baseline_tree == session.baseline_tree().unwrap()
                && !evidence.native_context_enabled
                && !evidence.working_set_enabled
    ));
    let SessionEvent::Started {
        system_prompt,
        input,
        ..
    } = &records[0].event
    else {
        panic!("Missing session start");
    };
    assert_eq!(
        system_prompt.as_deref(),
        Some(crate::nano::instructions::ROLE_POLICY)
    );
    let task: Value = serde_json::from_str(input).unwrap();
    let documents = task["documents"].as_array().unwrap();
    assert!(documents.iter().any(|doc| doc["kind"] == "task"));
    assert!(documents.iter().all(|doc| doc["kind"] != "runtime_policy"));
}

#[tokio::test]
async fn taskless_prefetch_recovers_after_canonical_publication_but_not_refresh_failure() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let _managed = git_session(&f, true).await;
    project::record_task_status("current", ".ferrus/TASK.md", TaskStatus::Unknown)
        .await
        .unwrap();
    project::record_run_started_for_task_with_workspace(
        "direct-prefetch",
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
        run_id: "direct-prefetch".into(),
    };
    let journal = FileJournal::create(&f.data, "direct-prefetch", Quotas::default()).unwrap();
    let coding = CodingTools {
        workspace: workspace::Workspace::new(&f.root, workspace::Limits::default()).unwrap(),
        commands: commands::Commands::trusted_local(
            &f.root,
            "direct-prefetch",
            journal.directory(),
            commands::Limits::default(),
        )
        .unwrap(),
    };
    let mut tools = NativeTools::new(binding, coding, instructions::Limits::default()).unwrap();
    tools.prefetch = vec![json!({"type":"path", "value":"src/lib.rs"})];
    let patch = baseline_patch(&tools, "CanonicalPrefetchedEdit");
    assert!(
        matches!(tools.execute(&patch, &Cancellation::default()).await, ToolOutcome::Success(value) if value["complete"] == true)
    );
    let messages = [Message::User {
        text: "Inspect the changed symbol".into(),
    }];
    let prepared = tools
        .prepare_context(&messages, &Cancellation::default())
        .await
        .unwrap()
        .unwrap();
    assert!(
        prepared
            .observations
            .iter()
            .any(|item| item["kind"] == "canonical_refresh" && item["status"] == "published"),
        "{:?}",
        prepared.observations
    );
    assert!(
        prepared
            .observations
            .iter()
            .any(|item| item["kind"] == "prefetch"),
        "{:?}",
        prepared.observations
    );
    assert_eq!(
        project::canonical_graph_reference().await.unwrap().status,
        project::CanonicalGraphStatus::Fresh
    );

    std::fs::write(
        f.root.join("ferrus.toml"),
        "[repository_graph]\nenabled = 'invalid'\n",
    )
    .unwrap();
    tools.invalidate_unknown().await;
    let prepared = tools
        .prepare_context(&messages, &Cancellation::default())
        .await
        .unwrap()
        .unwrap();
    assert!(
        prepared
            .observations
            .iter()
            .any(|item| item["kind"] == "canonical_refresh" && item["status"] == "failed")
    );
    assert!(
        !prepared
            .observations
            .iter()
            .any(|item| item["kind"] == "prefetch")
    );
    assert_eq!(
        project::canonical_graph_reference().await.unwrap().status,
        project::CanonicalGraphStatus::Stale
    );
    let invalidations: i64 = f.connection().query_row(
        "SELECT COUNT(*) FROM events WHERE type = 'canonical_graph_invalidated' AND run_id = 'direct-prefetch'",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(invalidations, 2);
    assert!(tools.shutdown().await);
}
