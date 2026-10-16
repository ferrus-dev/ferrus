//! Bounded presentation of durable session records, independent of host authority.

use crate::nano::session::{Budget, Record, SessionEvent};
use std::collections::VecDeque;

pub(super) const DISPLAY_BYTES: usize = 32 * 1024;
const ENTRY_BYTES: usize = 2048;
pub(super) const ENTRIES: usize = 128;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub run_id: String,
    pub task_id: String,
    pub sequence: u64,
    pub entries: VecDeque<String>,
    pub status: String,
    pub budget: Budget,
    pub ended: bool,
    pub turn: u64,
    pub accepted_inputs: VecDeque<String>,
}

/// Strip terminal control sequences as text, and bound bytes without splitting UTF-8.
pub(crate) fn display_text(text: &str, cap: usize) -> String {
    const TRUNCATED: &str = "\n[display truncated; full content is in the journal]";
    let truncated = text.len() > cap;
    let content_cap = if truncated {
        cap.saturating_sub(TRUNCATED.len())
    } else {
        cap
    };
    let mut result = String::new();
    for ch in text.chars() {
        let ch = if ch == '\t' {
            ' '
        } else if ch.is_control() && ch != '\n' {
            '?'
        } else {
            ch
        };
        if result.len() + ch.len_utf8() > content_cap {
            break;
        }
        result.push(ch);
    }
    if truncated {
        result.push_str(&TRUNCATED[..TRUNCATED.len().min(cap.saturating_sub(result.len()))]);
    }

    result
}

impl Snapshot {
    fn push(&mut self, label: &str, text: &str) {
        self.entries
            .push_back(format!("{label}: {}", display_text(text, ENTRY_BYTES)));
        while self.entries.len() > ENTRIES
            || self.entries.iter().map(String::len).sum::<usize>() > DISPLAY_BYTES
        {
            self.entries.pop_front();
        }
    }
    pub(crate) fn apply(&mut self, record: &Record) {
        self.sequence = record.sequence;
        self.budget = record.budget.clone();
        match &record.event {
            SessionEvent::Started {
                identity, input, ..
            } => {
                self.run_id = identity.session_id.clone();
                self.task_id = identity.task_id.clone().unwrap_or_default();
                self.status = "Starting".into();
                if identity.task_id.is_none() {
                    self.push(
                        "Session",
                        "Direct workspace conversation. Type a request to begin.",
                    );
                    return;
                }
                let initial = serde_json::Deserializer::from_str(input)
                    .into_iter::<serde_json::Value>()
                    .next()
                    .and_then(Result::ok);
                let task = initial
                    .as_ref()
                    .and_then(|value| value["documents"].as_array())
                    .and_then(|documents| {
                        documents
                            .iter()
                            .find(|document| document["kind"] == "task")
                            .and_then(|document| document["text"].as_str())
                    })
                    .unwrap_or(input);
                self.push("Task", task);
            }
            SessionEvent::UserInput { text, input_id } => {
                if let Some(id) = input_id {
                    self.accepted_inputs.push_back(id.clone());
                }
                while self.accepted_inputs.len() > 32 {
                    self.accepted_inputs.pop_front();
                }
                self.push("You", text);
            }
            SessionEvent::InputRequested => self.status = "Waiting for your input".into(),
            SessionEvent::ModelStarted { turn } => {
                self.turn = *turn;
                self.status = format!("Generating response (turn {turn})");
            }
            SessionEvent::ModelCompleted { response, .. } => {
                if !response.text.is_empty() {
                    self.push("Nano", &response.text);
                }
                self.status = "Processing response".into();
            }
            SessionEvent::ModelFailed { error, .. } => self.push("Provider", &format!("{error:?}")),
            SessionEvent::ToolIntent { call, .. } => {
                self.status = format!("Tool: {}", display_text(&call.name, 128));
                self.push("Tool", &format!("{} {}", call.name, call.arguments));
            }
            SessionEvent::ToolResult { outcome, .. } => self.push(
                "Result",
                &serde_json::to_string(outcome).unwrap_or_default(),
            ),
            SessionEvent::Ended { reason } => {
                self.ended = true;
                self.status = format!("Ended: {reason:?}");
                self.push("Session", &self.status.clone());
            }
            _ => (),
        }
    }
}
