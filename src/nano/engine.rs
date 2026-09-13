//! Sequential bounded inference/tool loop. Journal commits precede effects and notifications.

use super::{
    journal::{Journal, encode, valid_id},
    provider::{
        FinishReason, Message, ModelRequest, Provider, ProviderErrorKind, ProviderEvent, Usage,
    },
    session::{
        Budget, EndReason, LimitKind, Limits, SessionCommand, SessionEnd, SessionEvent,
        SessionIdentity,
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
    previous_call: Option<(String, serde_json::Value)>,
    started: Option<Instant>,
}

impl<P: Provider, T: Tools, H: Host, J: Journal> Engine<P, T, H, J> {
    pub(crate) fn new(
        identity: SessionIdentity,
        limits: Limits,
        provider: P,
        tools: T,
        host: H,
        journal: J,
    ) -> Result<Self> {
        limits.validate()?;
        ensure!(
            valid_id(&identity.session_id)
                && !identity.project_id.is_empty()
                && identity.task_id.is_some() == identity.run_id.is_some(),
            "Invalid session identity"
        );

        Ok(Self {
            identity,
            limits,
            provider,
            tools,
            host,
            journal,
            budget: Budget::default(),
            messages: Vec::new(),
            previous_call: None,
            started: None,
        })
    }

    /// One attempt per engine. Live resume and interactive steering are separate adapters.
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
        };

        if input.len() > self.limits.context_bytes {
            // An oversized input is never copied into the journal.
            return Ok(SessionEnd {
                reason: EndReason::Limit(LimitKind::ContextBytes),
                budget: self.budget.clone(),
                durable: false,
            });
        }

        if !self.commit(SessionEvent::Started {
            identity: self.identity.clone(),
            limits: self.limits.clone(),
            input: input.clone(),
            provider: self.provider.settings().map(Box::new),
        }) {
            return Ok(self.journal_failure());
        }

        self.messages.push(Message::User { text: input });

        let mut reason = self.drive(cancellation, deadline).await;

        self.provider.cancel();

        if !self.tools.shutdown().await && reason != EndReason::JournalFailed {
            reason = EndReason::EffectUnknown;
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
        })
    }

    async fn drive(&mut self, cancellation: &Cancellation, deadline: Instant) -> EndReason {
        loop {
            if let Some(reason) = self.stop(cancellation, deadline) {
                return reason;
            }
            if self.budget.model_turns >= self.limits.model_turns {
                return EndReason::Limit(LimitKind::ModelTurns);
            }

            let descriptors = self.tools.descriptors();
            let mut names = HashSet::new();
            if descriptors
                .iter()
                .any(|tool| tool.name.is_empty() || !names.insert(tool.name.clone()))
            {
                return EndReason::ProviderProtocol;
            }

            let context = match encode(&(&self.messages, &descriptors), self.limits.context_bytes) {
                Ok(bytes) => bytes,
                Err(_) => return EndReason::Limit(LimitKind::ContextBytes),
            };

            // One byte per token is a conservative local estimate, not billing usage.
            let input_estimate = context.len() as u64;
            let remaining = self.limits.tokens.saturating_sub(self.budget.tokens());
            if input_estimate >= remaining {
                return EndReason::Limit(LimitKind::Tokens);
            }

            let output_reservation = (remaining - input_estimate)
                .min(self.limits.response_bytes as u64)
                .min(
                    self.provider
                        .settings()
                        .map_or(u64::MAX, |settings| settings.max_output_tokens),
                );

            self.budget.model_turns += 1;
            self.budget.reserved_input_tokens = input_estimate;
            self.budget.reserved_output_tokens = output_reservation;

            if !self.commit(SessionEvent::ModelStarted {
                turn: self.budget.model_turns,
            }) {
                return EndReason::JournalFailed;
            }

            let request = ModelRequest {
                messages: self.messages.clone(),
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
                            ProviderEvent::TextDelta(delta) | ProviderEvent::ArgumentsDelta(delta),
                        ))) => {
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
                    if !self.record_failure(error.retryable, Some(error.kind.clone())) {
                        return EndReason::JournalFailed;
                    }
                    if !error.retryable {
                        return match error.kind {
                            ProviderErrorKind::ContextOverflow => {
                                EndReason::Limit(LimitKind::ContextTokens)
                            }
                            ProviderErrorKind::ResponseLimit => {
                                EndReason::Limit(LimitKind::ResponseBytes)
                            }
                            ProviderErrorKind::Protocol | ProviderErrorKind::Unsupported => {
                                EndReason::ProviderProtocol
                            }
                            _ => EndReason::ProviderFailed,
                        };
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

            if response.calls.is_empty() && response.text.trim().is_empty() {
                self.budget.no_progress += 1;
            }
            if !self.commit(SessionEvent::ModelCompleted {
                response: response.clone(),
                usage,
            }) {
                return EndReason::JournalFailed;
            }

            if response.finish == FinishReason::Length {
                return EndReason::ProviderTruncated;
            }
            if (response.finish == FinishReason::ToolCalls) == response.calls.is_empty() {
                return EndReason::ProviderProtocol;
            }

            self.messages.push(Message::Assistant {
                response: response.clone(),
            });

            if let Some(reason) = self.stop(cancellation, deadline) {
                return reason;
            }
            if encode(&self.messages, self.limits.context_bytes).is_err() {
                return EndReason::Limit(LimitKind::ContextBytes);
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

                if !self.commit(SessionEvent::ToolIntent {
                    call_id: call_id.clone(),
                    call: call.clone(),
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
                            Ok(Ok(())) => match interrupt(
                                self.tools.execute(validated, cancellation),
                                cancellation,
                                deadline,
                            )
                            .await
                            {
                                Ok(outcome) => outcome,
                                Err(reason) => {
                                    interrupted = Some(reason);
                                    ToolOutcome::Unknown(ToolError::Interrupted)
                                }
                            },
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

                if let Some(reason) = interrupted {
                    return reason;
                }

                if matches!(outcome, ToolOutcome::Unknown(_)) {
                    return EndReason::EffectUnknown;
                }

                if encode(&self.messages, self.limits.context_bytes).is_err() {
                    return EndReason::Limit(LimitKind::ContextBytes);
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

    fn record_failure(&mut self, retryable: bool, error: Option<ProviderErrorKind>) -> bool {
        let usage = Usage {
            input_tokens: self.budget.reserved_input_tokens,
            output_tokens: self.budget.reserved_output_tokens,
            reported: false,
        };

        self.budget.reserved_input_tokens = 0;
        self.budget.reserved_output_tokens = 0;
        self.budget.charge(&usage);

        self.commit(SessionEvent::ModelFailed {
            retryable,
            usage,
            error,
        })
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
