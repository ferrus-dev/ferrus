//! Coding-tool and host boundaries. No runtime identity comes from model arguments.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::watch;

use super::session::Record;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolCall {
    pub provider_call_id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug)]
pub(crate) struct ValidatedCall {
    pub call_id: String,
    pub provider_call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolError {
    UnknownTool,
    InvalidArguments,
    Denied,
    Failed,
    Interrupted,
    OutputLimit,
    /// Bounded native file-tool diagnostics, including per-path edit outcomes.
    Workspace(serde_json::Value),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "content", rename_all = "snake_case")]
pub(crate) enum ToolOutcome {
    Success(serde_json::Value),
    Failed(ToolError),
    /// The effect may have happened. Never automatically re-execute this call.
    Unknown(ToolError),
}

#[derive(Clone, Debug)]
pub(crate) struct Cancellation(Arc<watch::Sender<bool>>);

impl Default for Cancellation {
    fn default() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }
}

impl Cancellation {
    pub(crate) fn cancel(&self) {
        self.0.send_replace(true);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    pub(crate) async fn cancelled(&self) {
        let mut receiver = self.0.subscribe();
        let _ = receiver.wait_for(|cancelled| *cancelled).await;
    }
}

pub(crate) trait Tools {
    fn descriptors(&self) -> Vec<ToolDescriptor>;

    /// The adapter owns schema validation; execution receives only the validated object.
    fn validate(&self, name: &str, arguments: &serde_json::Value) -> Result<(), ToolError>;

    /// Implementations must clean up owned processes on drop and reconcile effects whose
    /// completion cannot be observed. Cancellation/deadline may drop this future.
    async fn execute(&mut self, call: &ValidatedCall, cancellation: &Cancellation) -> ToolOutcome;
}

pub(crate) trait Host {
    /// Revalidate session authority immediately before each effect.
    async fn authorize(&mut self, call: &ValidatedCall) -> Result<(), ToolError>;

    /// Called only after a record is durable. UI delivery does not own session state.
    fn committed(&mut self, record: &Record);
}
