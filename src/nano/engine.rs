//! Sequential bounded inference/tool loop. Journal commits precede effects and notifications.

use super::{
    compaction::{self, Projection, Summary},
    journal::{Journal, encode, valid_id},
    provider::{
        FinishReason, Message, ModelRequest, Provider, ProviderError, ProviderErrorKind,
        ProviderEvent, Usage,
    },
    session::{
        Budget, ContextComposition, EndReason, LaunchEvidence, LimitKind, Limits, SessionCommand,
        SessionEnd, SessionEvent, SessionIdentity, effective_settings_sha256,
    },
    tools::{Cancellation, Host, ToolCall, ToolError, ToolOutcome, Tools, ValidatedCall},
};
use anyhow::{Result, ensure};
use std::collections::HashSet;
use tokio::time::{Duration, Instant};

pub(crate) struct Engine<P, T, H, J> {
    identity: SessionIdentity,
    limits: Limits,
    pub provider: P,
    pub tools: T,
    pub host: H,
    pub journal: J,
    budget: Budget,
    messages: Vec<Message>,
    system_prompt: Option<String>,
    interactive: bool,
    latest_steering: Option<usize>,
    commands: Option<tokio::sync::mpsc::Receiver<SessionCommand>>,
    summary: Option<Summary>,
    last_request: Vec<Message>,
    previous_call: Option<(String, serde_json::Value)>,
    started: Option<Instant>,
    launch_evidence: Option<LaunchEvidence>,
    retryable_provider_failure: bool,
}

#[derive(Clone, Copy)]
struct Capacity {
    output: u64,
    remaining: u64,
    window: u64,
}

struct CompactionPlan {
    summary: Summary,
    request: ModelRequest,
    input: u64,
    summary_cap: usize,
}

impl<P: Provider, T: Tools, H: Host, J: Journal> Engine<P, T, H, J> {
    pub(crate) fn new(
        identity: SessionIdentity,
        limits: Limits,
        provider: P,
        tools: T,
        host: H,
        mut journal: J,
    ) -> Result<Self> {
        limits.validate()?;
        ensure!(
            valid_id(&identity.session_id)
                && !identity.project_id.is_empty()
                && (identity.task_id.is_none() || identity.run_id.is_some()),
            "Invalid session identity"
        );

        let interactive = journal.interactive();
        let commands = journal.take_commands();
        Ok(Self {
            identity,
            limits,
            provider,
            tools,
            host,
            journal,
            budget: Budget::default(),
            messages: Vec::new(),
            system_prompt: None,
            commands,
            interactive,
            latest_steering: None,
            summary: None,
            last_request: Vec::new(),
            previous_call: None,
            started: None,
            launch_evidence: None,
            retryable_provider_failure: false,
        })
    }

    pub(crate) fn set_launch_evidence(&mut self, evidence: LaunchEvidence) {
        self.launch_evidence = Some(evidence);
    }

    pub(crate) fn set_system_prompt(&mut self, prompt: &str) -> Result<()> {
        ensure!(
            self.started.is_none() && !prompt.trim().is_empty(),
            "Invalid system prompt"
        );
        self.system_prompt = Some(prompt.into());
        Ok(())
    }

    pub(crate) fn inherit_budget(&mut self, budget: Budget) -> Result<()> {
        ensure!(
            self.started.is_none()
                && budget.elapsed_ms == 0
                && budget.reserved_input_tokens == 0
                && budget.reserved_output_tokens == 0,
            "Invalid recovered budget"
        );
        self.budget = budget;
        Ok(())
    }

