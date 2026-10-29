//! Tests for provider.

use super::*;

#[tokio::test]
async fn uncertain_tool_cleanup_cannot_certify_model_completion() {
    let (_dir, mut engine) = setup(scripted(vec![response("Done", vec![])]), limits());
    engine.tools.uncertain_cleanup = true;
    let end = run(&mut engine, &Cancellation::default()).await;
    assert_eq!(end.reason, EndReason::EffectUnknown);
    assert!(end.durable);
    assert!(matches!(
        engine.host.records.last().unwrap().event,
        SessionEvent::Ended {
            reason: EndReason::EffectUnknown
        }
    ));
}

#[tokio::test]
async fn partial_arguments_and_abrupt_stream_end_never_execute() {
    let provider = ScriptedProvider {
        turns: VecDeque::from([Ok(VecDeque::from([
            ProviderEvent::ArgumentsDelta("{\"value\":".into()),
            ProviderEvent::TextDelta("working".into()),
        ]))]),
        ..Default::default()
    };
    let (_dir, mut engine) = setup(provider, limits());
    assert_eq!(
        run(&mut engine, &Cancellation::default()).await.reason,
        EndReason::ProviderProtocol
    );
    assert!(engine.tools.effects.lock().unwrap().is_empty());
    assert!(
        engine
            .host
            .records
            .iter()
            .all(|r| !matches!(r.event, SessionEvent::ToolIntent { .. }))
    );
}

#[tokio::test]
async fn turn_tool_token_retry_and_context_limits_are_enforced() {
    for kind in [
        LimitKind::ModelTurns,
        LimitKind::ToolCalls,
        LimitKind::Tokens,
        LimitKind::Retries,
        LimitKind::ContextBytes,
        LimitKind::ResponseBytes,
        LimitKind::ToolOutputBytes,
    ] {
        let mut budget = limits();
        let mut provider = scripted(vec![
            response("", vec![call("a", 1), call("b", 2)]),
            response("Done", vec![]),
        ]);
        match kind {
            LimitKind::ModelTurns => budget.model_turns = 1,
            LimitKind::ToolCalls => budget.tool_calls = 1,
            LimitKind::Tokens => budget.tokens = 1,
            LimitKind::Retries => {
                budget.retries = 0;
                provider.turns =
                    VecDeque::from([Err(ProviderError::new(ProviderErrorKind::Transport, true))]);
            }
            LimitKind::ContextBytes => budget.context_bytes = 1,
            LimitKind::ResponseBytes => budget.response_bytes = 1,
            LimitKind::ToolOutputBytes => budget.tool_output_bytes = 32,
            _ => unreachable!(),
        }

        let (_dir, mut engine) = setup(provider, budget);
        engine.tools.huge = kind == LimitKind::ToolOutputBytes;
        let end = run(&mut engine, &Cancellation::default()).await;
        assert_eq!(end.reason, EndReason::Limit(kind.clone()), "{kind:?}");
        if kind == LimitKind::ToolCalls {
            assert_eq!(engine.tools.effects.lock().unwrap().len(), 1);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn stalled_provider_cannot_hide_elapsed_budget() {
    let mut budget = limits();
    budget.elapsed_ms = 20;
    let provider = ScriptedProvider {
        turns: VecDeque::from([Ok(VecDeque::new())]),
        stall: true,
        ..Default::default()
    };
    let (_dir, mut engine) = setup(provider, budget);
    let end = run(&mut engine, &Cancellation::default()).await;
    assert_eq!(end.reason, EndReason::Limit(LimitKind::Elapsed));
    assert!(end.budget.elapsed_ms >= 20);
    assert_eq!(engine.provider.requests.len(), 1);
    assert!(end.budget.estimated_input_tokens > 0);
}

#[tokio::test]
async fn long_session_compacts_complete_groups_and_charges_summary_inference() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut bounds = limits();
    bounds.context_bytes = 4 * 1024;
    bounds.response_bytes = 1024;
    let mut engine = Engine::new(
        identity(),
        bounds,
        CompactionProvider::default(),
        FakeTools::default(),
        FakeHost::default(),
        FileJournal::create(&root, "session-1", Quotas::default()).unwrap(),
    )
    .unwrap();
    engine.set_system_prompt("Host policy").unwrap();
    let end = run(&mut engine, &Cancellation::default()).await;
    assert_eq!(end.reason, EndReason::ModelFinished);
    assert!(engine.provider.summaries > 0);
    assert_eq!(engine.provider.normal_turns, 10);
    assert_eq!(
        end.budget.model_turns,
        11 + engine.provider.summaries as u64
    );
    assert!(end.budget.reported_input_tokens >= 110 + 17 * engine.provider.summaries as u64);
    assert!(
        engine
            .host
            .records
            .iter()
            .any(|record| matches!(record.event, SessionEvent::CompactionCompleted { .. }))
    );
    let replay = Replay::from_records(&engine.host.records).unwrap();
    assert_eq!(replay.messages.len(), 23);
    assert!(replay.summary.is_some());
    let composed: Vec<_> = engine
        .host
        .records
        .iter()
        .filter_map(|record| {
            if let SessionEvent::ContextComposed { composition } = &record.event {
                Some(composition)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(composed.len(), 11 + engine.provider.summaries);
    assert!(composed.iter().any(|entry| entry.summary_present));
    assert!(composed.iter().all(|entry| {
        let margin = (entry.context_window_tokens / 20).clamp(16, 1024);
        entry.input_tokens_estimated + entry.output_tokens_reserved + margin
            <= entry.context_window_tokens
            && entry.stable_prefix_messages <= entry.request_messages
    }));
    assert!(
        engine
            .provider
            .requests
            .iter()
            .filter(|request| !request.tools.is_empty())
            .all(|request| {
                super::super::compaction::boundaries(&request.messages).is_ok()
                    && request.messages[0]
                        == Message::System {
                            text: "Host policy".into(),
                        }
                    && request.messages[1]
                        == Message::User {
                            text: "Implement a fixture task".into(),
                        }
                    && serde_json::to_vec(&(&request.messages, &request.tools))
                        .unwrap()
                        .len()
                        <= 4 * 1024
            })
    );
    assert!(engine.provider.requests.iter().any(|request| {
        request.messages.iter().any(|message| matches!(message, Message::User {text} if text.contains("Historical session summary")))
    }));
    assert!(engine
        .provider
        .requests
        .iter()
        .filter(|request| {
            !request.tools.is_empty()
                && request.messages.iter().any(|message| {
                    matches!(message, Message::User { text } if text.contains("Historical session summary"))
                })
        })
        .all(|request| request
            .messages
            .iter()
            .any(|message| matches!(message, Message::Assistant { .. }))));
}
