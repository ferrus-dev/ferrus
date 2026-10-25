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
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
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
    uncertain_cleanup: bool,
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

    async fn shutdown(&mut self) -> bool {
        !self.uncertain_cleanup
    }
}

#[derive(Default)]
struct FakeHost {
    records: Vec<Record>,
    deny: bool,
    cancel_on_intent: Option<Cancellation>,
    cancel_on_input: Option<Cancellation>,
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

        if matches!(record.event, SessionEvent::InputRequested)
            && let Some(cancel) = &self.cancel_on_input
        {
            cancel.cancel();
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

struct EvidenceTools {
    workspace: super::workspace::Workspace,
    enabled: bool,
    preparation_delay: Duration,
}

impl Tools for EvidenceTools {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.workspace.descriptors()
    }

    fn validate(&self, name: &str, args: &serde_json::Value) -> Result<(), ToolError> {
        self.workspace.validate(name, args)
    }

    async fn execute(&mut self, call: &ValidatedCall, cancel: &Cancellation) -> ToolOutcome {
        self.workspace.execute(call, cancel).await
    }

    async fn prepare_context(
        &mut self,
        messages: &[Message],
        _: &Cancellation,
    ) -> Result<Option<super::working_set::Preparation>, ToolError> {
        if !self.preparation_delay.is_zero() {
            tokio::time::sleep(self.preparation_delay).await;
        }

        if !self.enabled {
            return Ok(None);
        }

        super::working_set::prepare(messages, &json!({}), &json!({}), &self.workspace, false)
            .map(Some)
            .map_err(|_| ToolError::OutputLimit)
    }
}

struct RejectPreparation(FileJournal);

impl Journal for RejectPreparation {
    fn append(&mut self, event: SessionEvent, budget: &Budget) -> Result<Record> {
        anyhow::ensure!(
            !matches!(event, SessionEvent::ContextPrepared { .. }),
            "injected preparation commit failure"
        );
        self.0.append(event, budget)
    }

    fn checkpoint(&mut self) -> Result<()> {
        self.0.checkpoint()
    }
}

#[derive(Default)]
struct CompactionProvider {
    normal_turns: usize,
    summaries: usize,
    read_calls: bool,
    normal_text_bytes: usize,
    fail_summary: bool,
    summary_failure_retryable: bool,
    summary_stream_error: Option<ProviderErrorKind>,
    stall_summary: bool,
    active_summary: bool,
    summary_started: Arc<AtomicBool>,
    active: VecDeque<ProviderEvent>,
    requests: Vec<ModelRequest>,
}

impl Provider for CompactionProvider {
    async fn start(&mut self, request: ModelRequest) -> Result<(), ProviderError> {
        let event = if request.tools.is_empty() {
            self.summaries += 1;
            self.active_summary = true;
            self.summary_started.store(true, Ordering::SeqCst);
            if self.fail_summary {
                return Err(ProviderError::new(
                    ProviderErrorKind::Transport,
                    self.summary_failure_retryable,
                ));
            }

            ProviderEvent::Completed {
                response: ModelResponse {
                    finish: FinishReason::Stop,
                    text: "Earlier edits completed; recheck source before changing it.".into(),
                    calls: Vec::new(),
                    continuation: None,
                },
                usage: Some(Usage {
                    input_tokens: 17,
                    output_tokens: 9,
                    reported: true,
                }),
            }
        } else if self.normal_turns < 10 {
            self.active_summary = false;
            self.normal_turns += 1;
            let id = format!("call-{}", self.normal_turns);
            let call = if self.read_calls {
                ToolCall {
                    provider_call_id: id,
                    name: "read_file".into(),
                    arguments: json!({"path":"source.txt"}).to_string(),
                }
            } else {
                call(&id, self.normal_turns as i64)
            };
            let mut event = response("", vec![call]);
            if let ProviderEvent::Completed { response, .. } = &mut event {
                response.text = "x".repeat(self.normal_text_bytes);
            }

            event
        } else {
            self.active_summary = false;
            response("Done", vec![])
        };
        self.requests.push(request);
        self.active = VecDeque::from([event]);
        Ok(())
    }