    /// One managed attempt per engine; frontends submit steering at safe turn boundaries.
    pub(crate) async fn run(
        &mut self,
        command: SessionCommand,
        cancellation: &Cancellation,
    ) -> Result<SessionEnd> {
        ensure!(self.started.is_none(), "Session already started");

        let start = Instant::now();
        let deadline = start
            .checked_add(Duration::from_millis(self.limits.elapsed_ms))
            .ok_or_else(|| anyhow::anyhow!("Session deadline overflow"))?;

        self.started = Some(start);
        let input = match command {
            SessionCommand::Start { input } => input,
            SessionCommand::Cancel => {
                cancellation.cancel();
                String::new()
            }
            SessionCommand::Steer { .. } | SessionCommand::Interact => {
                anyhow::bail!("Expected session start")
            }
        };

        if input
            .len()
            .saturating_add(self.system_prompt.as_ref().map_or(0, String::len))
            > self.limits.context_bytes
        {
            // An oversized input is never copied into the journal.
            return Ok(SessionEnd {
                reason: EndReason::Limit(LimitKind::ContextBytes),
                budget: self.budget.clone(),
                durable: false,
                retryable_provider_failure: false,
            });
        }

        let provider_settings = self.provider.settings();
        let mut launch_evidence = self.launch_evidence.clone();
        if let (Some(evidence), Some(provider)) = (&mut launch_evidence, &provider_settings) {
            evidence.settings_sha256 =
                Some(effective_settings_sha256(provider, &self.limits, evidence)?);
        }

        if !self.commit(SessionEvent::Started {
            identity: self.identity.clone(),
            limits: self.limits.clone(),
            input: input.clone(),
            system_prompt: self.system_prompt.clone(),
            launch_evidence: launch_evidence.map(Box::new),
            inherited_budget: (self.budget != Budget::default()).then(|| self.budget.clone()),
            provider: provider_settings.map(Box::new),
        }) {
            return Ok(self.journal_failure());
        }

        if let Some(text) = &self.system_prompt {
            self.messages.push(Message::System { text: text.clone() });
        }

        self.messages.push(Message::User { text: input });

        if self.interactive && !self.commit(SessionEvent::InteractionOpened) {
            return Ok(self.journal_failure());
        }

        let mut reason = if self.interactive && self.identity.task_id.is_none() {
            if let Err(reason) = self.input_capacity("") {
                reason
            } else if !self.commit(SessionEvent::InputRequested) {
                EndReason::JournalFailed
            } else {
                match self.wait_for_input(cancellation, deadline).await {
                    Ok(()) => self.drive(cancellation, deadline).await,
                    Err(reason) => reason,
                }
            }
        } else {
            self.drive(cancellation, deadline).await
        };

        self.provider.cancel();

        let clean = self.tools.shutdown().await;
        if reason != EndReason::JournalFailed {
            if !clean {
                reason = EndReason::EffectUnknown;
            } else if let Some(terminal) = self.tools.end_reason() {
                reason = terminal;
            }
        }

        if reason == EndReason::JournalFailed {
            return Ok(self.journal_failure());
        }

        self.update_elapsed();
        if !self.commit(SessionEvent::Ended {
            reason: reason.clone(),
        }) {
            return Ok(self.journal_failure());
        }

        Ok(SessionEnd {
            reason,
            budget: self.budget.clone(),
            durable: true,
            retryable_provider_failure: self.retryable_provider_failure,
        })
    }

    fn accept_input(&mut self, command: SessionCommand) -> Result<bool, EndReason> {
        if matches!(
            command,
            SessionCommand::Cancel | SessionCommand::Start { .. }
        ) {
            return Err(EndReason::Cancelled);
        }

        if !self.interactive {
            if !self.commit(SessionEvent::InteractionOpened) {
                return Err(EndReason::JournalFailed);
            }

            self.interactive = true;
        }

        let SessionCommand::Steer { text, input_id } = command else {
            return Ok(false);
        };
        if text.trim().is_empty()
            || text.len() > super::wire::FRAME_BYTES
            || input_id
                .as_ref()
                .is_some_and(|id| !super::journal::valid_id(id))
        {
            return Err(EndReason::ProviderProtocol);
        }

        self.input_capacity(&text)?;
        let previous_no_progress = std::mem::replace(&mut self.budget.no_progress, 0);
        if !self.commit(SessionEvent::UserInput {
            text: text.clone(),
            input_id,
        }) {
            self.budget.no_progress = previous_no_progress;
            return Err(EndReason::JournalFailed);
        }

        self.previous_call = None;
        self.latest_steering = Some(self.messages.len());
        self.messages.push(Message::User { text });
        self.journal
            .checkpoint()
            .map_err(|_| EndReason::JournalFailed)?;
        Ok(true)
    }

