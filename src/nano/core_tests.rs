//! Scripted engine/replay tests never call a shell, MCP peer, or inference service.

use super::{
    engine::Engine,
    journal::{FileJournal, Journal, Quotas},
    provider::*,
    replay::Replay,
    session::*,
    tools::*,
};
use anyhow::Result;
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tempfile::TempDir;

type Script = VecDeque<Result<VecDeque<ProviderEvent>, ProviderError>>;
#[derive(Default)]
struct ScriptedProvider {
    turns: Script,
    active: VecDeque<ProviderEvent>,
    requests: Vec<ModelRequest>,
    stall: bool,
}

impl Provider for ScriptedProvider {
    async fn start(&mut self, request: ModelRequest) -> Result<(), ProviderError> {
        self.requests.push(request);
        self.active = self
            .turns
            .pop_front()
            .unwrap_or(Err(ProviderError::new(ProviderErrorKind::Transport, false)))?;
        Ok(())
    }

    async fn next_event(&mut self) -> Result<Option<ProviderEvent>, ProviderError> {
        if self.stall {
            std::future::pending::<()>().await;
        }
        Ok(self.active.pop_front())
    }
}

#[derive(Default)]
struct FakeTools {
    effects: Arc<Mutex<Vec<String>>>,
    fail: bool,
    stall: bool,
    huge: bool,
}

impl Tools for FakeTools {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        vec![ToolDescriptor {
            name: "edit".into(),
            description: "Fixture edit".into(),
            input_schema: json!({"type":"object","required":["value"],"properties":{"value":{"type":"integer"}},"additionalProperties":false}),
        }]
    }

    fn validate(&self, _: &str, arguments: &serde_json::Value) -> Result<(), ToolError> {
        if arguments
            .as_object()
            .is_some_and(|obj| obj.len() == 1 && obj.get("value").is_some_and(|v| v.is_i64()))
        {
            Ok(())
        } else {
            Err(ToolError::InvalidArguments)
        }
    }

    async fn execute(&mut self, call: &ValidatedCall, _: &Cancellation) -> ToolOutcome {
        self.effects.lock().unwrap().push(call.call_id.clone());
        if self.stall {
            std::future::pending::<()>().await;
        }

        if self.fail {
            ToolOutcome::Failed(ToolError::Failed)
        } else if self.huge {
            ToolOutcome::Success(json!("x".repeat(4096)))
        } else {
            ToolOutcome::Success(call.arguments.clone())
        }
    }
}

#[derive(Default)]
struct FakeHost {
    records: Vec<Record>,
    deny: bool,
    cancel_on_intent: Option<Cancellation>,
}

impl Host for FakeHost {
    async fn authorize(&mut self, _: &ValidatedCall) -> Result<(), ToolError> {
        if self.deny {
            Err(ToolError::Denied)
        } else {
            Ok(())
        }
    }

    fn committed(&mut self, record: &Record) {
        if matches!(record.event, SessionEvent::ToolIntent { .. }) {
            if let Some(cancel) = &self.cancel_on_intent {
                cancel.cancel();
            }
        }
        self.records.push(record.clone());
    }
}

struct FailingJournal {
    inner: FileJournal,
    fail_intent: bool,
    fail_result: bool,
}

impl Journal for FailingJournal {
    fn append(&mut self, event: SessionEvent, budget: &Budget) -> Result<Record> {
        anyhow::ensure!(
            !(self.fail_intent && matches!(event, SessionEvent::ToolIntent { .. }))
                && !(self.fail_result && matches!(event, SessionEvent::ToolResult { .. })),
            "Injected journal failure"
        );
        self.inner.append(event, budget)
    }

    fn checkpoint(&mut self) -> Result<()> {
        self.inner.checkpoint()
    }
}

fn identity() -> SessionIdentity {
    SessionIdentity {
        session_id: "session-1".into(),
        project_id: "project-1".into(),
        task_id: Some("t-1".into()),
        run_id: Some("r-1".into()),
    }
}

fn call(id: &str, value: i64) -> ToolCall {
    ToolCall {
        provider_call_id: id.into(),
        name: "edit".into(),
        arguments: json!({"value":value}).to_string(),
    }
}

fn response(text: &str, calls: Vec<ToolCall>) -> ProviderEvent {
    ProviderEvent::Completed {
        response: ModelResponse {
            finish: if calls.is_empty() {
                FinishReason::Stop
            } else {
                FinishReason::ToolCalls
            },
            text: text.into(),
            calls,
            continuation: Some(json!({"signed":"opaque"})),
        },
        usage: Some(Usage {
            input_tokens: 10,
            output_tokens: 5,
            reported: true,
        }),
    }
}

fn scripted(turns: Vec<ProviderEvent>) -> ScriptedProvider {
    ScriptedProvider {
        turns: turns
            .into_iter()
            .map(|event| Ok(VecDeque::from([event])))
            .collect(),
        ..Default::default()
    }
}

fn limits() -> Limits {
    Limits {
        tokens: 1_000_000,
        ..Default::default()
    }
}

