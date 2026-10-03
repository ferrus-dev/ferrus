//! Bounded SSE framing and Chat Completions assembly. Only [DONE] releases calls.

use crate::nano::{provider::*, tools::ToolCall};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet, VecDeque};

pub(super) fn error(kind: ProviderErrorKind) -> ProviderError {
    let retryable = matches!(
        kind,
        ProviderErrorKind::TruncatedStream
            | ProviderErrorKind::Timeout
            | ProviderErrorKind::Transport
            | ProviderErrorKind::RateLimited
    );
    ProviderError::new(kind, retryable)
}

#[derive(Default)]
struct Call {
    id: String,
    name: String,
    arguments: String,
}

pub(super) struct Decoder {
    settings: ProviderSettings,
    bytes: usize,
    line: Vec<u8>,
    data: Vec<u8>,
    events: VecDeque<ProviderEvent>,
    text: String,
    reasoning: BTreeMap<String, String>,
    calls: BTreeMap<usize, Call>,
    finish: Option<FinishReason>,
    usage: Option<Usage>,
    id: Option<String>,
    model: Option<String>,
    done: bool,
}

impl Decoder {
    pub(super) fn new(settings: ProviderSettings) -> Self {
        Self {
            settings,
            bytes: 0,
            line: Vec::new(),
            data: Vec::new(),
            events: VecDeque::new(),
            text: String::new(),
            reasoning: BTreeMap::new(),
            calls: BTreeMap::new(),
            finish: None,
            usage: None,
            id: None,
            model: None,
            done: false,
        }
    }

    pub(super) fn next(&mut self) -> Option<ProviderEvent> {
        self.events.pop_front()
    }

    pub(super) fn push(&mut self, bytes: &[u8]) -> Result<(), ProviderError> {
        self.bytes = self.bytes.saturating_add(bytes.len());
        if self.bytes > self.settings.wire_bytes {
            return Err(error(ProviderErrorKind::ResponseLimit));
        }

        for &byte in bytes {
            if byte == b'\n' {
                if self.line.last() == Some(&b'\r') {
                    self.line.pop();
                }

                let line = std::mem::take(&mut self.line);
                if line.is_empty() {
                    if !self.data.is_empty() {
                        let data = std::mem::take(&mut self.data);
                        let text = std::str::from_utf8(&data)
                            .map_err(|_| error(ProviderErrorKind::Protocol))?;
                        self.event(text.trim())?;
                    }
                } else if line.starts_with(b"data:") {
                    let value = line[5..].strip_prefix(b" ").unwrap_or(&line[5..]);
                    if !self.data.is_empty() {
                        self.data.push(b'\n');
                    }
                    self.data.extend_from_slice(value);
                } else if !(line.starts_with(b":")
                    || line.starts_with(b"id:")
                    || line.starts_with(b"retry:")
                    || line == b"event: message")
                {
                    return Err(error(ProviderErrorKind::Unsupported));
                }
            } else {
                self.line.push(byte);
            }

            if self.line.len().saturating_add(self.data.len()) > self.settings.event_bytes {
                return Err(error(ProviderErrorKind::ResponseLimit));
            }
        }
        Ok(())
    }

