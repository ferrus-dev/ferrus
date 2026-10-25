//! Tests for behavior.

use super::*;

#[test]
fn reasoning_effort_is_explicit_and_bound_to_effective_settings() {
    let cfg = config("http://127.0.0.1:1234/v1");
    let original = cfg.validate().unwrap().1;
    let encoded = serde_json::to_value(&original).unwrap();
    assert!(encoded.get("reasoning_effort").is_none());
    assert_eq!(
        serde_json::from_value::<ProviderSettings>(encoded).unwrap(),
        original
    );
    let provider = OpenAi::new(cfg).unwrap();
    let body: Value = serde_json::from_slice(&provider.body(request()).unwrap()).unwrap();
    assert!(body.get("reasoning_effort").is_none());
    let evidence = LaunchEvidence {
        baseline_tree: "fixture".into(),
        native_context_enabled: true,
        working_set_enabled: true,
        graph_peer_mode: Some(GraphPeerMode::Absent),
        graph_peer_id: None,
        mcp_peer_count: Some(0),
        mcp_tool_count: Some(0),
        graph_peer_timeout_ms: None,
        settings_sha256: None,
    };
    let digest = effective_settings_sha256(&original, &Limits::default(), &evidence).unwrap();
    for level in ["none", "minimal", "low", "medium", "high", "xhigh", "max"] {
        let cfg: Config = toml::from_str(&format!(
            "base_url = 'http://127.0.0.1:1234/v1'\nmodel = 'fixture-model'\nreasoning_effort = '{level}'\n"
        ))
        .unwrap();
        let provider = OpenAi::new(cfg).unwrap();
        let settings = provider.settings().unwrap();
        let body: Value = serde_json::from_slice(&provider.body(request()).unwrap()).unwrap();
        assert_eq!(body["reasoning_effort"], level);
        assert_ne!(
            effective_settings_sha256(&settings, &Limits::default(), &evidence).unwrap(),
            digest
        );
        let record = serde_json::to_value(&settings).unwrap();
        assert_eq!(record["reasoning_effort"], level);
        assert_eq!(
            serde_json::from_value::<ProviderSettings>(record).unwrap(),
            settings
        );
        assert_eq!(
            provider.estimate_input_tokens(&request()).unwrap(),
            serde_json::to_vec(&body).unwrap().len() as u64
        );
    }

    for invalid in ["'automatic'", "true", "1"] {
        assert!(toml::from_str::<Config>(&format!(
            "base_url = 'http://127.0.0.1:1234/v1'\nmodel = 'fixture-model'\nreasoning_effort = {invalid}\n"
        )).is_err());
    }
}

#[test]
fn split_frames_calls_usage_and_reasoning_are_preserved() {
    let lf = TOOLS.replace("\r\n", "\n");
    for stride in [1, 3, 8192] {
        for wire in [lf.clone(), lf.replace('\n', "\r\n")] {
            let events = decode(&wire, stride).unwrap();
            let ProviderEvent::Completed { response, usage } = events.last().unwrap() else {
                panic!("Missing completion")
            };
            assert_eq!(response.finish, FinishReason::ToolCalls);
            assert_eq!(
                response
                    .calls
                    .iter()
                    .map(|c| (&*c.provider_call_id, &*c.name, &*c.arguments))
                    .collect::<Vec<_>>(),
                [
                    ("call-a", "lookup", "{\"value\":1}"),
                    ("call-b", "lookup", "{\"value\":2}")
                ]
            );
            assert_eq!(usage.as_ref().unwrap().input_tokens, 12);
            assert_eq!(usage.as_ref().unwrap().output_tokens, 8);
            assert!(usage.as_ref().unwrap().reported);
            assert_eq!(
                response.continuation,
                Some(json!({"reasoning_content":"opaque reasoning"}))
            );
            let provider = OpenAi::new(config("http://127.0.0.1:1234/v1")).unwrap();
            let body: Value = serde_json::from_slice(
                &provider
                    .body(ModelRequest {
                        messages: vec![
                            Message::Assistant {
                                response: response.clone(),
                            },
                            Message::Tool {
                                provider_call_id: "call-a".into(),
                                outcome: ToolOutcome::Success(json!(42)),
                            },
                        ],
                        ..request()
                    })
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(body["messages"][0]["reasoning_content"], "opaque reasoning");
            assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "call-a");
            assert_eq!(body["messages"][1]["tool_call_id"], "call-a");
        }
    }

    let unicode = FINAL.replace("42", "caf\u{e9}");
    assert!(
        matches!(decode(&unicode, 1).unwrap().last(), Some(ProviderEvent::Completed { response, .. }) if response.text == "caf\u{e9}")
    );
}

