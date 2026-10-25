//! Tests for limits.

use super::*;

#[tokio::test]
async fn truncated_calls_never_execute_and_continuation_can_make_progress() {
    let mut truncated = response("Partial", vec![call("discarded", 1)]);
    if let ProviderEvent::Completed { response, .. } = &mut truncated {
        response.finish = FinishReason::Length;
        response.calls[0].arguments = "{\"value\":".into();
    }

    let (_dir, mut engine) = setup(
        scripted(vec![
            truncated,
            response("", vec![call("complete", 2)]),
            response("Done", vec![]),
        ]),
        limits(),
    );
    let end = run(&mut engine, &Cancellation::default()).await;
    assert_eq!(end.reason, EndReason::ModelFinished);
    assert_eq!(end.budget.model_turns, 3);
    assert_eq!(end.budget.tool_calls, 1);
    assert_eq!(*engine.tools.effects.lock().unwrap(), ["call-1"]);
    let messages = &engine.provider.requests[1].messages;
    assert!(
        matches!(&messages[1], Message::Assistant { response } if response.finish == FinishReason::Length && response.calls.is_empty() && response.text == "Partial")
    );
    assert!(
        matches!(&messages[2], Message::User { text } if text.contains("No tools from that response were executed"))
    );
    let replay = Replay::from_records(&engine.host.records).unwrap();
    assert!(replay.unknown_effects.is_empty());
    assert!(replay.checkpoint_ready());
    assert_eq!(replay.budget, end.budget);
    let first = engine
        .host
        .records
        .iter()
        .position(|record| {
            matches!(&record.event,
        SessionEvent::ModelCompleted { response, .. } if response.finish == FinishReason::Length)
        })
        .unwrap();
    assert_eq!(
        Replay::from_records(&engine.host.records[..=first])
            .unwrap()
            .messages,
        engine.provider.requests[1].messages
    );
}

#[tokio::test]
async fn repeated_truncated_responses_stop_at_no_progress_without_executing_calls() {
    let events = (0..3)
        .map(|_| {
            let mut event = response("Partial", vec![call("duplicate", 1), call("duplicate", 2)]);
            if let ProviderEvent::Completed { response, .. } = &mut event {
                response.finish = FinishReason::Length;
            }

            event
        })
        .collect();
    let (_dir, mut engine) = setup(scripted(events), limits());
    let end = run(&mut engine, &Cancellation::default()).await;
    assert_eq!(end.reason, EndReason::Limit(LimitKind::NoProgress));
    assert_eq!(end.budget.model_turns, 3);
    assert_eq!(end.budget.tool_calls, 0);
    assert_eq!(end.budget.retries, 0);
    assert!(engine.tools.effects.lock().unwrap().is_empty());
    let replay = Replay::from_records(&engine.host.records).unwrap();
    assert!(replay.checkpoint_ready());
    assert_eq!(replay.budget, end.budget);
}

#[tokio::test]
async fn uncompactable_request_reports_session_token_limit() {
    let provider = scripted(vec![response("Done", vec![])]);
    let input = provider
        .estimate_input_tokens(&ModelRequest {
            messages: vec![Message::User {
                text: "Implement a fixture task".into(),
            }],
            tools: FakeTools::default().descriptors(),
            max_output_tokens: 12,
        })
        .unwrap();
    let mut bounds = limits();
    bounds.tokens = input - 1;
    let (_dir, mut engine) = setup(provider, bounds);
    let end = run(&mut engine, &Cancellation::default()).await;
    assert_eq!(end.reason, EndReason::Limit(LimitKind::Tokens));
    assert!(engine.provider.requests.is_empty());
}

#[tokio::test]
async fn uncompactable_request_reports_serialized_context_limit() {
    let mut bounds = limits();
    bounds.context_bytes = 64;
    let (_dir, mut engine) = setup(scripted(vec![response("Done", vec![])]), bounds);
    let end = run(&mut engine, &Cancellation::default()).await;
    assert_eq!(end.reason, EndReason::Limit(LimitKind::ContextBytes));
    assert!(engine.provider.requests.is_empty());
}

#[tokio::test]
async fn interactive_user_turns_reset_loop_tracking_and_preserve_session_budgets() {
    for fail in [false, true] {
        let provider = scripted(
            (0..5)
                .flat_map(|turn| {
                    [
                        response(
                            "",
                            vec![call(&format!("{turn}-a"), 1), call(&format!("{turn}-b"), 1)],
                        ),
                        response("Request completed.", vec![]),
                    ]
                })
                .collect(),
        );
        let (_dir, base) = setup(provider, limits());
        let (sender, commands) = tokio::sync::mpsc::channel(8);
        let journal = InteractiveJournal {
            inner: base.journal,
            commands: Some(commands),
            sender,
            interactive: true,
            waits: 0,
            user_requests: 5,
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
        engine.tools.fail = fail;
        let end = engine
            .run(
                SessionCommand::Start {
                    input: "Workspace constraints".into(),
                },
                &Cancellation::default(),
            )
            .await
            .unwrap();
        assert_eq!(end.reason, EndReason::Cancelled, "failed tools: {fail}");
        assert!(end.durable);
        assert_eq!(end.budget.model_turns, 10);
        assert_eq!(end.budget.tool_calls, 10);
        assert_eq!(end.budget.reported_input_tokens, 100);
        assert_eq!(end.budget.reported_output_tokens, 50);
        let records = &engine.host.records;
        assert_eq!(
            records
                .iter()
                .filter(|r| matches!(r.event, SessionEvent::UserInput { .. }))
                .count(),
            5
        );
        for (index, record) in records.iter().enumerate() {
            if matches!(record.event, SessionEvent::UserInput { .. }) {
                assert_eq!(record.budget.no_progress, 0);
                if index > 0 {
                    let before = &records[index - 1].budget;
                    assert_eq!(record.budget.model_turns, before.model_turns);
                    assert_eq!(record.budget.tool_calls, before.tool_calls);
                    assert_eq!(record.budget.tokens(), before.tokens());
                    assert_eq!(record.budget.retries, before.retries);
                }
            }
        }

        // Repetition and failures still accumulate within each individual request.
        let progress: Vec<_> = records
            .iter()
            .filter(|r| matches!(r.event, SessionEvent::ToolResult { .. }))
            .map(|r| r.budget.no_progress)
            .collect();
        assert_eq!(
            progress,
            if fail {
                vec![1, 2].repeat(5)
            } else {
                vec![0, 1].repeat(5)
            }
        );
        let replay = Replay::from_records(records).unwrap();
        assert_eq!(replay.budget, end.budget);
    }
}
