//! Provider boundary. Adapters assemble complete responses; partial arguments are display-only.

use super::tools::{ToolCall, ToolDescriptor, ToolOutcome};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,

    /// False is a host estimate, never provider-reported billing data.
    pub reported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FinishReason {
    Stop,
    ToolCalls,
    Length,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelResponse {
    pub finish: FinishReason,
    pub text: String,
    pub calls: Vec<ToolCall>,

    /// Adapter-owned continuation data (including signed blocks), preserved without interpretation.
    pub continuation: Option<serde_json::Value>,
}

impl ModelResponse {
    pub(crate) fn is_final(&self) -> bool {
        self.finish == FinishReason::Stop && self.calls.is_empty() && !self.text.trim().is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub(crate) enum Message {
    User {
        text: String,
    },
    Assistant {
        response: ModelResponse,
    },
    Tool {
        provider_call_id: String,
        outcome: ToolOutcome,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ModelRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDescriptor>,
    pub max_output_tokens: u64,
}

#[derive(Debug, Clone)]
pub(crate) enum ProviderEvent {
    TextDelta(String),
    ArgumentsDelta(String),
    Completed {
        response: ModelResponse,
        usage: Option<Usage>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ProviderError {
    pub retryable: bool,
    pub kind: ProviderErrorKind,
    pub retry_after_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderErrorKind {
    Transport,
    Timeout,
    RateLimited,
    Authentication,
    ContextOverflow,
    TruncatedStream,
    Protocol,
    Unsupported,
    ResponseLimit,
}

impl ProviderError {
    pub(crate) fn new(kind: ProviderErrorKind, retryable: bool) -> Self {
        Self {
            kind,
            retryable,
            retry_after_ms: 0,
        }
    }
}

/// Only validated, non-secret effective settings may enter the journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderSettings {
    pub api: String,
    pub base_url: String,
    pub model: String,
    pub context_tokens: u64,
    pub max_output_tokens: u64,
    pub temperature: f64,
    pub request_timeout_ms: u64,
    pub wire_bytes: usize,
    pub event_bytes: usize,
    pub max_tool_calls: usize,
    pub include_usage: bool,
}

pub(crate) trait Provider {
    /// When present, the output cap also bounds the engine's per-attempt reservation.
    fn settings(&self) -> Option<ProviderSettings> {
        None
    }

    /// Conservative provider-wire input estimate, including tool schemas and framing.
    /// Reported usage remains separate from this admission estimate.
    fn estimate_input_tokens(&self, request: &ModelRequest) -> Result<u64, ProviderError> {
        let bytes = serde_json::to_vec(&(&request.messages, &request.tools))
            .map_err(|_| ProviderError::new(ProviderErrorKind::Protocol, false))?;
        Ok((bytes.len() as u64).saturating_add(128))
    }

    /// Close any retained stream, including cancellation between buffered events.
    fn cancel(&mut self) {}

    /// Both start and next_event must be safe to drop on cancellation/deadline.
    async fn start(&mut self, request: ModelRequest) -> Result<(), ProviderError>;

    async fn next_event(&mut self) -> Result<Option<ProviderEvent>, ProviderError>;
}
