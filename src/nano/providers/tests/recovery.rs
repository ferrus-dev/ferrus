//! Tests for recovery.

use super::*;

#[tokio::test]
async fn normalized_temperature_matches_wire_journal_and_settings_digest() {
    let (url, server) = server(vec![Reply::stream(FINAL)]).await;
    let mut cfg = config(&url);
    cfg.temperature = Some(0.123456789123);
    let legacy = cfg.validate().unwrap().1;
    let provider = OpenAi::new(cfg).unwrap();
    let effective = provider.settings().unwrap();
    assert_eq!(effective.temperature, Some(0.12345679));
    let mut old_record = serde_json::to_value(&legacy).unwrap();
    old_record["temperature"] = json!(0.123456789123);
    assert_eq!(
        serde_json::from_value::<ProviderSettings>(old_record).unwrap(),
        legacy
    );
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
    let directory = TempDir::new().unwrap();
    let mut engine = engine(provider, &directory);
    engine.set_launch_evidence(evidence.clone());
    let end = engine
        .run(
            SessionCommand::Start {
                input: "Answer".into(),
            },
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(end.reason, EndReason::ModelFinished);
    let SessionEvent::Started {
        provider: Some(recorded),
        launch_evidence: Some(recorded_evidence),
        limits: recorded_limits,
        ..
    } = &engine.host.records[0].event
    else {
        panic!("Missing provider evidence");
    };
    assert_eq!(**recorded, effective);
    assert_eq!(
        recorded_evidence.settings_sha256.as_ref().unwrap(),
        &effective_settings_sha256(&effective, recorded_limits, &evidence).unwrap()
    );
    let requests = server.await.unwrap();
    assert_eq!(
        requests[0].1["temperature"],
        json!(recorded.temperature.unwrap())
    );
    let replay = crate::nano::replay::Replay::from_records(&engine.host.records).unwrap();
    assert_eq!(replay.budget, end.budget);
}

#[tokio::test]
async fn length_finish_continues_over_http_without_replaying_partial_calls() {
    let partial = TOOLS
        .replace("\"tool_calls\"}]", "\"length\"}]")
        .replace("\"id\":\"call-a\"", "\"id\":\"\"");
    let (url, server) = server(vec![
        Reply::stream(&partial),
        Reply::stream(TOOLS),
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
    assert_eq!(end.budget.retries, 0);
    assert_eq!(end.budget.model_turns, 3);
    assert_eq!(end.budget.reported_input_tokens, 48);
    assert_eq!(end.budget.reported_output_tokens, 19);
    let requests = server.await.unwrap();
    let body = &requests[1].1;
    assert_eq!(body["messages"].as_array().unwrap().len(), 3);
    assert!(body["messages"][1].get("tool_calls").is_none());
    assert_eq!(body["messages"][1]["reasoning_content"], "opaque reasoning");
    assert_eq!(body["messages"][2]["role"], "user");
    assert!(
        body["messages"][2]["content"]
            .as_str()
            .unwrap()
            .contains("No tools from that response were executed")
    );
    let replay = crate::nano::replay::Replay::from_records(&engine.host.records).unwrap();
    assert_eq!(replay.budget, end.budget);
    assert!(replay.checkpoint_ready());
}
