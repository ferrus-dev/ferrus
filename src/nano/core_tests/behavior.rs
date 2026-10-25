//! Tests for behavior.

use super::*;

#[tokio::test]
async fn text_completion_records_usage_but_does_not_complete_a_ferrus_task() {
    let (_dir, mut engine) = setup(scripted(vec![response("Done", vec![])]), limits());
    let end = run(&mut engine, &Cancellation::default()).await;
    assert_eq!(end.reason, EndReason::ModelFinished);
    assert!(end.durable);
    assert_eq!(end.budget.reported_input_tokens, 10);
    assert_eq!(end.budget.reported_output_tokens, 5);
    assert_eq!(end.budget.estimated_input_tokens, 0);
    assert_eq!(end.budget.reserved_input_tokens, 0);
    assert!(engine.tools.effects.lock().unwrap().is_empty());
    assert!(matches!(
        engine.host.records.last().unwrap().event,
        SessionEvent::Ended {
            reason: EndReason::ModelFinished
        }
    ));
    assert!(
        engine
            .run(SessionCommand::Cancel, &Cancellation::default())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn multiple_tools_keep_call_result_order_and_continuation_data() {
    let (_dir, mut engine) = setup(
        scripted(vec![
            response("", vec![call("a", 1), call("b", 2)]),
            response("Done", vec![]),
        ]),
        limits(),
    );
    let end = run(&mut engine, &Cancellation::default()).await;
    assert_eq!(end.reason, EndReason::ModelFinished);
    assert_eq!(*engine.tools.effects.lock().unwrap(), ["call-1", "call-2"]);
    let order: Vec<_> = engine
        .host
        .records
        .iter()
        .filter_map(|record| match &record.event {
            SessionEvent::ToolIntent { call_id, .. } => Some(format!("intent:{call_id}")),
            SessionEvent::ToolStarted { call_id } => Some(format!("started:{call_id}")),
            SessionEvent::ToolResult { call_id, .. } => Some(format!("result:{call_id}")),
            _ => None,
        })
        .collect();
    assert_eq!(
        order,
        [
            "intent:call-1",
            "started:call-1",
            "result:call-1",
            "intent:call-2",
            "started:call-2",
            "result:call-2"
        ]
    );
    assert!(
        matches!(&engine.provider.requests[1].messages[1], Message::Assistant { response } if response.continuation == Some(json!({"signed":"opaque"})))
    );
    assert!(
        matches!(&engine.provider.requests[1].messages[2], Message::Tool { provider_call_id,.. } if provider_call_id=="a")
    );
    assert!(engine.provider.requests[0].max_output_tokens > 0);
}

#[tokio::test]
async fn retries_charge_estimates_and_successful_usage_stays_distinct() {
    let mut provider = scripted(vec![response("Done", vec![])]);
    provider
        .turns
        .push_front(Err(ProviderError::new(ProviderErrorKind::Transport, true)));
    let (_dir, mut engine) = setup(provider, limits());
    let end = run(&mut engine, &Cancellation::default()).await;
    assert_eq!(end.reason, EndReason::ModelFinished);
    assert_eq!(end.budget.retries, 1);
    assert_eq!(end.budget.model_turns, 2);
    assert!(end.budget.estimated_input_tokens > 0 && end.budget.estimated_output_tokens > 0);
    assert_eq!(end.budget.reported_input_tokens, 10);
    let state = Replay::from_records(&engine.host.records).unwrap();
    assert_eq!(state.budget, end.budget);
}

#[tokio::test]
async fn duplicate_call_responses_never_execute() {
    for truncated in [false, true] {
        let mut event = response("Partial", vec![call("duplicate", 1), call("duplicate", 2)]);
        if let ProviderEvent::Completed { response, .. } = &mut event {
            if truncated {
                response.finish = FinishReason::Length;
            }
        }

        let (_dir, mut engine) = setup(
            scripted(vec![event]),
            Limits {
                model_turns: 1,
                ..limits()
            },
        );
        let end = run(&mut engine, &Cancellation::default()).await;
        assert_eq!(
            end.reason,
            if truncated {
                EndReason::Limit(LimitKind::ModelTurns)
            } else {
                EndReason::ProviderProtocol
            }
        );
        assert!(engine.tools.effects.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn host_denial_returns_feedback_and_absent_usage_is_estimated() {
    let mut final_response = response("Done", vec![]);
    if let ProviderEvent::Completed { usage, .. } = &mut final_response {
        *usage = None;
    }

    let (_dir, mut engine) = setup(
        scripted(vec![response("", vec![call("a", 1)]), final_response]),
        limits(),
    );
    engine.host.deny = true;
    let end = run(&mut engine, &Cancellation::default()).await;
    assert_eq!(end.reason, EndReason::ModelFinished);
    assert!(engine.tools.effects.lock().unwrap().is_empty());
    assert!(end.budget.estimated_output_tokens > 0);
    assert_eq!(end.budget.reported_output_tokens, 5);
    assert!(engine.host.records.iter().any(|record| matches!(
        record.event,
        SessionEvent::ToolResult {
            outcome: ToolOutcome::Failed(ToolError::Denied),
            ..
        }
    )));
}
