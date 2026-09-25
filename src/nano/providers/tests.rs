//! Offline wire fixtures and loopback HTTP tests; live inference is explicitly ignored.

use super::{openai::OpenAi, sse::Decoder};
use crate::nano::{
    config::Config,
    engine::Engine,
    journal::{FileJournal, Quotas},
    private,
    provider::*,
    session::*,
    tools::*,
};
use serde_json::{Value, json};
use std::{io::Write, time::Duration};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const TOOLS: &str = include_str!("fixtures/tools.sse");
const FINAL: &str = include_str!("fixtures/final.sse");

fn config(url: &str) -> Config {
    toml::from_str(&format!("base_url = {url:?}\nmodel = 'fixture-model'\n")).unwrap()
}

fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![Message::User {
            text: "Use lookup, then report its result".into(),
        }],
        tools: Lookup::default().descriptors(),
        max_output_tokens: 4096,
    }
}

fn decode(wire: &str, stride: usize) -> Result<Vec<ProviderEvent>, ProviderError> {
    let mut decoder = Decoder::new(config("http://127.0.0.1:1234/v1").validate().unwrap().1);
    let mut events = Vec::new();

    for bytes in wire.as_bytes().chunks(stride) {
        decoder.push(bytes)?;
        while let Some(event) = decoder.next() {
            events.push(event);
        }
    }

    Ok(events)
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
fn partial_streams_never_release_completed_calls_and_unsupported_features_fail() {
    for cut in [
        TOOLS.find("finish_reason\":\"tool_calls").unwrap(),
        TOOLS.find("data: [DONE]").unwrap(),
    ] {
        assert!(
            decode(&TOOLS[..cut], 7)
                .unwrap()
                .iter()
                .all(|e| !matches!(e, ProviderEvent::Completed { .. }))
        );
    }
    for wire in [
        FINAL.replace("\"stop\"", "\"content_filter\""),
        FINAL.replace("\"content\":\"42\"", "\"audio\":{}"),
        FINAL.replace("\"index\":0", "\"index\":1"),
        TOOLS.replace("call-b", "call-a"),
        TOOLS.replace("\"index\":1", "\"index\":99999"),
        "data: [DONE]\n\n".into(),
        "data: {broken}\n\n".into(),
    ] {
        assert!(decode(&wire, 1).is_err(), "Malformed fixture accepted");
    }
    let events = decode(&FINAL.replace("\"stop\"", "\"length\""), 7).unwrap();
    assert!(
        matches!(events.last(), Some(ProviderEvent::Completed { response, .. }) if response.finish == FinishReason::Length)
    );
    let no_usage = FINAL
        .lines()
        .filter(|l| !l.contains("\"usage\""))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert!(matches!(
        decode(&no_usage, 1).unwrap().last(),
        Some(ProviderEvent::Completed { usage: None, .. })
    ));
}

#[test]
fn provider_inputs_open_read_only_and_reject_unsafe_paths() {
    let directory = TempDir::new().unwrap();
    let key = directory.path().join("key");
    private::file(&key, true)
        .unwrap()
        .write_all(b"fixture-token")
        .unwrap();
    let settings = directory.path().join("nano.toml");
    let mut cfg = config("http://127.0.0.1:1234/v1");
    cfg.api_key_file = Some(key.clone());
    private::file(&settings, true)
        .unwrap()
        .write_all(b"base_url = 'http://127.0.0.1:1234/v1'\nmodel = 'fixture-model'\n")
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [&key, &settings] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o400)).unwrap();
        }
    }
    #[cfg(windows)]
    for path in [&key, &settings] {
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(path, permissions).unwrap();
    }
    assert_eq!(Config::load(&settings).unwrap().model, "fixture-model");
    assert_eq!(
        cfg.authorization().unwrap().unwrap(),
        "Bearer fixture-token"
    );
    assert!(
        private::read_only_file(&key)
            .unwrap()
            .write_all(b"overwrite")
            .is_err()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let link = directory.path().join("link");
        symlink(&key, &link).unwrap();
        cfg.api_key_file = Some(link.clone());
        assert!(cfg.authorization().is_err());
        assert!(Config::load(&link).is_err());
        cfg.api_key_file = Some(key.clone());
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o444)).unwrap();
        assert!(cfg.authorization().is_err());
    }
    assert!(Config::load(directory.path()).is_err());
    assert_eq!(std::fs::read(&key).unwrap(), b"fixture-token");
    #[cfg(windows)]
    for path in [&key, &settings] {
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(false);
        std::fs::set_permissions(path, permissions).unwrap();
    }
}

