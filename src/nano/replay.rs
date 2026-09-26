//! Pure journal replay. It has no provider, tool, filesystem, or Ferrus effect ports.

use super::{
    provider::Message,
    session::{Budget, EndReason, Record, SessionEvent},
    tools::{ToolCall, ToolOutcome},
};
use anyhow::{Result, ensure};
use std::collections::VecDeque;

pub(crate) const JOURNAL_VERSION: u32 = 1;

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Replay {
    pub sequence: u64,
    pub session_id: String,
    pub budget: Budget,
    pub messages: Vec<Message>,
    pub projection: Option<super::working_set::Preparation>,
    pub summary: Option<super::compaction::Summary>,
    pub end: Option<EndReason>,
    pub pending_effect: Option<String>,
    pending_requires_start: bool,
    effect_started: bool,
    pub unknown_effects: Vec<String>,
    model_active: bool,
    compaction_active: bool,
    compaction_cut: Option<usize>,
    final_response_ready: bool,
    submitted: bool,
    calls: VecDeque<ToolCall>,
}

impl Replay {
    pub(crate) fn from_records(records: &[Record]) -> Result<Self> {
        let mut state = Self::default();
        for record in records {
            state.apply(record)?;
        }

        Ok(state)
    }

    pub(crate) fn checkpoint_ready(&self) -> bool {
        self.sequence > 0
            && !self.model_active
            && self.pending_effect.is_none()
            && self.calls.is_empty()
    }