fn setup(
    provider: ScriptedProvider,
    limits: Limits,
) -> (
    TempDir,
    Engine<ScriptedProvider, FakeTools, FakeHost, FileJournal>,
) {
    let dir = TempDir::new().unwrap();
    let journal = FileJournal::create(
        &dir.path().canonicalize().unwrap(),
        "session-1",
        Quotas::default(),
    )
    .unwrap();

    let engine = Engine::new(
        identity(),
        limits,
        provider,
        FakeTools::default(),
        FakeHost::default(),
        journal,
    )
    .unwrap();

    (dir, engine)
}
async fn run<P: Provider, T: Tools, H: Host, J: Journal>(
    engine: &mut Engine<P, T, H, J>,
    cancellation: &Cancellation,
) -> SessionEnd {
    engine
        .run(
            SessionCommand::Start {
                input: "Implement a fixture task".into(),
            },
            cancellation,
        )
        .await
        .unwrap()
}

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
            SessionEvent::ToolResult { call_id, .. } => Some(format!("result:{call_id}")),
            _ => None,
        })
        .collect();
    assert_eq!(
        order,
        [
            "intent:call-1",
            "result:call-1",
            "intent:call-2",
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
async fn malformed_unknown_and_schema_invalid_calls_never_execute() {
    let mut malformed = call("a", 1);
    malformed.arguments = "{\"value\":".into();
    let mut unknown = call("b", 2);
    unknown.name = "other".into();
    let mut invalid = call("c", 3);
    invalid.arguments = "{\"value\":\"wrong\"}".into();
    let mut budget = limits();
    budget.no_progress = 5;
    let (_dir, mut engine) = setup(
        scripted(vec![
            response("", vec![malformed, unknown, invalid]),
            response("Done", vec![]),
        ]),
        budget,
    );
    assert_eq!(
        run(&mut engine, &Cancellation::default()).await.reason,
        EndReason::ModelFinished
    );
    assert!(engine.tools.effects.lock().unwrap().is_empty());
    let errors: Vec<_> = engine
        .host
        .records
        .iter()
        .filter_map(|r| match &r.event {
            SessionEvent::ToolResult {
                outcome: ToolOutcome::Failed(error),
                ..
            } => Some(error.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        errors,
        [
            ToolError::InvalidArguments,
            ToolError::UnknownTool,
            ToolError::InvalidArguments
        ]
    );
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
async fn repeated_failures_and_identical_successes_stop_no_progress_loops() {
    for fail in [false, true] {
        let provider = scripted(
            (0..8)
                .map(|n| response("", vec![call(&n.to_string(), 1)]))
                .collect(),
        );
        let (_dir, mut engine) = setup(provider, limits());
        engine.tools.fail = fail;
        let end = run(&mut engine, &Cancellation::default()).await;
        assert_eq!(end.reason, EndReason::Limit(LimitKind::NoProgress));
        assert_eq!(
            engine.tools.effects.lock().unwrap().len(),
            if fail { 3 } else { 4 }
        );
    }
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
async fn cancellation_before_authorization_never_starts_an_effect() {
    let cancellation = Cancellation::default();
    let (_dir, mut engine) = setup(scripted(vec![response("", vec![call("a", 1)])]), limits());
    engine.host.cancel_on_intent = Some(cancellation.clone());
    let end = run(&mut engine, &cancellation).await;
    assert_eq!(end.reason, EndReason::Cancelled);
    assert!(engine.tools.effects.lock().unwrap().is_empty());
    assert!(engine.journal.state().unknown_effects.is_empty());
}

#[tokio::test(start_paused = true)]
async fn cancellation_or_deadline_during_an_effect_records_unknown_outcome() {
    for cancel in [false, true] {
        let mut budget = limits();
        budget.elapsed_ms = if cancel { 30_000 } else { 1_000 };
        let (_dir, mut engine) = setup(scripted(vec![response("", vec![call("a", 1)])]), budget);
        engine.tools.stall = true;
        let cancellation = Cancellation::default();
        let control = cancellation.clone();
        let effects = engine.tools.effects.clone();
        let interrupter = async move {
            if cancel {
                loop {
                    if !effects.lock().unwrap().is_empty() {
                        control.cancel();
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            }
        };
        let (end, ()) = tokio::join!(run(&mut engine, &cancellation), interrupter);
        assert_eq!(
            end.reason,
            if cancel {
                EndReason::Cancelled
            } else {
                EndReason::Limit(LimitKind::Elapsed)
            }
        );
        assert_eq!(engine.journal.state().unknown_effects, ["call-1"]);
        assert!(engine.journal.state().pending_effect.is_none());
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
async fn truncated_or_duplicate_call_responses_never_execute() {
    for truncated in [false, true] {
        let mut event = response("Partial", vec![call("duplicate", 1), call("duplicate", 2)]);
        if let ProviderEvent::Completed { response, .. } = &mut event {
            if truncated {
                response.finish = FinishReason::Length;
            }
        }
        let (_dir, mut engine) = setup(scripted(vec![event]), limits());
        let end = run(&mut engine, &Cancellation::default()).await;
        assert_eq!(
            end.reason,
            if truncated {
                EndReason::ProviderTruncated
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

#[tokio::test]
async fn native_workspace_tools_run_through_the_durable_engine() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let journal_root = TempDir::new().unwrap();
    let mut create = call("create", 1);
    create.name = "apply_patch".into();
    create.arguments = json!({"edits":[{"operation":"create","path":"source.txt","content":"native tool result\n"}]}).to_string();
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
        super::workspace::Workspace::new(&root, super::workspace::Limits::default()).unwrap(),
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
    let replay = Replay::from_records(&engine.host.records).unwrap();
    assert_eq!(replay.budget, end.budget);
}
