//! Tests for recovery.

use super::*;

#[tokio::test]
async fn system_policy_is_durable_and_separate_from_task_and_tool_history() {
    let (_dir, mut engine) = setup(
        scripted(vec![
            response("", vec![call("a", 1)]),
            response("Done", vec![]),
        ]),
        limits(),
    );
    assert!(engine.set_system_prompt(" ").is_err());
    engine.set_system_prompt("Host policy").unwrap();
    let end = run(&mut engine, &Cancellation::default()).await;
    assert_eq!(end.reason, EndReason::ModelFinished);
    assert!(engine.set_system_prompt("Replacement").is_err());
    for request in &engine.provider.requests {
        assert_eq!(
            request.messages[0],
            Message::System {
                text: "Host policy".into()
            }
        );
        assert_eq!(
            request.messages[1],
            Message::User {
                text: "Implement a fixture task".into()
            }
        );
        assert!(super::super::compaction::boundaries(&request.messages).is_ok());
    }

    let replay = Replay::from_records(&engine.host.records).unwrap();
    assert_eq!(
        &replay.messages[..2],
        &engine.provider.requests[0].messages[..2]
    );
    assert!(
        matches!(&engine.host.records[0].event, SessionEvent::Started { system_prompt: Some(prompt), .. } if prompt == "Host policy")
    );
    let mut legacy = serde_json::to_value(&engine.host.records[0]).unwrap();
    legacy["event"]
        .as_object_mut()
        .unwrap()
        .remove("system_prompt");
    let record: Record = serde_json::from_value(legacy).unwrap();
    let legacy_replay = Replay::from_records(&[record]).unwrap();
    assert!(matches!(
        &legacy_replay.messages[..],
        [Message::User { .. }]
    ));
}

#[tokio::test]
async fn journal_failures_stop_unrecorded_effects_and_uncommitted_acknowledgments() {
    for after_effect in [false, true] {
        let dir = TempDir::new().unwrap();
        let inner = FileJournal::create(
            &dir.path().canonicalize().unwrap(),
            "session-1",
            Quotas::default(),
        )
        .unwrap();
        let path = inner.directory().to_path_buf();
        let journal = FailingJournal {
            inner,
            fail_intent: !after_effect,
            fail_result: after_effect,
        };
        let mut engine = Engine::new(
            identity(),
            limits(),
            scripted(vec![response("", vec![call("a", 1), call("b", 2)])]),
            FakeTools::default(),
            FakeHost::default(),
            journal,
        )
        .unwrap();
        let end = run(&mut engine, &Cancellation::default()).await;
        assert_eq!(end.reason, EndReason::JournalFailed);
        assert!(!end.durable);
        assert_eq!(
            engine.tools.effects.lock().unwrap().len(),
            usize::from(after_effect)
        );
        assert!(
            engine
                .host
                .records
                .iter()
                .all(|r| !matches!(r.event, SessionEvent::ToolResult { .. }))
        );
        drop(engine);
        let (journal, records) = FileJournal::recover(&path, Quotas::default()).unwrap();
        assert_eq!(
            journal.state().pending_effect.as_deref(),
            after_effect.then_some("call-1")
        );
        assert_eq!(
            Replay::from_records(&records).unwrap().end,
            Some(EndReason::Limit(LimitKind::Elapsed))
        );
    }
}

#[tokio::test]
async fn recorded_replay_is_deterministic_preserves_budgets_and_has_no_effect_ports() {
    let (_dir, mut engine) = setup(
        scripted(vec![
            response("", vec![call("a", 1), call("b", 2)]),
            response("Done", vec![]),
        ]),
        limits(),
    );
    run(&mut engine, &Cancellation::default()).await;
    let records = engine.host.records.clone();
    let effects = engine.tools.effects.clone();
    let expected = effects.lock().unwrap().clone();
    drop(engine);
    let first = Replay::from_records(&records).unwrap();
    let second = Replay::from_records(&records).unwrap();
    assert_eq!(first.messages, second.messages);
    assert_eq!(first.budget, second.budget);
    assert_eq!(first.end, Some(EndReason::ModelFinished));
    assert_eq!(*effects.lock().unwrap(), expected);
    let mut undercharged = records.clone();
    let completed = undercharged
        .iter_mut()
        .find(|record| matches!(record.event, SessionEvent::ModelCompleted { .. }))
        .unwrap();
    completed.budget.reported_input_tokens = 0;
    assert!(Replay::from_records(&undercharged).is_err());
    let mut reordered = records.clone();
    let results: Vec<_> = reordered
        .iter()
        .enumerate()
        .filter(|(_, r)| matches!(r.event, SessionEvent::ToolResult { .. }))
        .map(|(i, _)| i)
        .collect();
    let event = reordered[results[0]].event.clone();
    reordered[results[0]].event = reordered[results[1]].event.clone();
    reordered[results[1]].event = event;
    assert!(Replay::from_records(&reordered).is_err());
}