    fn event(&mut self, data: &str) -> Result<(), ProviderError> {
        if self.done {
            return Err(error(ProviderErrorKind::Protocol));
        }

        if data == "[DONE]" {
            let finish = self
                .finish
                .clone()
                .ok_or_else(|| error(ProviderErrorKind::TruncatedStream))?;

            let mut ids = HashSet::new();
            let mut calls = Vec::new();

            for (expected, (index, call)) in self.calls.iter().enumerate() {
                if finish != FinishReason::Length
                    && (*index != expected
                        || call.id.is_empty()
                        || call.name.is_empty()
                        || !ids.insert(&call.id))
                {
                    return Err(error(ProviderErrorKind::Protocol));
                }

                calls.push(ToolCall {
                    provider_call_id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                });
            }

            if finish != FinishReason::Length
                && (finish == FinishReason::ToolCalls) == calls.is_empty()
            {
                return Err(error(ProviderErrorKind::Protocol));
            }

            let continuation = (!self.reasoning.is_empty()).then(|| json!(self.reasoning));
            self.events.push_back(ProviderEvent::Completed {
                response: ModelResponse {
                    finish,
                    text: std::mem::take(&mut self.text),
                    calls,
                    continuation,
                },
                usage: self.usage.clone(),
            });

            self.done = true;

            return Ok(());
        }

        let value: Value =
            serde_json::from_str(data).map_err(|_| error(ProviderErrorKind::Protocol))?;

        if value.get("error").is_some() {
            return Err(error(ProviderErrorKind::Protocol));
        }

        if let Some(id) = value.get("id").and_then(Value::as_str) {
            if self.id.as_deref().is_some_and(|old| old != id) {
                return Err(error(ProviderErrorKind::Protocol));
            }
            self.id = Some(id.into());
        }

        if let Some(model) = value.get("model").and_then(Value::as_str) {
            if self.model.as_deref().is_some_and(|old| old != model) {
                return Err(error(ProviderErrorKind::Protocol));
            }
            self.model = Some(model.into());
        }

        if let Some(usage) = value.get("usage").filter(|v| !v.is_null()) {
            if self.usage.is_some() {
                return Err(error(ProviderErrorKind::Protocol));
            }

            self.usage = Some(Usage {
                input_tokens: usage
                    .get("prompt_tokens")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| error(ProviderErrorKind::Protocol))?,
                output_tokens: usage
                    .get("completion_tokens")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| error(ProviderErrorKind::Protocol))?,
                reported: true,
            });
        }

        let choices = value
            .get("choices")
            .and_then(Value::as_array)
            .ok_or_else(|| error(ProviderErrorKind::Protocol))?;

        if choices.is_empty() {
            if self.finish.is_none() || self.usage.is_none() {
                return Err(error(ProviderErrorKind::Protocol));
            }
            return Ok(());
        }

        if choices.len() != 1
            || choices[0].get("index").and_then(Value::as_u64) != Some(0)
            || self.finish.is_some()
        {
            return Err(error(ProviderErrorKind::Unsupported));
        }

        let choice = &choices[0];
        let delta = choice
            .get("delta")
            .and_then(Value::as_object)
            .ok_or_else(|| error(ProviderErrorKind::Protocol))?;

        for (key, value) in delta {
            if value.is_null() {
                continue;
            }

            match key.as_str() {
                "role" if value.as_str() == Some("assistant") => (),
                "content" => {
                    let text = value
                        .as_str()
                        .ok_or_else(|| error(ProviderErrorKind::Unsupported))?;

                    self.text.push_str(text);
                    self.events.push_back(ProviderEvent::TextDelta(text.into()));
                }
                "reasoning_content" | "reasoning" => {
                    let text = value
                        .as_str()
                        .ok_or_else(|| error(ProviderErrorKind::Unsupported))?;

                    self.reasoning
                        .entry(key.clone())
                        .or_default()
                        .push_str(text);
                }
                "tool_calls" => {
                    for call in value
                        .as_array()
                        .ok_or_else(|| error(ProviderErrorKind::Protocol))?
                    {
                        let fields = call
                            .as_object()
                            .ok_or_else(|| error(ProviderErrorKind::Protocol))?;

                        if fields.keys().any(|key| {
                            !matches!(key.as_str(), "index" | "id" | "type" | "function")
                        }) {
                            return Err(error(ProviderErrorKind::Unsupported));
                        }

                        let index = call
                            .get("index")
                            .and_then(Value::as_u64)
                            .ok_or_else(|| error(ProviderErrorKind::Protocol))?;

                        if index >= self.settings.max_tool_calls as u64 {
                            return Err(error(ProviderErrorKind::ResponseLimit));
                        }

                        if call.get("type").is_some_and(|v| v != "function") {
                            return Err(error(ProviderErrorKind::Unsupported));
                        }

                        let entry = self.calls.entry(index as usize).or_default();
                        if let Some(id) = call.get("id").filter(|v| !v.is_null()) {
                            let id = id
                                .as_str()
                                .ok_or_else(|| error(ProviderErrorKind::Protocol))?;

                            if !entry.id.is_empty() {
                                return Err(error(ProviderErrorKind::Protocol));
                            }

                            entry.id = id.into();
                        }

                        if let Some(function) = call.get("function") {
                            let function = function
                                .as_object()
                                .ok_or_else(|| error(ProviderErrorKind::Protocol))?;

                            for (field, value) in function {
                                let text = value
                                    .as_str()
                                    .ok_or_else(|| error(ProviderErrorKind::Protocol))?;

                                match field.as_str() {
                                    "name" => entry.name.push_str(text),
                                    "arguments" => {
                                        entry.arguments.push_str(text);
                                        self.events
                                            .push_back(ProviderEvent::ArgumentsDelta(text.into()));
                                    }
                                    _ => return Err(error(ProviderErrorKind::Unsupported)),
                                }
                            }
                        }
                    }
                }
                _ => return Err(error(ProviderErrorKind::Unsupported)),
            }
        }

        if let Some(reason) = choice.get("finish_reason").filter(|v| !v.is_null()) {
            self.finish = Some(match reason.as_str() {
                Some("stop") => FinishReason::Stop,
                Some("tool_calls") => FinishReason::ToolCalls,
                Some("length") => FinishReason::Length,
                _ => return Err(error(ProviderErrorKind::Unsupported)),
            });
        }

        Ok(())
    }
}
