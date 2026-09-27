//! Transport-neutral session protocol and persisted resource accounting.

use serde::{Deserialize, Serialize};

use super::provider::{ModelResponse, ProviderErrorKind, ProviderSettings, Usage};
use super::tools::{EffectPlan, ToolCall, ToolOutcome};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionIdentity {
    pub session_id: String,
    pub project_id: String,
    pub task_id: Option<String>,
    pub run_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum SessionCommand {
    Start { input: String },
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Limits {
    pub model_turns: u64,
    pub tokens: u64,
    pub tool_calls: u64,
    pub retries: u64,
    pub no_progress: u64,
    pub elapsed_ms: u64,
    pub context_bytes: usize,
    pub response_bytes: usize,
    pub tool_output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            model_turns: 64,
            tokens: 200_000,
            tool_calls: 256,
            retries: 3,
            no_progress: 3,
            elapsed_ms: 30 * 60 * 1000,
            context_bytes: 256 * 1024,
            response_bytes: 64 * 1024,
            tool_output_bytes: 32 * 1024,
        }
    }
}

impl Limits {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.model_turns > 0
                && self.tokens > 0
                && self.tool_calls > 0
                && self.no_progress > 0
                && self.elapsed_ms > 0
                && self.context_bytes > 0
                && self.response_bytes > 0
                && self.tool_output_bytes > 0,
            "Session limits must be positive (retries may be zero)"
        );

        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Budget {
    pub model_turns: u64,
    pub tool_calls: u64,
    pub retries: u64,
    pub no_progress: u64,
    pub elapsed_ms: u64,
    pub reported_input_tokens: u64,
    pub reported_output_tokens: u64,
    pub estimated_input_tokens: u64,
    pub estimated_output_tokens: u64,
    pub reserved_input_tokens: u64,
    pub reserved_output_tokens: u64,
}

impl Budget {
    pub(crate) fn tokens(&self) -> u64 {
        self.reported_input_tokens
            .saturating_add(self.reported_output_tokens)
            .saturating_add(self.estimated_input_tokens)
            .saturating_add(self.estimated_output_tokens)
            .saturating_add(self.reserved_input_tokens)
            .saturating_add(self.reserved_output_tokens)
    }

    pub(crate) fn charge(&mut self, usage: &Usage) {
        let (input, output) = if usage.reported {
            (
                &mut self.reported_input_tokens,
                &mut self.reported_output_tokens,
            )
        } else {
            (
                &mut self.estimated_input_tokens,
                &mut self.estimated_output_tokens,
            )
        };

        *input = input.saturating_add(usage.input_tokens);
        *output = output.saturating_add(usage.output_tokens);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LimitKind {
    ModelTurns,
    Tokens,
    ToolCalls,
    Retries,
    NoProgress,
    Elapsed,
    ContextBytes,
    ContextTokens,
    ResponseBytes,
    ToolOutputBytes,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", content = "detail", rename_all = "snake_case")]
pub(crate) enum EndReason {
    ModelFinished,
    Submitted,
    TaskFailed,
    Paused,
    AuthorityLost,
    Cancelled,
    Limit(LimitKind),
    ProviderFailed,
    ProviderProtocol,
    ProviderTruncated,
    JournalFailed,
    EffectUnknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum SessionEvent {
    Started {
        identity: SessionIdentity,
        limits: Limits,
        input: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inherited_budget: Option<Budget>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<Box<ProviderSettings>>,
    },
    ModelStarted {
        turn: u64,
    },
    CompactionStarted {
        turn: u64,
        retained_from: usize,
    },
    CompactionCompleted {
        summary: super::compaction::Summary,
        usage: Usage,
    },
    ContextPrepared {
        preparation: super::working_set::Preparation,
    },
    ContextComposed {
        composition: ContextComposition,
    },
    ModelCompleted {
        response: ModelResponse,
        usage: Usage,
    },
    ModelFailed {
        retryable: bool,
        usage: Usage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ProviderErrorKind>,
    },
    ToolIntent {
        call_id: String,
        call: ToolCall,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effect_plan: Option<EffectPlan>,
        /// Legacy records had no separate execution boundary.
        #[serde(default, skip_serializing_if = "is_false")]
        start_recorded: bool,
    },
    ToolStarted {
        call_id: String,
    },
    ToolResult {
        call_id: String,
        outcome: ToolOutcome,
    },
    Ended {
        reason: EndReason,
    },
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContextComposition {
    pub history_messages: usize,
    pub request_messages: usize,
    pub stable_prefix_messages: usize,
    pub input_tokens_estimated: u64,
    pub output_tokens_reserved: u64,
    pub context_window_tokens: u64,
    pub evicted_outputs: usize,
    pub summary_present: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    pub version: u32,
    pub session_id: String,
    pub sequence: u64,
    pub budget: Budget,
    pub event: SessionEvent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionEnd {
    pub reason: EndReason,
    pub budget: Budget,
    /// False means the final state could not be durably recorded. Never acknowledge it as committed.
    pub durable: bool,
}