    async fn next_event(&mut self) -> Result<Option<ProviderEvent>, ProviderError> {
        if self.active_summary && self.stall_summary {
            std::future::pending::<()>().await;
        }

        if self.active_summary {
            if let Some(kind) = self.summary_stream_error.take() {
                return Err(ProviderError::new(kind, false));
            }
        }

        Ok(self.active.pop_front())
    }
}

struct PreparedReads;

impl Tools for PreparedReads {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        vec![ToolDescriptor {
            name: "read_file".into(),
            description: "Fixture read".into(),
            input_schema: json!({"type":"object","required":["path"]}),
        }]
    }

    fn validate(&self, name: &str, arguments: &serde_json::Value) -> Result<(), ToolError> {
        if name == "read_file" && arguments["path"] == "source.txt" {
            Ok(())
        } else {
            Err(ToolError::InvalidArguments)
        }
    }

    async fn execute(&mut self, _: &ValidatedCall, _: &Cancellation) -> ToolOutcome {
        ToolOutcome::Success(json!({
            "text":"x".repeat(700),
            "source":{"path":"source.txt","digest":"a".repeat(64)}
        }))
    }

    async fn prepare_context(
        &mut self,
        messages: &[Message],
        _: &Cancellation,
    ) -> Result<Option<super::working_set::Preparation>, ToolError> {
        let replacements = messages
            .iter()
            .enumerate()
            .find_map(|(index, message)| {
                matches!(message, Message::Tool { .. }).then_some(super::working_set::Replacement {
                    message: index,
                    value: json!({"kind":"evidence_reference","marker":"prepared-input"}),
                })
            })
            .into_iter()
            .collect();
        Ok(Some(super::working_set::Preparation {
            replacements,
            observations: vec![json!({"kind":"prefetch", "marker":"prepared-input"})],
            ..Default::default()
        }))
    }
}

struct RejectPostCompactionPreparation {
    inner: FileJournal,
    compacted: bool,
}

impl Journal for RejectPostCompactionPreparation {
    fn append(&mut self, event: SessionEvent, budget: &Budget) -> Result<Record> {
        anyhow::ensure!(
            !(self.compacted && matches!(event, SessionEvent::ContextPrepared { .. })),
            "injected final projection commit failure"
        );
        let compacted = matches!(event, SessionEvent::CompactionCompleted { .. });
        let record = self.inner.append(event, budget)?;
        self.compacted |= compacted;
        Ok(record)
    }

    fn checkpoint(&mut self) -> Result<()> {
        self.inner.checkpoint()
    }
}

struct UnsupportedEstimate;

impl Provider for UnsupportedEstimate {
    fn estimate_input_tokens(&self, _: &ModelRequest) -> Result<u64, ProviderError> {
        Err(ProviderError::new(ProviderErrorKind::Unsupported, false))
    }

    async fn start(&mut self, _: ModelRequest) -> Result<(), ProviderError> {
        panic!("unsupported request must not reach the provider")
    }

    async fn next_event(&mut self) -> Result<Option<ProviderEvent>, ProviderError> {
        panic!("unsupported request must not start a stream")
    }
}

struct InteractiveJournal {
    inner: FileJournal,
    commands: Option<tokio::sync::mpsc::Receiver<SessionCommand>>,
    sender: tokio::sync::mpsc::Sender<SessionCommand>,
    interactive: bool,
    waits: usize,
    user_requests: usize,
    fail_input: bool,
    inject_steering: bool,
}