    fn drain_input(&mut self) -> Result<bool, EndReason> {
        let mut accepted = false;
        for _ in 0..8 {
            let command = self
                .commands
                .as_mut()
                .and_then(|commands| commands.try_recv().ok());
            let Some(command) = command else { break };
            accepted |= self.accept_input(command)?;
        }

        Ok(accepted)
    }

    async fn wait_for_input(
        &mut self,
        cancellation: &Cancellation,
        deadline: Instant,
    ) -> Result<(), EndReason> {
        loop {
            let Some(commands) = self.commands.as_mut() else {
                return Err(EndReason::Cancelled);
            };
            match interrupt(commands.recv(), cancellation, deadline).await {
                Ok(Some(command)) => {
                    if self.accept_input(command)? {
                        return Ok(());
                    }
                }
                Ok(None) => return Err(EndReason::Cancelled),
                Err(reason) => return Err(reason),
            }
        }
    }

    async fn drive(&mut self, cancellation: &Cancellation, deadline: Instant) -> EndReason {
        loop {
            if let Some(reason) = self.stop(cancellation, deadline) {
                return reason;
            }

            if let Some(reason) = self.next_turn_limit() {
                return reason;
            }

            if let Err(reason) = self.drain_input() {
                return reason;
            }

            let assembly_start = Instant::now();
            let preparation = match interrupt(
                self.tools.prepare_context(&self.messages, cancellation),
                cancellation,
                deadline,
            )
            .await
            {
                Ok(Ok(preparation)) => preparation,
                Ok(Err(super::tools::ToolError::Denied)) => return EndReason::AuthorityLost,
                Ok(Err(super::tools::ToolError::Interrupted)) => return EndReason::Cancelled,
                Ok(Err(_)) => return EndReason::Limit(LimitKind::ContextBytes),
                Err(reason) => return reason,
            };
            let had_preparation = preparation.is_some();
            let mut preparation = preparation.unwrap_or_default();
            if preparation.projection.is_some() {
                return EndReason::Limit(LimitKind::ContextBytes);
            }

            let base = match preparation.apply(&self.messages) {
                Ok(messages) => messages,
                Err(_) => return EndReason::Limit(LimitKind::ContextBytes),
            };
            let descriptors = self.tools.descriptors();
            let mut names = HashSet::new();
            if descriptors
                .iter()
                .any(|tool| tool.name.is_empty() || !names.insert(tool.name.clone()))
            {
                return EndReason::ProviderProtocol;
            }

            let mut remaining = self.limits.tokens.saturating_sub(self.budget.tokens());
            let window = self
                .provider
                .settings()
                .map_or(self.limits.context_bytes as u64, |settings| {
                    settings.context_tokens.unwrap_or(u64::MAX)
                });
            let mut output_reservation = self.output_reservation(remaining, window);
            if output_reservation == 0 {
                return EndReason::Limit(LimitKind::Tokens);
            }

            if let Some(summary) = &self.summary {
                preparation.projection = Some(Projection {
                    summary: Some(summary.clone()),
                    ..Default::default()
                });
            }

            // Compaction may call the provider before the final projection is known.
            // Persist host replacements and observations before that inference.
            let committed_preparation = if had_preparation {
                if !self.commit(SessionEvent::ContextPrepared {
                    preparation: preparation.clone(),
                }) {
                    return EndReason::JournalFailed;
                }

                Some(preparation.clone())
            } else {
                None
            };
            let mut messages = match preparation.apply(&self.messages) {
                Ok(messages) => messages,
                Err(_) => return EndReason::Limit(LimitKind::ContextBytes),
            };
            let mut admitted = match self.admit(
                &messages,
                &descriptors,
                output_reservation,
                remaining,
                window,
            ) {
                Ok(admitted) => admitted,
                Err(reason) => return reason,
            };
            if admitted.is_err() {
                let prefix = match compaction::prefix_len(&base) {
                    Ok(prefix) => prefix,
                    Err(_) => return EndReason::Limit(LimitKind::ContextBytes),
                };
                let mut projection = Projection::deterministic(
                    &base,
                    self.summary
                        .as_ref()
                        .map_or(prefix, |summary| summary.retained_from),
                );
                projection.summary = self.summary.clone();
                preparation.projection = Some(projection);
                messages = match preparation.apply(&self.messages) {
                    Ok(messages) => messages,
                    Err(_) => return EndReason::Limit(LimitKind::ContextBytes),
                };
                admitted = match self.admit(
                    &messages,
                    &descriptors,
                    output_reservation,
                    remaining,
                    window,
                ) {
                    Ok(admitted) => admitted,
                    Err(reason) => return reason,
                };
            }

            if let Err(limit) = &admitted {
                let limit = limit.clone();
                let summary = match self
                    .compact(
                        &base,
                        &descriptors,
                        Capacity {
                            output: output_reservation,
                            remaining,
                            window,
                        },
                        limit,
                        cancellation,
                        deadline,
                    )
                    .await
                {
                    Ok(summary) => summary,
                    Err(reason) => return reason,
                };
                remaining = self.limits.tokens.saturating_sub(self.budget.tokens());
                output_reservation = output_reservation.min(remaining / 2);
                if output_reservation == 0 {
                    return EndReason::Limit(LimitKind::Tokens);
                }

                let mut projection = preparation.projection.take().unwrap_or_default();
                projection.evicted =
                    Projection::deterministic(&base, summary.retained_from).evicted;
                projection.summary = Some(summary);
                preparation.projection = Some(projection);
                messages = match preparation.apply(&self.messages) {
                    Ok(messages) => messages,
                    Err(_) => return EndReason::Limit(LimitKind::ContextBytes),
                };
                admitted = match self.admit(
                    &messages,
                    &descriptors,
                    output_reservation,
                    remaining,
                    window,
                ) {
                    Ok(admitted) => admitted,
                    Err(reason) => return reason,
                };
            }

            let (input_estimate, output_reservation) = match admitted {
                Ok(value) => value,
                Err(limit) => return EndReason::Limit(limit),
            };
            if let Some(reason) = self.stop(cancellation, deadline) {
                return reason;
            }

            if let Some(projection) = &mut preparation.projection {
                projection.original_bytes =
                    serde_json::to_vec(&base).map_or(0, |bytes| bytes.len());
                projection.projected_bytes =
                    serde_json::to_vec(&messages).map_or(0, |bytes| bytes.len());
                projection.changed_from = projection
                    .evicted
                    .first()
                    .map(|h| h.message)
                    .or_else(|| projection.summary.as_ref().map(|_| 1));
            }

            let composition = ContextComposition {
                history_messages: self.messages.len(),
                request_messages: messages.len(),
                stable_prefix_messages: self
                    .last_request
                    .iter()
                    .zip(&messages)
                    .take_while(|(old, new)| old == new)
                    .count(),
                input_tokens_estimated: input_estimate,
                output_tokens_reserved: output_reservation,
                context_window_tokens: window,
                evicted_outputs: preparation
                    .projection
                    .as_ref()
                    .map_or(0, |p| p.evicted.len()),
                summary_present: preparation
                    .projection
                    .as_ref()
                    .is_some_and(|p| p.summary.is_some()),
                assembly_elapsed_ms: Some(
                    u64::try_from(assembly_start.elapsed().as_millis()).unwrap_or(u64::MAX),
                ),
            };
            if (had_preparation || preparation.projection.is_some())
                && committed_preparation.as_ref() != Some(&preparation)
                && !self.commit(SessionEvent::ContextPrepared { preparation })
            {
                return EndReason::JournalFailed;
            }

            if !self.commit(SessionEvent::ContextComposed { composition }) {
                return EndReason::JournalFailed;
            }

            self.last_request = messages.clone();

            self.budget.model_turns += 1;
            self.budget.reserved_input_tokens = input_estimate;
            self.budget.reserved_output_tokens = output_reservation;

            if !self.commit(SessionEvent::ModelStarted {
                turn: self.budget.model_turns,
            }) {
                return EndReason::JournalFailed;
            }

            let request = ModelRequest {
                messages,
                tools: descriptors,
                max_output_tokens: output_reservation,
            };

            let started = interrupt(self.provider.start(request), cancellation, deadline).await;
            let mut stream_bytes = 0usize;
            let response = match started {
                Err(reason) => {
                    if !self.record_model_failure(false) {
                        return EndReason::JournalFailed;
                    }

                    return reason;
                }
                Ok(Err(error)) => Err(error),
                Ok(Ok(())) => loop {
                    match interrupt(self.provider.next_event(), cancellation, deadline).await {
                        Err(reason) => {
                            if !self.record_model_failure(false) {
                                return EndReason::JournalFailed;
                            }

                            return reason;
                        }
                        Ok(Err(error)) => break Err(error),
                        Ok(Ok(None)) => {
                            if !self.record_model_failure(false) {
                                return EndReason::JournalFailed;
                            }

                            return EndReason::ProviderProtocol;
                        }
                        Ok(Ok(Some(
                            event
                            @ (ProviderEvent::TextDelta(_) | ProviderEvent::ArgumentsDelta(_)),
                        ))) => {
                            let delta = match &event {
                                ProviderEvent::TextDelta(text) => {
                                    self.journal.model_delta(self.budget.model_turns, text);
                                    text
                                }
                                ProviderEvent::ArgumentsDelta(text) => text,
                                _ => unreachable!(),
                            };
                            stream_bytes = stream_bytes.saturating_add(delta.len().max(1));
                            if stream_bytes > self.limits.response_bytes {
                                if !self.record_model_failure(false) {
                                    return EndReason::JournalFailed;
                                }

                                return EndReason::Limit(LimitKind::ResponseBytes);
                            }

                            // Ready buffered fragments must not starve cancellation on this executor.
                            tokio::task::yield_now().await;
                        }
                        Ok(Ok(Some(ProviderEvent::Completed { response, usage }))) => {
                            break Ok((response, usage));
                        }
                    }
                },
            };

            let (response, reported) = match response {
                Ok(value) => value,
                Err(error) => {
                    let retry = error.retryable && self.budget.retries < self.limits.retries;
                    if retry {
                        self.budget.retries += 1;
                    }

                    if !self.record_failure(error.retryable, Some(error.clone())) {
                        return EndReason::JournalFailed;
                    }

                    if !error.retryable {
                        return Self::provider_failure(error.kind);
                    }

                    if !retry {
                        return EndReason::Limit(LimitKind::Retries);
                    }

                    let backoff = (250u64.saturating_mul(1 << self.budget.retries.min(6)))
                        .max(error.retry_after_ms)
                        .min(30_000);
                    if let Err(reason) = interrupt(
                        tokio::time::sleep(Duration::from_millis(backoff)),
                        cancellation,
                        deadline,
                    )
                    .await
                    {
                        return reason;
                    }

                    continue;
                }
            };

            let bytes = match encode(&response, self.limits.response_bytes) {
                Ok(value) => value,
                Err(_) => {
                    if !self.record_model_failure(false) {
                        return EndReason::JournalFailed;
                    }

                    return EndReason::Limit(LimitKind::ResponseBytes);
                }
            };

            let usage = reported.unwrap_or(Usage {
                input_tokens: input_estimate,
                output_tokens: stream_bytes.max(bytes.len()) as u64,
                reported: false,
            });

            self.budget.reserved_input_tokens = 0;
            self.budget.reserved_output_tokens = 0;
            self.budget.charge(&usage);
            self.retryable_provider_failure = false;

            if response.finish == FinishReason::Length
                || (response.calls.is_empty() && response.text.trim().is_empty())
            {
                self.budget.no_progress += 1;
            }

            if !self.commit(SessionEvent::ModelCompleted {
                response: response.clone(),
                usage,
            }) {
                return EndReason::JournalFailed;
            }

            if response.finish == FinishReason::Length {
                tracing::warn!(
                    output_tokens_reserved = output_reservation,
                    "Nano model response reached its output limit; continuing without executing truncated tool calls"
                );
                self.messages.extend(response.messages());
                if self.journal.checkpoint().is_err() {
                    return EndReason::JournalFailed;
                }

                continue;
            }

            if (response.finish == FinishReason::ToolCalls) == response.calls.is_empty() {
                return EndReason::ProviderProtocol;
            }

            self.messages.extend(response.messages());

            if let Some(reason) = self.stop(cancellation, deadline) {
                return reason;
            }

            let mut ids = HashSet::new();
            if response
                .calls
                .iter()
                .any(|call| call.provider_call_id.is_empty() || !ids.insert(&call.provider_call_id))
            {
                return EndReason::ProviderProtocol;
            }

            if response.is_final() {
                if self.journal.checkpoint().is_err() {
                    return EndReason::JournalFailed;
                }

                match self.drain_input() {
                    Ok(true) => continue,
                    Ok(false) => (),
                    Err(reason) => return reason,
                }

                if self.interactive {
                    if let Err(reason) = self.input_capacity("") {
                        return reason;
                    }

                    if !self.commit(SessionEvent::InputRequested) {
                        return EndReason::JournalFailed;
                    }

                    if let Err(reason) = self.wait_for_input(cancellation, deadline).await {
                        return reason;
                    }

                    continue;
                }

                return EndReason::ModelFinished;
            }

            for call in response.calls {
                if let Some(reason) = self.stop(cancellation, deadline) {
                    return reason;
                }

                if self.budget.tool_calls >= self.limits.tool_calls {
                    return EndReason::Limit(LimitKind::ToolCalls);
                }

                self.budget.tool_calls += 1;

                let call_id = format!("call-{}", self.budget.tool_calls);
                let validated = self.validate(&call_id, &call, &names);
                let effect_plan = validated
                    .as_ref()
                    .ok()
                    .and_then(|validated| self.tools.effect_plan(validated));

                if !self.commit(SessionEvent::ToolIntent {
                    call_id: call_id.clone(),
                    call: call.clone(),
                    effect_plan,
                    start_recorded: true,
                }) {
                    return EndReason::JournalFailed;
                }

                let mut interrupted = None;
                let mut outcome = match &validated {
                    Err(error) => ToolOutcome::Failed(error.clone()),
                    Ok(validated) => {
                        match interrupt(self.host.authorize(validated), cancellation, deadline)
                            .await
                        {
                            Err(reason) => {
                                interrupted = Some(reason);
                                ToolOutcome::Failed(ToolError::Interrupted)
                            }
                            Ok(Err(error)) => ToolOutcome::Failed(error),
                            Ok(Ok(())) => {
                                if !self.commit(SessionEvent::ToolStarted {
                                    call_id: call_id.clone(),
                                }) {
                                    return EndReason::JournalFailed;
                                }

                                match interrupt(
                                    self.tools.execute(validated, cancellation),
                                    cancellation,
                                    deadline,
                                )
                                .await
                                {
                                    Ok(outcome) => outcome,
                                    Err(reason) => {
                                        interrupted = Some(reason);
                                        self.tools
                                            .interrupted()
                                            .await
                                            .unwrap_or(ToolOutcome::Unknown(ToolError::Interrupted))
                                    }
                                }
                            }
                        }
                    }
                };

                if encode(&outcome, self.limits.tool_output_bytes).is_err() {
                    outcome = ToolOutcome::Unknown(ToolError::OutputLimit);
                    interrupted = Some(EndReason::Limit(LimitKind::ToolOutputBytes));
                }

                let fingerprint = validated
                    .as_ref()
                    .ok()
                    .map(|call| (call.name.clone(), call.arguments.clone()));

                if !matches!(outcome, ToolOutcome::Success(_))
                    || (fingerprint.is_some() && fingerprint == self.previous_call)
                {
                    self.budget.no_progress += 1;
                } else {
                    self.budget.no_progress = 0;
                }

                self.previous_call = fingerprint;

                if !self.commit(SessionEvent::ToolResult {
                    call_id,
                    outcome: outcome.clone(),
                }) {
                    return EndReason::JournalFailed;
                }

                self.messages.push(Message::Tool {
                    provider_call_id: call.provider_call_id,
                    outcome: outcome.clone(),
                });

                if let Some(reason) = self.tools.end_reason() {
                    return reason;
                }

                if let Some(reason) = interrupted {
                    return reason;
                }

                if matches!(outcome, ToolOutcome::Unknown(_)) {
                    return EndReason::EffectUnknown;
                }
            }

            if self.journal.checkpoint().is_err() {
                return EndReason::JournalFailed;
            }
        }
    }