#[tokio::test]
async fn replay_requires_a_final_response_from_the_latest_model_attempt() {
    for case in [
        "started",
        "failed",
        "empty",
        "whitespace",
        "length",
        "tool_calls",
        "tools_done",
        "stale",
        "valid",
    ] {
        let (_dir, mut engine) = setup(
            scripted(vec![
                response("", vec![call("a", 1)]),
                response("Done", vec![]),
            ]),
            limits(),
        );
        run(&mut engine, &Cancellation::default()).await;
        let mut records = engine.host.records.clone();
        records.pop(); // Replace the engine's ending with an owner-written one.
        match case {
            "started" => records.truncate(1),
            "tools_done" => {
                let next = records
                    .iter()
                    .position(|r| matches!(r.event, SessionEvent::ModelStarted { turn: 2 }))
                    .unwrap();
                records.truncate(next);
            }
            "stale" => {
                let mut next = records.last().unwrap().clone();
                next.sequence += 1;
                next.budget.model_turns += 1;
                next.budget.reserved_input_tokens = 10;
                next.budget.reserved_output_tokens = 5;
                next.event = SessionEvent::ModelStarted {
                    turn: next.budget.model_turns,
                };
                records.push(next.clone());
                next.sequence += 1;
                next.budget.reserved_input_tokens = 0;
                next.budget.reserved_output_tokens = 0;
                let usage = Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    reported: true,
                };
                next.budget.charge(&usage);
                next.event = SessionEvent::ModelFailed {
                    error: None,
                    diagnostic: None,
                    retryable: false,
                    usage,
                };
                records.push(next);
            }
            "valid" => (),
            _ => {
                let record = records.last_mut().unwrap();
                let SessionEvent::ModelCompleted { response, usage } = &mut record.event else {
                    panic!("Expected final response")
                };
                match case {
                    "failed" => {
                        record.event = SessionEvent::ModelFailed {
                            error: None,
                            diagnostic: None,
                            retryable: false,
                            usage: usage.clone(),
                        }
                    }
                    "empty" => response.text.clear(),
                    "whitespace" => response.text = " \t\n".into(),
                    "length" => response.finish = FinishReason::Length,
                    "tool_calls" => response.finish = FinishReason::ToolCalls,
                    _ => unreachable!(),
                }
            }
        }

        // The prefix remains valid; only the fabricated successful ending is invalid.
        Replay::from_records(&records).unwrap();
        let mut end = records.last().unwrap().clone();
        end.sequence += 1;
        end.event = SessionEvent::Ended {
            reason: EndReason::ModelFinished,
        };
        records.push(end);
        assert_eq!(
            Replay::from_records(&records).is_ok(),
            case == "valid",
            "{case}"
        );
        let directory = engine.journal.directory().to_path_buf();
        drop(engine);
        let mut bytes = Vec::new();
        for record in &records {
            serde_json::to_writer(&mut bytes, record).unwrap();
            bytes.push(b'\n');
        }

        let path = directory.join("events.jsonl");
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(
            FileJournal::recover(&directory, Quotas::default()).is_ok(),
            case == "valid",
            "{case}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[tokio::test]
async fn native_workspace_tools_run_through_the_durable_engine() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let journal_root = TempDir::new().unwrap();
    let mut create = call("create", 1);
    create.name = "apply_patch".into();
    create.arguments =
        json!({"operation":"create","path":"source.txt","content":"native tool result\n"})
            .to_string();
    let mut read = call("read", 1);
    read.name = "read_file".into();
    read.arguments = json!({"path":"source.txt"}).to_string();
    let mut engine = Engine::new(
        identity(),
        limits(),
        scripted(vec![
            response("", vec![create, read]),
            response("Done", vec![]),
        ]),
        super::super::workspace::Workspace::new(&root, super::super::workspace::Limits::default())
            .unwrap(),
        FakeHost::default(),
        FileJournal::create(
            &journal_root.path().canonicalize().unwrap(),
            "session-1",
            Quotas::default(),
        )
        .unwrap(),
    )
    .unwrap();
    let end = run(&mut engine, &Cancellation::default()).await;
    assert_eq!(end.reason, EndReason::ModelFinished);
    assert_eq!(
        std::fs::read(root.join("source.txt")).unwrap(),
        b"native tool result\n"
    );
    let results: Vec<_> = engine
        .host
        .records
        .iter()
        .filter_map(|record| match &record.event {
            SessionEvent::ToolResult {
                outcome: ToolOutcome::Success(value),
                ..
            } => Some(value),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0]["changes"][0]["after_digest"],
        results[1]["source"]["digest"]
    );
    assert_eq!(results[1]["text"], "native tool result\n");
    assert!(
        engine
            .host
            .records
            .iter()
            .any(|record| matches!(&record.event,
                SessionEvent::ToolIntent {effect_plan:Some(EffectPlan::Patch {files}),..}
                if files.len()==1 && files[0].path=="source.txt" && files[0].before_digest.is_none()
                    && files[0].after_digest.as_deref()==results[1]["source"]["digest"].as_str()
            ))
    );
    let replay = Replay::from_records(&engine.host.records).unwrap();
    assert_eq!(replay.budget, end.budget);
}

#[tokio::test]
async fn replay_rejects_authoring_and_submit_ends_without_confirmed_effects() {
    let (_directory, mut engine) =
        setup(scripted(vec![response("done", vec![])]), Limits::default());
    run(&mut engine, &Cancellation::default()).await;
    for reason in [
        EndReason::Submitted,
        EndReason::DefinitionCompleted,
        EndReason::ReviewCompleted,
        EndReason::ConsultationResponded,
    ] {
        let mut records = engine.host.records.clone();
        records.last_mut().unwrap().event = SessionEvent::Ended { reason };
        assert!(Replay::from_records(&records).is_err());
    }
}

#[tokio::test]
async fn working_set_projection_is_durable_replayable_and_independently_disabled() {
    for enabled in [true, false] {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("source.txt"), "content needed to edit\n").unwrap();
        let read = |id: &str| ToolCall {
            provider_call_id: id.into(),
            name: "read_file".into(),
            arguments: json!({"path":"source.txt"}).to_string(),
        };
        let mut engine = Engine::new(
            identity(),
            limits(),
            scripted(vec![
                response("", vec![read("a")]),
                response("", vec![read("b")]),
                response("done", vec![]),
            ]),
            EvidenceTools {
                workspace: super::super::workspace::Workspace::new(
                    &root,
                    super::super::workspace::Limits::default(),
                )
                .unwrap(),
                enabled,
                preparation_delay: Duration::ZERO,
            },
            FakeHost::default(),
            FileJournal::create(&root, "session-1", Quotas::default()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            run(&mut engine, &Cancellation::default()).await.reason,
            EndReason::ModelFinished
        );
        let mut replay = Replay::default();
        let mut projected = Vec::new();
        for record in &engine.host.records {
            replay.apply(record).unwrap();
            if let SessionEvent::ContextPrepared { preparation } = &record.event {
                projected.push(preparation.apply(&replay.messages).unwrap());
            }
        }

        assert_eq!(projected.len(), if enabled { 3 } else { 0 });
        if enabled {
            for (messages, request) in projected.iter().zip(&engine.provider.requests) {
                assert_eq!(messages, &request.messages);
            }

            assert!(
                matches!(&projected[2][2], Message::Tool { outcome:ToolOutcome::Success(value),.. } if value["kind"] == "evidence_reference")
            );
        }

        assert!(
            matches!(&replay.messages[2], Message::Tool { outcome:ToolOutcome::Success(value),.. } if value["text"] == "content needed to edit\n")
        );
        assert!(
            matches!(&engine.provider.requests[2].messages[4], Message::Tool { outcome:ToolOutcome::Success(value),.. } if value["text"] == "content needed to edit\n")
        );
        assert_eq!(engine.provider.requests[0].messages[0], replay.messages[0]);
    }
}

#[tokio::test]
async fn failed_compaction_charges_reservation_and_recovers_without_replaying_effects() {
    for retryable in [false, true] {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut bounds = limits();
        bounds.context_bytes = 4 * 1024;
        bounds.response_bytes = 1024;
        let mut engine = Engine::new(
            identity(),
            bounds,
            CompactionProvider {
                fail_summary: true,
                summary_failure_retryable: retryable,
                ..Default::default()
            },
            FakeTools::default(),
            FakeHost::default(),
            FileJournal::create(&root, "session-1", Quotas::default()).unwrap(),
        )
        .unwrap();
        let end = run(&mut engine, &Cancellation::default()).await;
        assert_eq!(end.reason, EndReason::ProviderFailed);
        assert_eq!(end.retryable_provider_failure, retryable);
        assert!(end.durable);
        assert!(end.budget.estimated_input_tokens > 0);
        let effects = engine.tools.effects.lock().unwrap().len();
        let records = engine.host.records.clone();
        assert!(
            records
                .iter()
                .any(|record| matches!(record.event, SessionEvent::CompactionStarted { .. }))
        );
        assert!(
            records
                .iter()
                .any(|record| matches!(record.event, SessionEvent::ModelFailed { .. }))
        );
        drop(engine);
        let (_journal, recovered) =
            FileJournal::recover(&root.join("nano/sessions/session-1"), Quotas::default()).unwrap();
        let replay = Replay::from_records(&recovered).unwrap();
        assert_eq!(replay.end, Some(EndReason::ProviderFailed));
        assert_eq!(replay.budget, end.budget);
        assert!(effects > 0);
    }
}

#[tokio::test]
async fn interactive_rejects_unserviceable_text_before_durable_acceptance() {
    let mut limits = limits();
    limits.tokens = 2_000;
    let (_dir, base) = setup(scripted(vec![]), limits.clone());
    let (sender, commands) = tokio::sync::mpsc::channel(8);
    sender
        .try_send(SessionCommand::Steer {
            input_id: Some("draft-1".into()),
            text: "x".repeat(4_000),
        })
        .unwrap();
    let journal = InteractiveJournal {
        inner: base.journal,
        commands: Some(commands),
        sender,
        interactive: true,
        waits: 0,
        user_requests: 0,
        fail_input: false,
        inject_steering: false,
    };
    let mut identity = identity();
    identity.task_id = None;
    let mut engine = Engine::new(
        identity,
        limits,
        base.provider,
        base.tools,
        base.host,
        journal,
    )
    .unwrap();
    let end = engine
        .run(
            SessionCommand::Start {
                input: "Workspace constraints".into(),
            },
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(end.reason, EndReason::Limit(LimitKind::Tokens));
    assert!(end.durable);
    assert!(engine.provider.requests.is_empty());
    let records = &engine.host.records;
    assert!(
        records
            .iter()
            .any(|r| matches!(r.event, SessionEvent::InputRequested))
    );
    assert!(
        !records
            .iter()
            .any(|r| matches!(r.event, SessionEvent::UserInput { .. }))
    );
    Replay::from_records(records).unwrap();
}

#[tokio::test]
async fn taskless_interactive_waits_before_inference_and_reconstructs_from_journal() {
    let (dir, base) = setup(scripted(vec![response("Checks passed.", vec![])]), limits());
    let (sender, commands) = tokio::sync::mpsc::channel(8);
    let journal = InteractiveJournal {
        inner: base.journal,
        commands: Some(commands),
        sender,
        interactive: true,
        waits: 0,
        user_requests: 1,
        fail_input: false,
        inject_steering: false,
    };
    let mut identity = identity();
    identity.task_id = None;
    let mut engine = Engine::new(
        identity,
        limits(),
        base.provider,
        base.tools,
        base.host,
        journal,
    )
    .unwrap();
    let end = engine
        .run(
            SessionCommand::Start {
                input: "Workspace constraints".into(),
            },
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(end.reason, EndReason::Cancelled);
    assert!(end.durable);
    assert_eq!(engine.provider.requests.len(), 1);
    assert!(
        matches!(engine.provider.requests[0].messages.last(), Some(Message::User { text }) if text == "Explain the completed checks.")
    );
    let records = &engine.host.records;
    let wait = records
        .iter()
        .position(|r| matches!(r.event, SessionEvent::InputRequested))
        .unwrap();
    let input = records
        .iter()
        .position(|r| matches!(r.event, SessionEvent::UserInput { .. }))
        .unwrap();
    let model = records
        .iter()
        .position(|r| matches!(r.event, SessionEvent::ModelStarted { .. }))
        .unwrap();
    assert!(wait < input && input < model);
    assert_eq!(records[wait].budget.model_turns, 0);
    Replay::from_records(records).unwrap();
    let snapshot = super::super::conversation::Reader::new(
        dir.path().join("nano/sessions/session-1/events.jsonl"),
        "session-1".into(),
    )
    .poll()
    .unwrap()
    .unwrap();
    assert!(snapshot.ended);
    assert!(snapshot.task_id.is_empty());
    assert!(
        snapshot
            .entries
            .iter()
            .any(|entry| entry.contains("Checks passed."))
    );
}

#[tokio::test]
async fn interactive_steering_waits_for_complete_tool_groups_and_replays() {
    let (dir, base) = setup(
        scripted(vec![
            response("Editing", vec![call("a", 1), call("b", 2)]),
            response("Checks completed.", vec![]),
            response("Both checks passed.", vec![]),
        ]),
        limits(),
    );
    let (sender, commands) = tokio::sync::mpsc::channel(8);
    let journal = InteractiveJournal {
        inner: base.journal,
        commands: Some(commands),
        sender,
        interactive: false,
        waits: 0,
        user_requests: 1,
        fail_input: false,
        inject_steering: true,
    };
    let mut engine = Engine::new(
        identity(),
        limits(),
        base.provider,
        base.tools,
        base.host,
        journal,
    )
    .unwrap();
    let end = engine
        .run(
            SessionCommand::Start {
                input: "Implement the task".into(),
            },
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(end.reason, EndReason::Cancelled);
    assert!(end.durable);
    assert_eq!(engine.provider.requests.len(), 3);
    let messages = &engine.provider.requests[1].messages;
    assert!(
        matches!(messages.last(), Some(Message::User { text }) if text == "Keep the public API stable.")
    );
    assert_eq!(
        messages
            .iter()
            .filter(|m| matches!(m, Message::Tool { .. }))
            .count(),
        2
    );
    assert!(
        matches!(engine.provider.requests[2].messages.last(), Some(Message::User { text }) if text == "Explain the completed checks.")
    );
    let records = &engine.host.records;
    let first_input = records
        .iter()
        .position(|r| matches!(r.event, SessionEvent::UserInput { .. }))
        .unwrap();
    assert_eq!(
        records[..first_input]
            .iter()
            .filter(|r| matches!(r.event, SessionEvent::ToolResult { .. }))
            .count(),
        2
    );
    assert_eq!(
        records
            .iter()
            .filter(|r| matches!(r.event, SessionEvent::InteractionOpened))
            .count(),
        1
    );
    let replay = Replay::from_records(records).unwrap();
    assert_eq!(
        replay.messages.len(),
        engine.provider.requests[2].messages.len() + 1
    );
    assert!(
        matches!(replay.messages.last(), Some(Message::Assistant { response }) if response.text == "Both checks passed.")
    );
    // A fresh frontend can read the same durable conversation without touching its writer.
    let path = dir.path().join("nano/sessions/session-1/events.jsonl");
    let snapshot = super::super::conversation::Reader::new(path, "session-1".into())
        .poll()
        .unwrap()
        .unwrap();
    assert!(snapshot.ended);
    assert!(
        snapshot
            .entries
            .iter()
            .any(|text| text.contains("Explain the completed checks."))
    );
    let second_turn = records
        .iter()
        .position(|record| matches!(record.event, SessionEvent::ModelStarted { turn: 2 }))
        .unwrap();
    let cut = Replay::from_records(&records[..second_turn])
        .unwrap()
        .messages
        .len();
    let mut summarized_steering = records.clone();
    summarized_steering[second_turn].event = SessionEvent::CompactionStarted {
        turn: 2,
        retained_from: cut,
    };
    assert!(Replay::from_records(&summarized_steering).is_err());
    // Owner-written input inside a pending tool group fails closed.
    let mut malformed = records.clone();
    let at = malformed
        .iter()
        .position(|r| matches!(r.event, SessionEvent::ToolStarted { .. }))
        .unwrap();
    malformed[at].event = SessionEvent::UserInput {
        input_id: None,
        text: "interrupt this tool".into(),
    };
    assert!(Replay::from_records(&malformed).is_err());
}

#[tokio::test]
async fn interactive_input_is_not_used_if_its_journal_commit_fails() {
    let (_dir, base) = setup(
        scripted(vec![response("Editing", vec![call("a", 1)])]),
        limits(),
    );
    let (sender, commands) = tokio::sync::mpsc::channel(8);
    let journal = InteractiveJournal {
        inner: base.journal,
        commands: Some(commands),
        sender,
        interactive: true,
        waits: 0,
        user_requests: 1,
        fail_input: true,
        inject_steering: true,
    };
    let mut engine = Engine::new(
        identity(),
        limits(),
        base.provider,
        base.tools,
        base.host,
        journal,
    )
    .unwrap();
    let end = engine
        .run(
            SessionCommand::Start {
                input: "Implement".into(),
            },
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(end.reason, EndReason::JournalFailed);
    assert_eq!(engine.provider.requests.len(), 1);
    assert_eq!(engine.tools.effects.lock().unwrap().len(), 1);
}