impl Journal for InteractiveJournal {
    fn append(&mut self, event: SessionEvent, budget: &Budget) -> Result<Record> {
        anyhow::ensure!(
            !(self.fail_input && matches!(event, SessionEvent::UserInput { .. })),
            "Injected input failure"
        );
        let record = self.inner.append(event, budget)?;
        match &record.event {
            // Simulate typing while inference is active, before its two tool effects.
            SessionEvent::ModelStarted { turn: 1 } if self.inject_steering => {
                self.sender.try_send(SessionCommand::Interact).unwrap();
                self.sender
                    .try_send(SessionCommand::Steer {
                        input_id: None,
                        text: "Keep the public API stable.".into(),
                    })
                    .unwrap();
            }
            SessionEvent::InputRequested => {
                self.waits += 1;
                let command = if self.waits <= self.user_requests {
                    SessionCommand::Steer {
                        input_id: None,
                        text: "Explain the completed checks.".into(),
                    }
                } else {
                    SessionCommand::Cancel
                };
                self.sender.try_send(command).unwrap();
            }
            _ => (),
        }

        Ok(record)
    }

    fn checkpoint(&mut self) -> Result<()> {
        self.inner.checkpoint()
    }

    fn interactive(&self) -> bool {
        self.interactive
    }

    fn take_commands(&mut self) -> Option<tokio::sync::mpsc::Receiver<SessionCommand>> {
        self.commands.take()
    }
}

#[path = "core_tests/recovery.rs"]
mod recovery_tests;

#[path = "core_tests/behavior.rs"]
mod behavior_tests;

#[path = "core_tests/provider.rs"]
mod provider_tests;

#[path = "core_tests/cancellation.rs"]
mod cancellation_tests;

#[path = "core_tests/limits.rs"]
mod limits_tests;

#[path = "core_tests/context.rs"]
mod context_tests;

#[path = "core_tests/compaction.rs"]
mod compaction_tests;

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
async fn interactive_exhausted_turns_neither_request_nor_accept_more_input() {
    for queued in [false, true] {
        for limit in [LimitKind::ModelTurns, LimitKind::Tokens] {
            for remaining in [0, 1, 2, 32] {
                let mut limits = limits();
                let mut event = response("Request completed.", vec![]);
                match limit {
                    LimitKind::ModelTurns => limits.model_turns = 1,
                    LimitKind::Tokens => {
                        let ProviderEvent::Completed { usage, .. } = &mut event else {
                            unreachable!()
                        };
                        *usage = Some(Usage {
                            input_tokens: limits.tokens - remaining,
                            output_tokens: 0,
                            reported: true,
                        });
                    }
                    _ => unreachable!(),
                }

                let (_dir, base) = setup(scripted(vec![event]), limits.clone());
                let (sender, commands) = tokio::sync::mpsc::channel(8);
                let journal = InteractiveJournal {
                    inner: base.journal,
                    commands: Some(commands),
                    sender,
                    interactive: !queued,
                    waits: 0,
                    user_requests: 2,
                    fail_input: false,
                    inject_steering: queued,
                };
                let mut identity = identity();
                if !queued {
                    identity.task_id = None;
                }

                let mut engine = Engine::new(
                    identity,
                    limits,
                    base.provider,
                    base.tools,
                    base.host,
                    journal,
                )
                .unwrap();
                let end = engine
                    .run(
                        SessionCommand::Start {
                            input: "Workspace constraints".into(),
                        },
                        &Cancellation::default(),
                    )
                    .await
                    .unwrap();
                assert_eq!(end.reason, EndReason::Limit(limit.clone()));
                assert!(end.durable);
                assert_eq!(engine.provider.requests.len(), 1);
                let records = &engine.host.records;
                assert_eq!(
                    records
                        .iter()
                        .filter(|r| matches!(r.event, SessionEvent::InputRequested))
                        .count(),
                    usize::from(!queued)
                );
                assert_eq!(
                    records
                        .iter()
                        .filter(|r| matches!(r.event, SessionEvent::UserInput { .. }))
                        .count(),
                    usize::from(!queued)
                );
                assert!(records.iter().any(|r| matches!(&r.event, SessionEvent::ModelCompleted { response, .. } if response.text == "Request completed.")));
                Replay::from_records(records).unwrap();
            }
        }
    }
}