    fn validate(
        &self,
        call_id: &str,
        call: &ToolCall,
        names: &HashSet<String>,
    ) -> Result<ValidatedCall, ToolError> {
        if !names.contains(&call.name) {
            return Err(ToolError::UnknownTool);
        }

        let arguments: serde_json::Value =
            serde_json::from_str(&call.arguments).map_err(|_| ToolError::InvalidArguments)?;

        if !arguments.is_object() {
            return Err(ToolError::InvalidArguments);
        }

        self.tools.validate(&call.name, &arguments)?;

        Ok(ValidatedCall {
            call_id: call_id.into(),
            provider_call_id: call.provider_call_id.clone(),
            name: call.name.clone(),
            arguments,
        })
    }

    fn record_model_failure(&mut self, retryable: bool) -> bool {
        self.record_failure(retryable, None)
    }

    fn record_failure(&mut self, retryable: bool, error: Option<ProviderError>) -> bool {
        self.retryable_provider_failure = retryable;
        let usage = Usage {
            input_tokens: self.budget.reserved_input_tokens,
            output_tokens: self.budget.reserved_output_tokens,
            reported: false,
        };

        self.budget.reserved_input_tokens = 0;
        self.budget.reserved_output_tokens = 0;
        self.budget.charge(&usage);

        let committed = self.commit(SessionEvent::ModelFailed {
            retryable,
            usage,
            error: error.as_ref().map(|error| error.kind.clone()),
            diagnostic: error.as_ref().and_then(|error| error.diagnostic.clone()),
        });
        if committed && let Some(error) = error {
            tracing::warn!(kind = ?error.kind, retryable, diagnostic = ?error.diagnostic,
                "Nano provider attempt failed");
        }

        committed
    }