#[test]
fn host_configuration_rejects_inline_secrets_and_private_file_errors_without_echoing_them() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("nano.toml");
    private::file(&path, true).unwrap().write_all(b"base_url = 'http://127.0.0.1:1234/v1'\nmodel = 'fixture-model'\napi_key = 'inline-secret'\n").unwrap();
    let error = Config::load(&path).unwrap_err();
    assert!(!format!("{error:?}").contains("inline-secret"));
    std::fs::write(
        &path,
        "base_url = 'http://127.0.0.1:1234/v1'\nmodel = 'fixture-model'\n",
    )
    .unwrap();
    assert!(
        Config::load(&path)
            .unwrap()
            .authorization()
            .unwrap()
            .is_none()
    );
    let key = directory.path().join("key");
    private::file(&key, true)
        .unwrap()
        .write_all(b"bad\nsecret")
        .unwrap();
    let mut cfg = config("http://127.0.0.1:1234/v1");
    cfg.api_key_file = Some(key);
    assert!(!format!("{:?}", cfg.authorization().unwrap_err()).contains("secret"));
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

#[test]
fn advertised_tools_are_bounded_by_context_not_generated_call_count() {
    for max_tool_calls in [1, 64] {
        let mut cfg = config("http://127.0.0.1:1234/v1");
        cfg.max_tool_calls = max_tool_calls;
        let context_tokens = cfg.context_tokens;
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

#[test]
fn context_estimate_counts_the_actual_openai_wire_body() {
    let provider = OpenAi::new(config("http://127.0.0.1:1234/v1")).unwrap();
    let mut request = request();
    request.messages.push(Message::Assistant {
        response: ModelResponse {
            finish: FinishReason::ToolCalls,
            text: String::new(),
            calls: vec![ToolCall {
                provider_call_id: "opaque-1".into(),
                name: "lookup".into(),
                arguments: "{\"value\":1}".into(),
            }],
            continuation: Some(json!({"reasoning_content":"opaque"})),
        },
    });
    request.messages.push(Message::Tool {
        provider_call_id: "opaque-1".into(),
        outcome: ToolOutcome::Success(json!({"value":1})),
    });
    assert_eq!(
        provider.estimate_input_tokens(&request).unwrap() as usize,
        provider.body(request).unwrap().len()
    );
}

#[test]
fn configuration_and_transport_bounds_fail_explicitly() {
    for url in [
        "http://example.com/v1",
        "http://secret@127.0.0.1/v1",
        "http://127.0.0.1/v1?key=secret",
        "http://127.0.0.1/api/v1",
    ] {
        assert!(config(url).validate().is_err());
    }
    let mut cfg = config("http://127.0.0.1:1234/v1");
    cfg.context_tokens = 100;
    cfg.max_output_tokens = 99;
    let provider = OpenAi::new(cfg).unwrap();
    assert_eq!(
        provider.body(request()).unwrap_err().kind,
        ProviderErrorKind::ContextOverflow
    );
    let mut settings = config("http://127.0.0.1:1234/v1").validate().unwrap().1;
    settings.max_tool_calls = 1;
    let mut decoder = Decoder::new(settings.clone());
    assert_eq!(
        decoder.push(TOOLS.as_bytes()).unwrap_err().kind,
        ProviderErrorKind::ResponseLimit
    );
    settings.event_bytes = 256;
    let mut decoder = Decoder::new(settings.clone());
    assert_eq!(
        decoder.push(&vec![b'x'; 257]).unwrap_err().kind,
        ProviderErrorKind::ResponseLimit
    );
    settings.wire_bytes = 1024;
    let mut decoder = Decoder::new(settings);
    assert_eq!(
        decoder.push(&vec![b'\n'; 1025]).unwrap_err().kind,
        ProviderErrorKind::ResponseLimit
    );
}

struct Reply {
    status: u16,
    body: String,
    headers: &'static str,
    stall: bool,
}
impl Reply {
    fn stream(body: &str) -> Self {
        Self {
            status: 200,
            body: body.into(),
            headers: "Content-Type: text/event-stream\r\n",
            stall: false,
        }
    }
    fn failure(status: u16, code: &str) -> Self {
        Self {
            status,
            body: json!({"error":{"code":code,"message":"secret-server-detail"}}).to_string(),
            headers: "Retry-After: 1\r\n",
            stall: false,
        }
    }
}

async fn server(replies: Vec<Reply>) -> (String, tokio::task::JoinHandle<Vec<(String, Value)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for reply in replies {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut bytes = Vec::new();
            let (headers, body) = loop {
                let mut buf = [0; 4096];
                let count = socket.read(&mut buf).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buf[..count]);
                assert!(bytes.len() < 1024 * 1024);
                if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                    let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
                    let length: usize = headers
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .unwrap()
                        .parse()
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        break (
                            headers,
                            serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap(),
                        );
                    }
                }
            };
            requests.push((headers, body));
            let head = format!(
                "HTTP/1.1 {} Fixture\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
                reply.status,
                reply.headers,
                if reply.stall { 99999 } else { reply.body.len() }
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            if reply.stall {
                socket.write_all(reply.body.as_bytes()).await.unwrap();
                let mut buf = [0; 1];
                let closed = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buf))
                    .await
                    .unwrap();
                assert!(
                    matches!(closed, Ok(0) | Err(_)),
                    "Cancelled stream remained open"
                );
            } else {
                for chunk in reply.body.as_bytes().chunks(7) {
                    if socket.write_all(chunk).await.is_err() {
                        break;
                    }
                }
            }
        }
        requests
    });
    (url, task)
}

