//! Tests for provider.

use super::*;

#[test]
fn work_phase_token_budget_is_independent_of_provider_context_and_output() {
    let original = config("http://127.0.0.1:1234/v1");
    assert_eq!(original.session_tokens, 1_000_000);
    let original_provider = original.validate().unwrap().1;
    let mut changed = config("http://127.0.0.1:1234/v1");
    changed.session_tokens = 2_000_000;
    assert_eq!(changed.validate().unwrap().1, original_provider);
    changed.session_tokens = 0;
    assert!(changed.validate().is_err());
    let configured: Config = toml::from_str("base_url = 'http://127.0.0.1:1234/v1'\nmodel = 'fixture-model'\nsession_tokens = 3000000\n").unwrap();
    assert_eq!(configured.session_tokens, 3_000_000);
}

#[tokio::test]
async fn context_overflow_ends_the_session_without_retrying_or_executing_tools() {
    let (url, server) = server(vec![Reply::failure(400, "context_length_exceeded")]).await;
    let directory = TempDir::new().unwrap();
    let mut engine = engine(OpenAi::new(config(&url)).unwrap(), &directory);
    let end = engine
        .run(
            SessionCommand::Start {
                input: "Use lookup".into(),
            },
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(end.reason, EndReason::Limit(LimitKind::ContextTokens));
    assert_eq!(end.budget.model_turns, 1);
    assert_eq!(end.budget.retries, 0);
    assert_eq!(engine.tools.calls, 0);
    assert!(engine.host.records.iter().any(|r| matches!(
        r.event,
        SessionEvent::ModelFailed {
            error: Some(ProviderErrorKind::ContextOverflow),
            ..
        }
    )));
    server.await.unwrap();
}

#[tokio::test]
async fn oversized_http_error_preserves_response_limit_and_status() {
    let mut reply = Reply::failure(400, "unsupported");
    reply.body = "secret-body".repeat(100);
    let (url, server) = server(vec![reply]).await;
    let mut cfg = config(&url);
    cfg.event_bytes = 256;
    let mut provider = OpenAi::new(cfg).unwrap();
    let error = provider.start(request()).await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::ResponseLimit);
    assert_eq!(
        error.diagnostic,
        Some(ProviderDiagnostic::Http { status: 400 })
    );
    assert!(!error.retryable);
    assert!(!format!("{error:?}").contains("secret-body"));
    server.await.unwrap();
}

#[tokio::test]
async fn retries_and_truncated_streams_share_budget_without_duplicate_effects() {
    let lf = TOOLS.replace("\r\n", "\n");
    for wire in [lf.clone(), lf.replace('\n', "\r\n")] {
        let partial = &wire[..wire.find("data: [DONE]").unwrap()];
        let (url, server) = server(vec![
            Reply::failure(429, "rate_limit"),
            Reply::stream(partial),
            Reply::stream(&wire),
            Reply::stream(FINAL),
        ])
        .await;
        let directory = TempDir::new().unwrap();
        let mut engine = engine(OpenAi::new(config(&url)).unwrap(), &directory);
        let end = engine
            .run(
                SessionCommand::Start {
                    input: "Use lookup".into(),
                },
                &Cancellation::default(),
            )
            .await
            .unwrap();
        assert_eq!(end.reason, EndReason::ModelFinished);
        assert_eq!(engine.tools.calls, 2);
        assert_eq!(end.budget.retries, 2);
        assert_eq!(end.budget.model_turns, 4);
        assert!(end.budget.estimated_input_tokens > 0 && end.budget.estimated_output_tokens > 0);
        assert!(end.budget.elapsed_ms >= 1500);
        assert!(
            !serde_json::to_string(&engine.host.records)
                .unwrap()
                .contains("secret-server-detail")
        );
        assert_eq!(server.await.unwrap().len(), 4);
    }
}

#[tokio::test]
async fn active_stream_still_consumes_the_engine_elapsed_budget() {
    let body = format!("{}{FINAL}", ": generating\n".repeat(6));
    let mut reply = Reply::stream(&body);
    reply.line_delay = Some(Duration::from_millis(250));
    let (url, server) = server(vec![reply]).await;
    let mut cfg = config(&url);
    cfg.request_timeout_ms = 10_000;
    let directory = TempDir::new().unwrap();
    let mut engine = engine_with_limits(
        OpenAi::new(cfg).unwrap(),
        &directory,
        Limits {
            elapsed_ms: 1000,
            ..Default::default()
        },
    );
    let end = engine
        .run(
            SessionCommand::Start {
                input: "lookup".into(),
            },
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(end.reason, EndReason::Limit(LimitKind::Elapsed));
    assert_eq!(engine.tools.calls, 0);
    assert!(end.durable);
    server.await.unwrap();
}