#[test]
fn tool_outcome_envelopes_and_inline_reasoning_remain_unchanged() {
    let provider = OpenAi::new(config("http://127.0.0.1:1234/v1")).unwrap();
    for outcome in [
        ToolOutcome::Success(json!(42)),
        ToolOutcome::Failed(ToolError::Denied),
        ToolOutcome::Unknown(ToolError::Interrupted),
    ] {
        let mut req = request();
        req.messages.push(Message::Tool {
            provider_call_id: "opaque-1".into(),
            outcome: outcome.clone(),
        });
        let body: Value = serde_json::from_slice(&provider.body(req).unwrap()).unwrap();
        let content: Value =
            serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap();
        assert_eq!(content, serde_json::to_value(outcome).unwrap());
        assert_eq!(body["messages"][1]["tool_call_id"], "opaque-1");
    }

    let wire = FINAL.replace("42", "<think>opaque</think>42");
    assert!(
        matches!(decode(&wire, 1).unwrap().last(), Some(ProviderEvent::Completed { response, .. }) if response.text == "<think>opaque</think>42" && response.continuation.is_none())
    );
}

#[test]
fn length_finish_preserves_partial_calls_for_audit_but_not_for_continuation() {
    let wire = TOOLS
        .replace("\"tool_calls\"}]", "\"length\"}]")
        .replace("\"id\":\"call-a\"", "\"id\":\"\"")
        .replace("\"name\":\"look\"", "\"name\":\"\"");
    let events = decode(&wire, 1).unwrap();
    let ProviderEvent::Completed { response, .. } = events.last().unwrap() else {
        panic!("Missing completion");
    };
    assert_eq!(response.finish, FinishReason::Length);
    assert!(!response.calls.is_empty());
    let messages = response.messages();
    assert!(
        matches!(&messages[0], Message::Assistant { response } if response.calls.is_empty() && response.continuation.is_some())
    );
    assert!(matches!(&messages[1], Message::User { .. }));
}

#[tokio::test]
async fn authenticated_and_anonymous_sessions_record_settings_and_usage_without_secrets() {
    for authenticate in [false, true] {
        let (url, server) = server(vec![Reply::stream(TOOLS), Reply::stream(FINAL)]).await;
        let directory = TempDir::new().unwrap();
        let mut cfg = config(&url);
        if authenticate {
            let path = directory.path().join("credential");
            private::file(&path, true)
                .unwrap()
                .write_all(b"fixture-private-token")
                .unwrap();
            cfg.api_key_file = Some(path);
        }

        let mut engine = engine(OpenAi::new(cfg).unwrap(), &directory);
        engine.set_system_prompt("Host policy").unwrap();
        let end = engine
            .run(
                SessionCommand::Start {
                    input: "Use lookup with value 1, then report the answer.".into(),
                },
                &Cancellation::default(),
            )
            .await
            .unwrap();
        assert_eq!(end.reason, EndReason::ModelFinished);
        assert_eq!(engine.tools.calls, 2);
        assert_eq!(end.budget.reported_input_tokens, 36);
        assert_eq!(end.budget.reported_output_tokens, 11);
        let records = serde_json::to_string(&engine.host.records).unwrap();
        assert!(
            records.contains("fixture-model") && records.contains("openai_chat_completions_v1")
        );
        assert!(!records.contains("fixture-private-token") && !records.contains("api_key_file"));
        for (headers, body) in server.await.unwrap() {
            assert_eq!(
                headers
                    .to_ascii_lowercase()
                    .contains("authorization: bearer fixture-private-token"),
                authenticate
            );
            assert!(headers.starts_with("POST /v1/chat/completions "));
            assert_eq!(body["stream"], true);
            assert_eq!(body["max_tokens"], Limits::default().response_bytes);
            assert!(body.get("temperature").is_none());
            assert_eq!(body["messages"][0]["role"], "system");
        }
    }
}

#[tokio::test]
#[ignore = "requires an explicitly configured LM Studio server and loaded tool-capable model"]
async fn live_lm_studio_tool_session() {
    let path = std::env::var_os("FERRUS_NANO_SMOKE_CONFIG")
        .expect("Set FERRUS_NANO_SMOKE_CONFIG to the private host settings file");
    let cfg = Config::load(std::path::Path::new(&path)).unwrap();
    let model = cfg.model.clone();
    let directory = TempDir::new().unwrap();
    let mut engine = engine(OpenAi::new(cfg).unwrap(), &directory);
    let end = engine.run(SessionCommand::Start { input:"Call the lookup tool with value 1. Then report the answer field from its result. Do not answer before calling the tool.".into() }, &Cancellation::default()).await.unwrap();
    assert_eq!(end.reason, EndReason::ModelFinished);
    assert!(engine.tools.calls > 0);
    assert!(engine.host.records.iter().any(|record| matches!(&record.event,
        SessionEvent::ModelCompleted { response, .. } if response.is_final() && response.text.contains("42"))));
    println!(
        "model={model} tool_calls={} reported_input={} reported_output={} estimated_input={} estimated_output={}",
        engine.tools.calls,
        end.budget.reported_input_tokens,
        end.budget.reported_output_tokens,
        end.budget.estimated_input_tokens,
        end.budget.estimated_output_tokens
    );
}
