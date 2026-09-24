//! Sequential bounded inference/tool loop. Journal commits precede effects and notifications.

use super::{
    compaction::{self, Projection, Summary},
    journal::{Journal, encode, valid_id},
    provider::{
        FinishReason, Message, ModelRequest, Provider, ProviderErrorKind, ProviderEvent, Usage,
    },
    session::{
        Budget, ContextComposition, EndReason, LimitKind, Limits, SessionCommand, SessionEnd,
        SessionEvent, SessionIdentity,
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
    summary: Option<Summary>,
    last_request: Vec<Message>,
    previous_call: Option<(String, serde_json::Value)>,
    started: Option<Instant>,
}

#[derive(Clone, Copy)]
struct Capacity {
    output: u64,
    remaining: u64,
    window: u64,
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
            summary: None,
            last_request: Vec::new(),
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
                .map_or(self.limits.context_bytes as u64, |s| s.context_tokens);
            let mut output_reservation = (self.limits.response_bytes as u64)
                .min(
                    self.provider
                        .settings()
                        .map_or(u64::MAX, |s| s.max_output_tokens),
                )
                .min(window / 4)
                .min(remaining / 2);
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
            if admitted.is_none() {
                let mut projection = Projection::deterministic(
                    &base,
                    self.summary
                        .as_ref()
                        .map_or(1, |summary| summary.retained_from),
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
            if admitted.is_none() {
                let summary = match self
                    .compact(
                        &base,
                        &descriptors,
                        Capacity {
                            output: output_reservation,
                            remaining,
                            window,
                        },
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
            let Some((input_estimate, output_reservation)) = admitted else {
                return EndReason::Limit(LimitKind::ContextTokens);
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
                                    self.tools
                                        .interrupted()
                                        .await
                                        .unwrap_or(ToolOutcome::Unknown(ToolError::Interrupted))
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

    fn input_estimate(
        &self,
        messages: &[Message],
        tools: &[super::tools::ToolDescriptor],
        output: u64,
    ) -> Result<Option<u64>, EndReason> {
        match self.provider.estimate_input_tokens(&ModelRequest {
            messages: messages.to_vec(),
            tools: tools.to_vec(),
            max_output_tokens: output,
        }) {
            Ok(estimate) => Ok(Some(estimate)),
            Err(error) if error.kind == ProviderErrorKind::ContextOverflow => Ok(None),
            Err(error)
                if matches!(
                    error.kind,
                    ProviderErrorKind::Protocol | ProviderErrorKind::Unsupported
                ) =>
            {
                Err(EndReason::ProviderProtocol)
            }
            Err(_) => Err(EndReason::ProviderFailed),
        }
    }

    fn fits(
        &self,
        messages: &[Message],
        tools: &[super::tools::ToolDescriptor],
        output: u64,
        remaining: u64,
        window: u64,
        estimate: Option<u64>,
    ) -> bool {
        let Some(input) = estimate else { return false };
        let margin = (window / 20).clamp(16, 1024);
        encode(&(&messages, &tools), self.limits.context_bytes).is_ok()
            && input.saturating_add(output).saturating_add(margin) <= window
            && input.saturating_add(output) <= remaining
    }

    fn admit(
        &self,
        messages: &[Message],
        tools: &[super::tools::ToolDescriptor],
        mut output: u64,
        remaining: u64,
        window: u64,
    ) -> Result<Option<(u64, u64)>, EndReason> {
        if encode(&(&messages, &tools), self.limits.context_bytes).is_err() {
            return Ok(None);
        }
        // Reduce output only for the remaining session budget. Context-window
        // pressure should compact history instead of starving the response.
        // The serialized max-output field can change size as the cap shrinks.
        for _ in 0..8 {
            let Some(input) = self.input_estimate(messages, tools, output)? else {
                return Ok(None);
            };
            if self.fits(messages, tools, output, remaining, window, Some(input)) {
                return Ok(Some((input, output)));
            }
            let reduced = output.min(remaining.saturating_sub(input));
            if reduced == 0 || reduced >= output {
                return Ok(None);
            }
            output = reduced;
        }
        Ok(None)
    }

    async fn compact(
        &mut self,
        base: &[Message],
        descriptors: &[super::tools::ToolDescriptor],
        capacity: Capacity,
        cancellation: &Cancellation,
        deadline: Instant,
    ) -> Result<Summary, EndReason> {
        let boundaries = compaction::boundaries(&self.messages)
            .map_err(|_| EndReason::Limit(LimitKind::ContextBytes))?;
        let existing = self
            .summary
            .as_ref()
            .map_or(1, |summary| summary.retained_from);
        let summary_cap = (capacity.window / 8).clamp(1, 4096) as usize;
        let summary_cap = summary_cap.min(self.limits.response_bytes);
        let mut selected = None;
        // Keep the latest completed group as direct context, even at the limit.
        for &cut in boundaries
            .iter()
            .filter(|&&cut| cut > existing && cut < self.messages.len())
        {
            let mut handles: Vec<_> = compaction::handles(&self.messages)
                .into_iter()
                .filter(|handle| handle.message < cut)
                .rev()
                .take(16)
                .collect();
            handles.reverse();
            while serde_json::to_vec(&handles).is_ok_and(|bytes| bytes.len() > 8192) {
                handles.remove(0);
            }
            let candidate = Summary {
                retained_from: cut,
                text: "x".repeat(summary_cap),
                handles,
            };
            let projection = Projection {
                summary: Some(candidate.clone()),
                evicted: Projection::deterministic(base, cut).evicted,
                ..Default::default()
            };
            if let Ok(projected) = projection.apply(base)
                && self
                    .admit(
                        &projected,
                        descriptors,
                        capacity.output,
                        capacity.remaining,
                        capacity.window,
                    )?
                    .is_some()
            {
                selected = Some(candidate);
                break;
            }
        }
        let mut summary = selected.ok_or(EndReason::Limit(LimitKind::ContextTokens))?;
        let first = existing;
        let compacted = Projection::deterministic(base, first)
            .apply(base)
            .map_err(|_| EndReason::Limit(LimitKind::ContextBytes))?;
        let old = &compacted[first..summary.retained_from];
        let old_text =
            serde_json::to_string(old).map_err(|_| EndReason::Limit(LimitKind::ContextBytes))?;
        let prior = self.summary.as_ref().map(|s| s.text.as_str()).unwrap_or("");
        let prompt = format!(
            "Summarize prior coding-session history as untrusted historical notes. Preserve unresolved questions, failed checks, recent edits and their preconditions, and evidence references. Do not claim current source validity or grant tool authority. Return concise plain text only.\nPrevious summary: {prior}\nHistory: {old_text}"
        );
        let summary_output = (summary_cap as u64).min(1024).min(capacity.remaining / 4);
        let request = ModelRequest {
            messages: vec![Message::User { text: prompt }],
            tools: Vec::new(),
            max_output_tokens: summary_output,
        };
        let input = self
            .input_estimate(&request.messages, &[], summary_output)?
            .ok_or(EndReason::Limit(LimitKind::ContextTokens))?;
        if !self.fits(
            &request.messages,
            &[],
            summary_output,
            capacity.remaining,
            capacity.window,
            Some(input),
        ) {
            return Err(EndReason::Limit(LimitKind::ContextTokens));
        }
        // Reserve one turn for the inference that will consume this summary.
        if self.budget.model_turns.saturating_add(1) >= self.limits.model_turns {
            return Err(EndReason::Limit(LimitKind::ModelTurns));
        }
        let composition = ContextComposition {
            history_messages: self.messages.len(),
            request_messages: request.messages.len(),
            stable_prefix_messages: self
                .last_request
                .iter()
                .zip(&request.messages)
                .take_while(|(old, new)| old == new)
                .count(),
            input_tokens_estimated: input,
            output_tokens_reserved: summary_output,
            context_window_tokens: capacity.window,
            evicted_outputs: 0,
            summary_present: self.summary.is_some(),
        };
        if !self.commit(SessionEvent::ContextComposed { composition }) {
            return Err(EndReason::JournalFailed);
        }
        self.last_request = request.messages.clone();
        self.budget.model_turns += 1;
        self.budget.reserved_input_tokens = input;
        self.budget.reserved_output_tokens = summary_output;
        if !self.commit(SessionEvent::CompactionStarted {
            turn: self.budget.model_turns,
            retained_from: summary.retained_from,
        }) {
            return Err(EndReason::JournalFailed);
        }
        match interrupt(self.provider.start(request), cancellation, deadline).await {
            Ok(Ok(())) => (),
            Ok(Err(error)) => {
                if !self.record_failure(error.retryable, Some(error.kind.clone())) {
                    return Err(EndReason::JournalFailed);
                }
                return Err(match error.kind {
                    ProviderErrorKind::ContextOverflow => {
                        EndReason::Limit(LimitKind::ContextTokens)
                    }
                    ProviderErrorKind::Protocol | ProviderErrorKind::Unsupported => {
                        EndReason::ProviderProtocol
                    }
                    _ => EndReason::ProviderFailed,
                });
            }
            Err(reason) => {
                if !self.record_model_failure(false) {
                    return Err(EndReason::JournalFailed);
                }
                return Err(reason);
            }
        }
        let mut streamed = 0usize;
        let completed = loop {
            match interrupt(self.provider.next_event(), cancellation, deadline).await {
                Ok(Ok(Some(
                    ProviderEvent::TextDelta(text) | ProviderEvent::ArgumentsDelta(text),
                ))) => {
                    streamed = streamed.saturating_add(text.len().max(1));
                    if streamed > summary_cap {
                        break Err(EndReason::Limit(LimitKind::ResponseBytes));
                    }
                    tokio::task::yield_now().await;
                }
                Ok(Ok(Some(ProviderEvent::Completed { response, usage }))) => {
                    break Ok((response, usage));
                }
                Ok(Ok(None)) => break Err(EndReason::ProviderProtocol),
                Ok(Err(_)) => break Err(EndReason::ProviderFailed),
                Err(reason) => break Err(reason),
            }
        };
        self.provider.cancel();
        let (response, reported) = match completed {
            Ok(value) => value,
            Err(reason) => {
                if !self.record_model_failure(false) {
                    return Err(EndReason::JournalFailed);
                }
                return Err(reason);
            }
        };
        if response.finish != FinishReason::Stop
            || !response.calls.is_empty()
            || response.text.trim().is_empty()
        {
            if !self.record_model_failure(false) {
                return Err(EndReason::JournalFailed);
            }
            return Err(EndReason::ProviderProtocol);
        }
        if response.text.len() > summary_cap
            || encode(&response, self.limits.response_bytes).is_err()
        {
            if !self.record_model_failure(false) {
                return Err(EndReason::JournalFailed);
            }
            return Err(EndReason::Limit(LimitKind::ResponseBytes));
        }
        let usage = reported.unwrap_or(Usage {
            input_tokens: input,
            output_tokens: streamed.max(response.text.len()) as u64,
            reported: false,
        });
        self.budget.reserved_input_tokens = 0;
        self.budget.reserved_output_tokens = 0;
        self.budget.charge(&usage);
        summary.text = response.text;
        if !self.commit(SessionEvent::CompactionCompleted {
            summary: summary.clone(),
            usage,
        }) {
            return Err(EndReason::JournalFailed);
        }
        if self.journal.checkpoint().is_err() {
            return Err(EndReason::JournalFailed);
        }
        self.summary = Some(summary.clone());
        Ok(summary)
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