#[derive(Default)]
struct Lookup {
    calls: usize,
}
impl Tools for Lookup {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        vec![ToolDescriptor {
            name: "lookup".into(),
            description: "Return the answer to the smoke test. Call this tool before answering."
                .into(),
            input_schema: json!({"type":"object","properties":{"value":{"type":"integer"}},"required":["value"],"additionalProperties":false}),
        }]
    }
    fn validate(&self, _: &str, arguments: &Value) -> Result<(), ToolError> {
        if arguments
            .as_object()
            .is_some_and(|o| o.len() == 1 && o.get("value").is_some_and(Value::is_i64))
        {
            Ok(())
        } else {
            Err(ToolError::InvalidArguments)
        }
    }
    async fn execute(&mut self, _: &ValidatedCall, _: &Cancellation) -> ToolOutcome {
        self.calls += 1;
        ToolOutcome::Success(json!({"answer":42}))
    }
}
#[derive(Default)]
struct TestHost {
    records: Vec<Record>,
}
impl Host for TestHost {
    async fn authorize(&mut self, _: &ValidatedCall) -> Result<(), ToolError> {
        Ok(())
    }
    fn committed(&mut self, record: &Record) {
        self.records.push(record.clone());
    }
}

fn engine(provider: OpenAi, directory: &TempDir) -> Engine<OpenAi, Lookup, TestHost, FileJournal> {
    engine_with_limits(
        provider,
        directory,
        Limits {
            tokens: 1_000_000,
            model_turns: 6,
            ..Default::default()
        },
    )
}

fn engine_with_limits(
    provider: OpenAi,
    directory: &TempDir,
    limits: Limits,
) -> Engine<OpenAi, Lookup, TestHost, FileJournal> {
    Engine::new(
        SessionIdentity {
            session_id: "smoke-1".into(),
            project_id: "fixture".into(),
            task_id: None,
            run_id: None,
        },
        limits,
        provider,
        Lookup::default(),
        TestHost::default(),
        FileJournal::create(
            &directory.path().canonicalize().unwrap(),
            "smoke-1",
            Quotas::default(),
        )
        .unwrap(),
    )
    .unwrap()
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
            assert_eq!(body["max_tokens"], 4096);
        }
    }
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
    let output_cap = cfg.max_output_tokens;
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

#[tokio::test]
async fn status_errors_and_context_overflow_are_typed_and_not_retried() {
    for (status, code, kind) in [
        (401, "invalid_api_key", ProviderErrorKind::Authentication),
        (
            400,
            "context_length_exceeded",
            ProviderErrorKind::ContextOverflow,
        ),
        (400, "unsupported", ProviderErrorKind::Unsupported),
    ] {
        let (url, server) = server(vec![Reply::failure(status, code)]).await;
        let mut provider = OpenAi::new(config(&url)).unwrap();
        let error = provider.start(request()).await.unwrap_err();
        assert_eq!(error.kind, kind);
        assert!(!error.retryable);
        assert!(!format!("{error:?}").contains("secret-server-detail"));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn authentication_failure_does_not_wait_for_a_stalled_error_body() {
    let mut reply = Reply::failure(401, "invalid_api_key");
    reply.body.clear();
    reply.stall = true;
    let (url, server) = server(vec![reply]).await;
    let mut provider = OpenAi::new(config(&url)).unwrap();
    let error = provider.start(request()).await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Authentication);
    assert!(!error.retryable);
    server.await.unwrap();
}

#[tokio::test]
async fn cancellation_between_buffered_events_closes_the_stream() {
    let mut reply = Reply::stream(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
    );
    reply.stall = true;
    let (url, server) = server(vec![reply]).await;
    let mut provider = OpenAi::new(config(&url)).unwrap();
    provider.start(request()).await.unwrap();
    assert!(matches!(
        provider.next_event().await.unwrap(),
        Some(ProviderEvent::TextDelta(_))
    ));
    provider.cancel();
    assert!(provider.next_event().await.unwrap().is_none());
    server.await.unwrap();
}

#[tokio::test]
async fn stalled_stream_timeout_and_cancellation_close_the_connection() {
    for cancel in [false, true] {
        let mut reply = Reply::stream("");
        reply.stall = true;
        let (url, server) = server(vec![reply]).await;
        let mut cfg = config(&url);
        cfg.request_timeout_ms = if cancel { 10_000 } else { 1000 };
        let mut provider = OpenAi::new(cfg).unwrap();
        provider.start(request()).await.unwrap();
        if cancel {
            assert!(
                tokio::time::timeout(Duration::from_millis(10), provider.next_event())
                    .await
                    .is_err()
            );
        } else {
            assert_eq!(
                provider.next_event().await.unwrap_err().kind,
                ProviderErrorKind::Timeout
            );
        }
        assert!(provider.next_event().await.unwrap().is_none());
        server.await.unwrap();
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