    pub(crate) fn apply(&mut self, record: &Record) -> Result<()> {
        ensure!(
            record.version == JOURNAL_VERSION,
            "Unsupported nano journal version"
        );

        ensure!(
            record.sequence == self.sequence + 1,
            "Non-contiguous journal sequence"
        );

        ensure!(self.end.is_none(), "Event after session end");

        if self.sequence == 0 {
            ensure!(
                matches!(&record.event, SessionEvent::Started { identity, .. } if identity.session_id == record.session_id),
                "Journal must start with its session identity"
            );
            self.session_id = record.session_id.clone();
        }

        ensure!(
            self.session_id == record.session_id,
            "Journal session mismatch"
        );

        let before = &self.budget;
        let after = &record.budget;

        ensure!(
            after.model_turns >= before.model_turns
                && after.tool_calls >= before.tool_calls
                && after.retries >= before.retries
                && after.elapsed_ms >= before.elapsed_ms
                && after.reported_input_tokens >= before.reported_input_tokens
                && after.reported_output_tokens >= before.reported_output_tokens
                && after.estimated_input_tokens >= before.estimated_input_tokens
                && after.estimated_output_tokens >= before.estimated_output_tokens,
            "Budget consumption regressed"
        );

        let mut expected = before.clone();
        expected.elapsed_ms = after.elapsed_ms;
        expected.no_progress = after.no_progress;

        match &record.event {
            SessionEvent::Started {
                inherited_budget: Some(inherited),
                ..
            } => {
                ensure!(
                    before == &Budget::default()
                        && inherited.elapsed_ms == 0
                        && inherited.reserved_input_tokens == 0
                        && inherited.reserved_output_tokens == 0,
                    "Invalid inherited session budget"
                );
                expected = inherited.clone();
            }
            SessionEvent::ModelStarted { .. } | SessionEvent::CompactionStarted { .. } => {
                ensure!(
                    before.reserved_input_tokens == 0
                        && before.reserved_output_tokens == 0
                        && after.reserved_input_tokens > 0
                        && after.reserved_output_tokens > 0,
                    "Invalid provider usage reservation"
                );
                expected.model_turns += 1;
                expected.reserved_input_tokens = after.reserved_input_tokens;
                expected.reserved_output_tokens = after.reserved_output_tokens;
            }
            SessionEvent::ModelCompleted { usage, .. }
            | SessionEvent::CompactionCompleted { usage, .. }
            | SessionEvent::ModelFailed { usage, .. } => {
                expected.reserved_input_tokens = 0;
                expected.reserved_output_tokens = 0;
                expected.charge(usage);
                if let SessionEvent::ModelFailed { retryable, .. } = &record.event {
                    ensure!(
                        after.retries == before.retries
                            || (*retryable && after.retries == before.retries + 1),
                        "Invalid retry charge"
                    );
                    expected.retries = after.retries;
                }
            }
            SessionEvent::ToolIntent { .. } => expected.tool_calls += 1,
            _ => (),
        }

        ensure!(
            &expected == after,
            "Budget does not match recorded consumption"
        );

        match &record.event {
            SessionEvent::Started {
                limits,
                input,
                inherited_budget,
                ..
            } => {
                ensure!(
                    self.sequence == 0
                        && inherited_budget
                            .as_ref()
                            .map_or(after == &Budget::default(), |inherited| after == inherited),
                    "Duplicate or charged session start"
                );
                limits.validate()?;
                self.messages.push(Message::User {
                    text: input.clone(),
                });
            }
            SessionEvent::ContextPrepared { preparation } => {
                ensure!(
                    self.sequence > 0 && self.checkpoint_ready(),
                    "Context prepared inside an unfinished group"
                );
                preparation.apply(&self.messages)?;
                ensure!(
                    preparation
                        .projection
                        .as_ref()
                        .and_then(|value| value.summary.as_ref())
                        == self.summary.as_ref(),
                    "Context projection does not match the compacted history"
                );
                self.projection = Some(preparation.clone());
            }
            SessionEvent::ContextComposed { composition } => {
                ensure!(
                    self.checkpoint_ready()
                        && composition.history_messages == self.messages.len()
                        && composition.request_messages > 0
                        && composition.stable_prefix_messages <= composition.request_messages
                        && composition.input_tokens_estimated > 0
                        && composition.output_tokens_reserved > 0
                        && composition
                            .input_tokens_estimated
                            .saturating_add(composition.output_tokens_reserved)
                            <= composition.context_window_tokens,
                    "Invalid context composition"
                );
            }
            SessionEvent::ModelStarted { turn } => {
                ensure!(
                    self.checkpoint_ready() && self.end.is_none(),
                    "Model started inside an unfinished group"
                );
                ensure!(
                    *turn == before.model_turns + 1 && after.model_turns == *turn,
                    "Invalid model turn"
                );
                self.model_active = true;
                self.final_response_ready = false;
            }
            SessionEvent::CompactionStarted {
                turn,
                retained_from,
            } => {
                ensure!(
                    self.checkpoint_ready(),
                    "Compaction inside an unfinished group"
                );
                ensure!(
                    *turn == before.model_turns + 1
                        && after.model_turns == *turn
                        && super::compaction::boundaries(&self.messages)?.contains(retained_from)
                        && *retained_from > 1,
                    "Invalid compaction boundary"
                );
                self.model_active = true;
                self.compaction_active = true;
                self.compaction_cut = Some(*retained_from);
                self.final_response_ready = false;
            }
            SessionEvent::ModelCompleted { response, .. } => {
                ensure!(
                    self.model_active && !self.compaction_active && self.calls.is_empty(),
                    "Unexpected model response"
                );
                self.model_active = false;
                self.final_response_ready = response.is_final();
                self.calls = response.calls.clone().into();
                self.messages.push(Message::Assistant {
                    response: response.clone(),
                });
            }
            SessionEvent::CompactionCompleted { summary, .. } => {
                ensure!(
                    self.model_active
                        && self.compaction_active
                        && self.compaction_cut == Some(summary.retained_from)
                        && self
                            .summary
                            .as_ref()
                            .is_none_or(|old| summary.retained_from > old.retained_from)
                        && summary.handles.len() <= 16,
                    "Unexpected compaction result"
                );
                ensure!(
                    summary
                        .handles
                        .iter()
                        .all(|handle| super::compaction::handles(&self.messages).contains(handle)),
                    "Compaction handle does not match the journal"
                );
                super::compaction::Projection {
                    summary: Some(summary.clone()),
                    ..Default::default()
                }
                .apply(&self.messages)?;
                self.model_active = false;
                self.compaction_active = false;
                self.compaction_cut = None;
                self.summary = Some(summary.clone());
            }
            SessionEvent::ModelFailed { .. } => {
                ensure!(self.model_active, "Unexpected provider failure");
                self.model_active = false;
                self.compaction_active = false;
                self.compaction_cut = None;
                self.final_response_ready = false;
            }
            SessionEvent::ToolIntent {
                call_id,
                call,
                effect_plan,
                start_recorded,
            } => {
                ensure!(
                    self.pending_effect.is_none() && self.calls.front() == Some(call),
                    "Tool intent out of model order"
                );
                ensure!(
                    after.tool_calls == before.tool_calls + 1
                        && *call_id == format!("call-{}", after.tool_calls),
                    "Invalid tool call ID"
                );
                if let Some(super::tools::EffectPlan::Patch { files }) = effect_plan {
                    ensure!(
                        call.name == "apply_patch"
                            && !files.is_empty()
                            && files.len() <= 16
                            && files.iter().all(|file| {
                                !file.path.is_empty()
                                    && file.before_digest.as_deref().is_none_or(valid_digest)
                                    && file.after_digest.as_deref().is_none_or(valid_digest)
                            }),
                        "Invalid patch effect plan"
                    );
                }
                self.pending_effect = Some(call_id.clone());
                self.pending_requires_start = *start_recorded;
                self.effect_started = false;
                self.submitted = false;
            }
            SessionEvent::ToolStarted { call_id } => {
                ensure!(
                    self.pending_effect.as_ref() == Some(call_id)
                        && self.pending_requires_start
                        && !self.effect_started,
                    "Tool start has no matching unstarted intent"
                );
                self.effect_started = true;
            }
            SessionEvent::ToolResult { call_id, outcome } => {
                ensure!(
                    self.pending_effect.as_ref() == Some(call_id),
                    "Tool result has no matching intent"
                );
                ensure!(
                    !self.pending_requires_start
                        || self.effect_started
                        || matches!(outcome, ToolOutcome::Failed(_)),
                    "Unstarted tool cannot succeed or have an unknown effect"
                );
                let call = self.calls.pop_front().expect("intent checked call queue");
                self.submitted = call.name == "submit"
                    && matches!(outcome, ToolOutcome::Success(value) if value["status"] == "submitted" && value["task_state"] == "reviewing");
                self.messages.push(Message::Tool {
                    provider_call_id: call.provider_call_id,
                    outcome: outcome.clone(),
                });
                if matches!(outcome, ToolOutcome::Unknown(_)) {
                    self.unknown_effects.push(call_id.clone());
                }
                self.pending_effect = None;
                self.pending_requires_start = false;
                self.effect_started = false;
            }
            SessionEvent::Ended { reason } => {
                if *reason == EndReason::Submitted {
                    ensure!(
                        self.submitted && self.pending_effect.is_none() && !self.model_active,
                        "Submit finish requires a confirmed native handoff"
                    );
                }
                if *reason == EndReason::ModelFinished {
                    ensure!(
                        self.checkpoint_ready(),
                        "Model finish inside an unfinished group"
                    );
                    ensure!(
                        self.final_response_ready,
                        "Model finish requires a completed final response"
                    );
                }
                self.end = Some(reason.clone());
            }
        }

        self.budget = record.budget.clone();
        self.sequence = record.sequence;

        Ok(())
    }
}
