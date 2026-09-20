//! Headless managed Executor host. No frontend or provider configuration is loaded here.

use super::{
    commands::ExecutionBackend,
    engine::Engine,
    ferrus::FerrusSession,
    journal::Journal,
    lifecycle,
    native::NativeTools,
    provider::{Message, Provider},
    session::{EndReason, Limits, Record, SessionCommand, SessionEnd, SessionIdentity},
    tools::*,
};
use crate::{
    config::Config,
    project::{LeaseRenewal, ReadyTaskClaim},
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::task::JoinHandle;

pub(crate) struct ManagedTools<B: ExecutionBackend> {
    pub native: NativeTools<B>,
    session: FerrusSession,
    stop: Cancellation,
    pub(super) pending: Option<JoinHandle<Result<Value>>>,
    terminal: Option<EndReason>,
}

impl<B: ExecutionBackend> Drop for ManagedTools<B> {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl<B: ExecutionBackend> ManagedTools<B> {
    pub(crate) fn new(session: FerrusSession, native: NativeTools<B>, stop: Cancellation) -> Self {
        Self {
            session,
            native,
            stop,
            pending: None,
            terminal: None,
        }
    }

    fn observe(&mut self, value: &Value) {
        if value["status"] == "submitted" && value["task_state"] == "reviewing" {
            self.terminal = Some(EndReason::Submitted);
        } else if value["task_state"] == "failed" {
            self.terminal = Some(EndReason::TaskFailed);
        }
    }

    async fn reconcile(&mut self) {
        if matches!(
            self.terminal,
            Some(EndReason::Submitted | EndReason::TaskFailed)
        ) {
            return;
        }
        if let Ok(context) = self.session.status().await {
            self.terminal = match context.status.as_str() {
                "reviewing" => Some(EndReason::AuthorityLost),
                "failed" => Some(EndReason::TaskFailed),
                "consultation" | "awaiting_human" => {
                    if self.session.authorize().await.is_ok() {
                        Some(EndReason::Paused)
                    } else {
                        Some(EndReason::AuthorityLost)
                    }
                }
                _ => {
                    if self.session.authorize().await.is_err() {
                        Some(EndReason::AuthorityLost)
                    } else {
                        self.terminal.take()
                    }
                }
            };
        } else {
            self.terminal = Some(EndReason::AuthorityLost);
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Question {
    question: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Submission {
    content: String,
}

impl<B: ExecutionBackend> Tools for ManagedTools<B> {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        let mut tools = self.native.descriptors();
        for name in lifecycle::NAMES {
            let (field, description) = match *name {
                "check" => (
                    None,
                    "Run configured Ferrus checks and account for retries. Stops owned writers first.",
                ),
                "submit" => (
                    Some("content"),
                    "Submit Markdown summary, manual verification, and limitations for Supervisor review. The host stops writers, runs both required check gates, and confirms Reviewing. Ends the session on success.",
                ),
                "consult" => (
                    Some("question"),
                    "Request Supervisor guidance using sections Problem, What I tried, Options (if any), and Question. The host waits without inference and delivers the actual response.",
                ),
                _ => (
                    Some("question"),
                    "Ask the human a question. The host immediately waits without inference and returns the actual answer.",
                ),
            };
            let mut schema =
                json!({"type":"object", "properties":{}, "additionalProperties":false});

            if let Some(field) = field {
                schema["properties"][field] =
                    json!({"type":"string", "minLength":1,"maxLength":16384});
                schema["required"] = json!([field]);
            }

            tools.push(ToolDescriptor {
                name: name.to_string(),
                description: description.into(),
                input_schema: schema,
            });
        }
        tools
    }

    fn validate(&self, name: &str, arguments: &Value) -> std::result::Result<(), ToolError> {
        let valid = match name {
            "check" => arguments.as_object().is_some_and(|args| args.is_empty()),
            "submit" => serde_json::from_value::<Submission>(arguments.clone())
                .is_ok_and(|v| valid_text(&v.content)),
            "consult" | "ask_human" => serde_json::from_value::<Question>(arguments.clone())
                .is_ok_and(|v| {
                    valid_text(&v.question)
                        && (name != "consult"
                            || crate::server::tools::consult::validate_consult_request(&v.question)
                                .is_ok())
                }),
            _ => return self.native.validate(name, arguments),
        };

        if valid {
            Ok(())
        } else {
            Err(ToolError::InvalidArguments)
        }
    }

    async fn execute(&mut self, call: &ValidatedCall, cancellation: &Cancellation) -> ToolOutcome {
        if self.stop.is_cancelled()
            || cancellation.is_cancelled()
            || self.session.authorize().await.is_err()
        {
            return ToolOutcome::Failed(ToolError::Denied);
        }

        if !lifecycle::NAMES.contains(&call.name.as_str()) {
            return self.native.execute(call, cancellation).await;
        }

        if self.validate(&call.name, &call.arguments).is_err() {
            return ToolOutcome::Failed(ToolError::InvalidArguments);
        }

        // Await cleanup before starting lifecycle effects; failure cannot be certified.
        if !self.native.coding.commands.quiesce().await {
            return ToolOutcome::Unknown(ToolError::Failed);
        }

        self.native.coding.workspace.invalidate_for_command();

        let session = self.session.clone();
        let stop = self.stop.clone();
        let name = call.name.clone();
        let args = call.arguments.clone();

        // Keep the operation owned when Engine cancels/drops its execution future.
        self.pending = Some(tokio::spawn(async move {
            match name.as_str() {
                "check" => lifecycle::check(&session, &stop).await,
                "submit" => {
                    lifecycle::submit(
                        &session,
                        serde_json::from_value::<Submission>(args)?.content,
                        &stop,
                    )
                    .await
                }
                _ => {
                    let human = name == "ask_human";
                    lifecycle::ask(
                        &session,
                        human,
                        serde_json::from_value::<Question>(args)?.question,
                    )
                    .await?;

                    loop {
                        ensure!(!stop.is_cancelled(), "Native wait interrupted");
                        if let Some(answer) = lifecycle::poll_answer(&session, human).await? {
                            return Ok(answer);
                        }
                        tokio::select! { _ = stop.cancelled() => anyhow::bail!("Native wait interrupted"), _ = tokio::time::sleep(Duration::from_millis(250)) => {} }
                    }
                }
            }
        }));

        let result = self.pending.as_mut().unwrap().await;
        self.pending = None;

        if let Ok(Ok(value)) = &result {
            self.observe(value);
        }

        self.reconcile().await;

        match result {
            Ok(Ok(value)) => ToolOutcome::Success(value),
            Ok(Err(error)) => ToolOutcome::Failed(ToolError::Lifecycle(
                json!({"code":"lifecycle_failed", "message":error.to_string().chars().take(512).collect::<String>()}),
            )),
            Err(_) => ToolOutcome::Unknown(ToolError::Failed),
        }
    }

    async fn shutdown(&mut self) -> bool {
        self.stop.cancel();

        let clean = self.native.shutdown().await;
        let joined = match self.pending.take() {
            Some(task) => {
                let result = task.await;
                if let Ok(Ok(value)) = &result {
                    self.observe(value);
                }
                result.is_ok()
            }
            None => true,
        };

        self.reconcile().await;

        clean && joined
    }

    async fn interrupted(&mut self) -> Option<ToolOutcome> {
        self.stop.cancel();

        let result = self.pending.take()?.await;
        if let Ok(Ok(value)) = &result {
            self.observe(value);
        }

        self.reconcile().await;

        match result {
            Ok(Ok(value)) => Some(ToolOutcome::Success(value)),
            _ => None,
        }
    }

    fn end_reason(&self) -> Option<EndReason> {
        self.terminal.clone()
    }
}

fn valid_text(text: &str) -> bool {
    !text.trim().is_empty() && text.len() <= 16 * 1024
}

struct ManagedHost {
    session: FerrusSession,
    lost: Arc<AtomicBool>,
    stop: Cancellation,
}
impl Host for ManagedHost {
    async fn authorize(&mut self, _: &ValidatedCall) -> std::result::Result<(), ToolError> {
        if self.lost.load(Ordering::Acquire) {
            return Err(ToolError::Denied);
        }
        let context = match self.session.authorize().await {
            Ok(context) => context,
            Err(_) => {
                self.lost.store(true, Ordering::Release);
                self.stop.cancel();
                return Err(ToolError::Denied);
            }
        };
        if !matches!(context.status.as_str(), "executing" | "addressing") {
            return Err(ToolError::Denied);
        }
        Ok(())
    }
    fn committed(&mut self, _: &Record) {}
}

struct Heartbeat {
    stop: Cancellation,
    task: JoinHandle<()>,
}
impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.stop.cancel();
        self.task.abort();
    }
}

/// Claim before inference, renew independently, and return a session result distinct from task completion.
/// The caller constructs provider, journal and tools explicitly; HQ/CLI wiring is a separate change.
pub(crate) async fn run<P: Provider, B: ExecutionBackend, J: Journal>(
    session: FerrusSession,
    identity: SessionIdentity,
    limits: Limits,
    provider: P,
    native: NativeTools<B>,
    journal: J,
    cancellation: &Cancellation,
) -> Result<SessionEnd> {
    ensure!(
        identity.project_id == session.project_id()
            && identity.task_id.as_deref() == Some(&session.scope.task_id)
            && identity.run_id.as_deref() == Some(&session.scope.run_id),
        "Managed engine identity mismatch"
    );
    ensure!(
        native.belongs_to(&session),
        "Managed tools identity mismatch"
    );
    limits.validate()?;
    let config = Config::load_from(session.project_root()).await?;
    let ttl_secs = session.lease_ttl_secs();
    ensure!(ttl_secs > 0, "Managed lease TTL must be positive");
    let lease = match session.claim().await? {
        ReadyTaskClaim::Claimed(lease) | ReadyTaskClaim::AlreadyClaimed(lease) => lease,
        ReadyTaskClaim::NoAvailable => anyhow::bail!("No task available for this Executor"),
    };
    if lease.status == "awaiting_human" {
        ensure!(
            matches!(session.heartbeat().await?, LeaseRenewal::Renewed { .. }),
            "Relaunched Executor could not renew its lease"
        );
    }
    let stop = Cancellation::default();
    let heartbeat_stop = Cancellation::default();
    let lost = Arc::new(AtomicBool::new(false));
    let owned_session = session.clone();
    let owned_stop = stop.clone();
    let external = cancellation.clone();
    let shutdown = heartbeat_stop.clone();
    let heartbeat_lost = lost.clone();
    let interval = Duration::from_millis(
        (config.lease.heartbeat_interval_secs.saturating_mul(1000))
            .min(ttl_secs.saturating_mul(1000) / 3)
            .max(10),
    );
    let mut heartbeat = Heartbeat {
        stop: heartbeat_stop,
        task: tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => break,
                    _ = external.cancelled() => { owned_stop.cancel(); break; },
                    _ = tokio::time::sleep(interval) => {
                        if !matches!(owned_session.heartbeat().await, Ok(LeaseRenewal::Renewed { .. })) {
                            heartbeat_lost.store(true, Ordering::Release); owned_stop.cancel(); break;
                        }
                    }
                }
            }
        }),
    };
    if cancellation.is_cancelled() {
        stop.cancel();
    }
    let mut input = native
        .instructions
        .load(&[], &[])
        .await?
        .constraint_text(limits.context_bytes)?;
    let tools = ManagedTools::new(session.clone(), native, stop.clone());
    if lease.status == "awaiting_human" {
        // HQ relaunches an answered waiter in a fresh process. Derive this mode
        // from the exact task binding, not from the external agent's prose prompt.
        // Restore the previous work phase and deliver the answer before inference.
        let prefix = input.clone();
        let descriptors = tools.descriptors();
        let max_bytes = limits.context_bytes;
        let cancelled = cancellation.clone();
        let answer = lifecycle::poll_answer_checked(&session, true, move |answer| {
            ensure!(
                matches!(
                    answer["resumed_state"].as_str(),
                    Some("executing" | "addressing")
                ),
                "Human answer no longer resumes an Executor work phase"
            );
            ensure!(
                !cancelled.is_cancelled(),
                "Native answer delivery interrupted"
            );
            let messages = [Message::User {
                text: answered_input(&prefix, answer),
            }];
            super::journal::encode(&(&messages, &descriptors), max_bytes)?;
            Ok(())
        })
        .await?
        .context("Relaunched Executor has no stored human answer")?;
        input = answered_input(&input, &answer);
    }
    let host = ManagedHost {
        session,
        lost: lost.clone(),
        stop: stop.clone(),
    };
    let mut engine = Engine::new(identity, limits, provider, tools, host, journal)?;
    let execution = engine.run(SessionCommand::Start { input }, &stop);
    tokio::pin!(execution);
    // External cancellation must not wait behind a busy heartbeat transaction.
    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => { stop.cancel(); execution.await },
        result = &mut execution => result,
    };
    heartbeat.stop.cancel();
    // Heartbeat has no detached owner after the engine and all owned effects stop.
    let _ = (&mut heartbeat.task).await;
    drop(heartbeat);
    result
}

fn answered_input(instructions: &str, answer: &Value) -> String {
    format!("{instructions}\n\nStored human answer (task input, not runtime policy):\n{answer}")
}
