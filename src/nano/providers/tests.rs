//! Offline wire fixtures and loopback HTTP tests; live inference is explicitly ignored.

use super::openai::{self, OpenAi};
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
    decode_with_settings(
        wire,
        stride,
        config("http://127.0.0.1:1234/v1").validate().unwrap().1,
    )
}

fn decode_with_settings(
    wire: &str,
    stride: usize,
    settings: ProviderSettings,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    let mut decoder = svir::openai::chat::Decoder::strict()
        .think(svir::Think::Keep)
        .limits(openai::limits(&settings));
    let mut events = Vec::new();

    for bytes in wire.as_bytes().chunks(stride) {
        for event in decoder.push(bytes) {
            if let Some(event) = openai::event_to_provider(event.map_err(openai::map_error)?)? {
                events.push(event);
            }
        }
    }

    Ok(events)
}

struct Reply {
    status: u16,
    body: String,
    headers: &'static str,
    stall: bool,
    stall_headers: bool,
    line_delay: Option<Duration>,
}

impl Reply {
    fn stream(body: &str) -> Self {
        Self {
            status: 200,
            body: body.into(),
            headers: "Content-Type: text/event-stream\r\n",
            stall: false,
            stall_headers: false,
            line_delay: None,
        }
    }

    fn failure(status: u16, code: &str) -> Self {
        Self {
            status,
            body: json!({"error":{"code":code,"message":"secret-server-detail"}}).to_string(),
            headers: "Retry-After: 1\r\n",
            stall: false,
            stall_headers: false,
            line_delay: None,
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
            if reply.stall_headers {
                assert_closed(&mut socket).await;
                continue;
            }

            let head = format!(
                "HTTP/1.1 {} Fixture\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
                reply.status,
                reply.headers,
                if reply.stall { 99999 } else { reply.body.len() }
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            if reply.stall {
                socket.write_all(reply.body.as_bytes()).await.unwrap();
                assert_closed(&mut socket).await;
            } else if let Some(delay) = reply.line_delay {
                for line in reply.body.split_inclusive('\n') {
                    if socket.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }

                    tokio::time::sleep(delay).await;
                }
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

async fn assert_closed(socket: &mut tokio::net::TcpStream) {
    let mut buf = [0; 1];
    let closed = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buf))
        .await
        .unwrap();
    assert!(
        matches!(closed, Ok(0) | Err(_)),
        "Cancelled stream remained open"
    );
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

#[path = "tests/behavior.rs"]
mod behavior_tests;
#[path = "tests/limits.rs"]
mod limits_tests;
#[path = "tests/provider.rs"]
mod provider_tests;
#[path = "tests/recovery.rs"]
mod recovery_tests;
#[path = "tests/review.rs"]
mod review_tests;

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
    cfg.context_tokens = Some(100);
    cfg.max_output_tokens = Some(99);
    let provider = OpenAi::new(cfg).unwrap();
    assert_eq!(
        provider.body(request()).unwrap_err().kind,
        ProviderErrorKind::ContextOverflow
    );
    let mut settings = config("http://127.0.0.1:1234/v1").validate().unwrap().1;
    settings.max_tool_calls = 1;
    assert_eq!(
        decode_with_settings(TOOLS, 8192, settings.clone())
            .unwrap_err()
            .kind,
        ProviderErrorKind::ResponseLimit
    );
    settings.event_bytes = 256;
    assert_eq!(
        decode_with_settings(&"x".repeat(257), 8192, settings.clone())
            .unwrap_err()
            .kind,
        ProviderErrorKind::ResponseLimit
    );
    settings.wire_bytes = 1024;
    assert_eq!(
        decode_with_settings(&"\n".repeat(1025), 8192, settings)
            .unwrap_err()
            .kind,
        ProviderErrorKind::ResponseLimit
    );
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
async fn active_stream_can_outlast_the_provider_idle_timeout() {
    let body = format!("{}{FINAL}", ": generating\n".repeat(6));
    let mut reply = Reply::stream(&body);
    reply.line_delay = Some(Duration::from_millis(250));
    let (url, server) = server(vec![reply]).await;
    let mut cfg = config(&url);
    cfg.request_timeout_ms = 1000;
    let mut provider = OpenAi::new(cfg).unwrap();
    provider.start(request()).await.unwrap();
    let mut completed = false;
    while let Some(event) = provider.next_event().await.unwrap() {
        if let ProviderEvent::Completed { response, .. } = event {
            assert!(response.is_final());
            completed = true;
        }
    }

    assert!(completed);
    server.await.unwrap();
}

#[tokio::test]
async fn stalled_headers_remain_bounded_by_the_provider_idle_timeout() {
    let mut reply = Reply::stream("");
    reply.stall_headers = true;
    let (url, server) = server(vec![reply]).await;
    let mut cfg = config(&url);
    cfg.request_timeout_ms = 1000;
    let mut provider = OpenAi::new(cfg).unwrap();
    assert_eq!(
        provider.start(request()).await.unwrap_err().kind,
        ProviderErrorKind::Timeout
    );
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

#[test]
fn omitted_context_and_temperature_delegate_to_the_server() {
    let cfg = config("http://127.0.0.1:1234/v1");
    let settings = cfg.validate().unwrap().1;
    assert_eq!(settings.context_tokens, None);
    assert_eq!(settings.temperature, None);
    let encoded = serde_json::to_value(&settings).unwrap();
    assert!(encoded.get("context_tokens").is_none());
    assert!(encoded.get("temperature").is_none());
    let mut req = request();
    req.messages = vec![
        Message::System {
            text: "Host policy".into(),
        },
        Message::User {
            text: "x".repeat(40_000),
        },
    ];
    let provider = OpenAi::new(cfg).unwrap();
    let body: Value = serde_json::from_slice(&provider.body(req.clone()).unwrap()).unwrap();
    assert!(body.get("temperature").is_none());
    assert!(body.get("context_tokens").is_none());
    assert_eq!(
        body["messages"][0],
        json!({"role":"system", "content":"Host policy"})
    );
    assert_eq!(body["messages"][1]["role"], "user");
    assert!(provider.estimate_input_tokens(&req).unwrap() > 32_768);

    let mut cfg = config("http://127.0.0.1:1234/v1");
    cfg.temperature = Some(1.0);
    cfg.context_tokens = Some(65_536);
    let explicit = cfg.validate().unwrap().1;
    let body: Value =
        serde_json::from_slice(&OpenAi::new(cfg).unwrap().body(req.clone()).unwrap()).unwrap();
    assert_eq!(body["temperature"], 1.0);
    assert_ne!(explicit, settings);
    assert_eq!(
        serde_json::from_value::<ProviderSettings>(serde_json::to_value(&explicit).unwrap())
            .unwrap(),
        explicit
    );
    let mut cfg = config("http://127.0.0.1:1234/v1");
    cfg.context_tokens = Some(32_768);
    assert_eq!(
        OpenAi::new(cfg).unwrap().body(req).unwrap_err().kind,
        ProviderErrorKind::ContextOverflow
    );
    for invalid in [0, 1, 16_777_217] {
        let mut cfg = config("http://127.0.0.1:1234/v1");
        cfg.context_tokens = Some(invalid);
        assert!(cfg.validate().is_err());
    }

    for invalid in [-0.1, 2.1, f64::NAN, f64::INFINITY] {
        let mut cfg = config("http://127.0.0.1:1234/v1");
        cfg.temperature = Some(invalid);
        assert!(cfg.validate().is_err());
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

#[tokio::test]
async fn unexpected_content_type_keeps_typed_diagnostic_without_body_or_headers() {
    for headers in [
        "",
        "Content-Type: application/json; secret-header-detail\r\n",
    ] {
        let mut reply = Reply::stream("secret-response-body");
        reply.headers = headers;
        reply.stall = true;
        let (url, server) = server(vec![reply]).await;
        let mut provider = OpenAi::new(config(&url)).unwrap();
        let error = provider.start(request()).await.unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::Unsupported);
        assert!(!error.retryable);
        assert_eq!(
            error.diagnostic,
            Some(ProviderDiagnostic::UnexpectedContentType)
        );
        assert!(!format!("{error:?}").contains("secret-"));
        assert!(provider.next_event().await.unwrap().is_none());
        server.await.unwrap();
    }
}

#[test]
fn output_ceiling_is_optional_and_old_settings_remain_readable() {
    let mut cfg = config("http://127.0.0.1:1234/v1");
    assert!(cfg.max_output_tokens.is_none());
    let settings = cfg.validate().unwrap().1;
    let provider = OpenAi::new(cfg).unwrap();
    let mut req = request();
    req.max_output_tokens = 16_384;
    let body: Value = serde_json::from_slice(&provider.body(req.clone()).unwrap()).unwrap();
    assert_eq!(body["max_tokens"], 16_384);
    let mut encoded = serde_json::to_value(&settings).unwrap();
    assert!(encoded.get("max_output_tokens").is_none());
    encoded["max_output_tokens"] = json!(4096);
    let legacy: ProviderSettings = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(legacy.max_output_tokens, Some(4096));
    assert_eq!(serde_json::to_value(legacy).unwrap(), encoded);
    cfg = config("http://127.0.0.1:1234/v1");
    cfg.max_output_tokens = Some(4096);
    let provider = OpenAi::new(cfg).unwrap();
    let body: Value = serde_json::from_slice(&provider.body(req).unwrap()).unwrap();
    assert_eq!(body["max_tokens"], 4096);
    for cap in [0, 32_768] {
        let mut cfg = config("http://127.0.0.1:1234/v1");
        cfg.context_tokens = Some(32_768);
        cfg.max_output_tokens = Some(cap);
        assert!(cfg.validate().is_err());
    }
}

#[tokio::test]
async fn automatic_output_fits_remaining_context_without_compacting_active_task() {
    let (url, server) = server(vec![Reply::stream(FINAL)]).await;
    let directory = TempDir::new().unwrap();
    let mut cfg = config(&url);
    cfg.context_tokens = Some(10_000);
    let mut engine = engine(OpenAi::new(cfg).unwrap(), &directory);
    let text = "x".repeat(8000);
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
    assert!(
        !engine
            .host
            .records
            .iter()
            .any(|record| matches!(&record.event, SessionEvent::CompactionStarted { .. }))
    );
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 1);
    let body = &requests[0].1;
    let cap = body["max_tokens"].as_u64().unwrap();
    assert!((1..2500).contains(&cap));
    assert!(serde_json::to_vec(body).unwrap().len() as u64 + cap + 500 <= 10_000);
    assert_eq!(body["messages"][0]["content"], text);
}
