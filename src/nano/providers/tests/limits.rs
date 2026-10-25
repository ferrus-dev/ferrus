//! Tests for limits.

use super::*;

#[tokio::test]
async fn context_error_after_a_long_message_keeps_its_typed_limit() {
    let mut reply = Reply::failure(400, "context_length_exceeded");
    reply.body = format!(
        "{{\"error\":{{\"message\":{},\"code\":\"context_length_exceeded\"}}}}",
        serde_json::to_string(&"secret-detail".repeat(7000)).unwrap()
    );
    let (url, server) = server(vec![reply]).await;
    let mut provider = OpenAi::new(config(&url)).unwrap();
    let error = provider.start(request()).await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::ContextOverflow);
    assert_eq!(
        error.diagnostic,
        Some(ProviderDiagnostic::Http { status: 400 })
    );
    assert_eq!(error.retry_after_ms, 1000);
    assert!(!error.retryable);
    assert!(!format!("{error:?}").contains("secret-detail"));
    server.await.unwrap();
}

#[test]
fn advertised_tools_are_bounded_by_context_not_generated_call_count() {
    for max_tool_calls in [1, 64] {
        let mut cfg = config("http://127.0.0.1:1234/v1");
        cfg.max_tool_calls = max_tool_calls;
        cfg.context_tokens = Some(32_768);
        let context_tokens = cfg.context_tokens.unwrap();
        let provider = OpenAi::new(cfg).unwrap();
        let mut request = request();
        request.tools = (0..=max_tool_calls)
            .map(|index| ToolDescriptor {
                name: format!("lookup_{index}"),
                ..request.tools[0].clone()
            })
            .collect();
        let body: Value = serde_json::from_slice(&provider.body(request.clone()).unwrap()).unwrap();
        assert_eq!(body["tools"].as_array().unwrap().len(), max_tool_calls + 1);
        for (tool, advertised) in request.tools.iter().zip(body["tools"].as_array().unwrap()) {
            assert_eq!(advertised["function"]["name"], tool.name);
            assert_eq!(advertised["function"]["parameters"], tool.input_schema);
        }

        request.tools[0].description = "x".repeat(context_tokens as usize);
        assert_eq!(
            provider.body(request).unwrap_err().kind,
            ProviderErrorKind::ContextOverflow
        );
    }
}

#[tokio::test]
async fn unknown_context_does_not_limit_engine_requests_to_the_old_default() {
    let (url, server) = server(vec![Reply::stream(FINAL)]).await;
    let directory = TempDir::new().unwrap();
    let mut engine = engine(OpenAi::new(config(&url)).unwrap(), &directory);
    engine.set_system_prompt("Host policy").unwrap();
    let text = "x".repeat(40_000);
    let end = engine
        .run(
            SessionCommand::Start {
                input: text.clone(),
            },
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(end.reason, EndReason::ModelFinished);
    assert_eq!(end.budget.model_turns, 1);
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].1["messages"][1]["content"], text);
    assert_eq!(
        requests[0].1["max_tokens"],
        Limits::default().response_bytes
    );
    assert!(requests[0].1.get("temperature").is_none());
    assert!(crate::nano::replay::Replay::from_records(&engine.host.records).is_ok());
}

#[tokio::test]
async fn transient_failures_charge_only_the_effective_output_cap() {
    let partial = &FINAL[..FINAL.find("data: [DONE]").unwrap()];
    let (url, server) = server(vec![
        Reply::failure(429, "rate_limit"),
        Reply::failure(503, "unavailable"),
        Reply::stream(partial),
        Reply::stream(FINAL),
    ])
    .await;
    let directory = TempDir::new().unwrap();
    let cfg = config(&url);
    let output_cap = 4096;
    let mut cfg = cfg;
    cfg.max_output_tokens = Some(output_cap);
    let mut engine = engine_with_limits(OpenAi::new(cfg).unwrap(), &directory, Limits::default());
    let end = engine
        .run(
            SessionCommand::Start {
                input: "x".repeat(8192),
            },
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(end.reason, EndReason::ModelFinished);
    assert_eq!(end.budget.retries, 3);
    assert_eq!(end.budget.model_turns, 4);
    assert_eq!(end.budget.estimated_output_tokens, 3 * output_cap);
    for record in &engine.host.records {
        match &record.event {
            SessionEvent::ModelStarted { .. } => {
                assert_eq!(record.budget.reserved_output_tokens, output_cap);
            }
            SessionEvent::ModelFailed { usage, .. } => {
                assert_eq!(usage.output_tokens, output_cap);
                assert!(!usage.reported);
            }
            _ => (),
        }
    }

    let replay = crate::nano::replay::Replay::from_records(&engine.host.records).unwrap();
    assert_eq!(replay.budget, end.budget);
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 4);
    for (_, body) in requests {
        assert_eq!(body["max_tokens"], output_cap);
    }
}