    fn stop(&self, cancellation: &Cancellation, deadline: Instant) -> Option<EndReason> {
        if cancellation.is_cancelled() {
            Some(EndReason::Cancelled)
        } else if Instant::now() >= deadline {
            Some(EndReason::Limit(LimitKind::Elapsed))
        } else if self.budget.tokens() >= self.limits.tokens {
            Some(EndReason::Limit(LimitKind::Tokens))
        } else if self.budget.no_progress >= self.limits.no_progress {
            Some(EndReason::Limit(LimitKind::NoProgress))
        } else {
            None
        }
    }

    fn update_elapsed(&mut self) {
        self.budget.elapsed_ms = self.started.map_or(0, |start| {
            start.elapsed().as_millis().min(u64::MAX as u128) as u64
        });
    }

    fn commit(&mut self, event: SessionEvent) -> bool {
        if !matches!(event, SessionEvent::Started { .. }) {
            self.update_elapsed();
        }

        match self.journal.append(event, &self.budget) {
            Ok(record) => {
                self.host.committed(&record);
                true
            }
            Err(_) => false,
        }
    }

    fn journal_failure(&self) -> SessionEnd {
        SessionEnd {
            reason: EndReason::JournalFailed,
            budget: self.budget.clone(),
            durable: false,
            retryable_provider_failure: self.retryable_provider_failure,
        }
    }
}

async fn interrupt<T>(
    future: impl std::future::Future<Output = T>,
    cancellation: &Cancellation,
    deadline: Instant,
) -> Result<T, EndReason> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(EndReason::Cancelled),
        _ = tokio::time::sleep_until(deadline) => Err(EndReason::Limit(LimitKind::Elapsed)),
        output = future => Ok(output),
    }
}

mod capacity;
