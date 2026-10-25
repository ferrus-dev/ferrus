//! svir Chat Completions adapter; Nano retains its durable provider contracts.

mod transport;

use crate::nano::{config::Config, provider::*, tools::ToolCall};
use anyhow::Result;
use svir::{Completion, Event, Request, openai::chat::Encoder};

pub(crate) struct OpenAi {
    client: svir::Client<transport::Transport>,
    settings: ProviderSettings,
    active: Option<svir::EventStream<transport::Transport>>,
}

impl OpenAi {
    pub(crate) fn new(config: Config) -> Result<Self> {
        let (_, mut settings) = config.validate()?;
        // Record the exact decimal svir's f32 encoder sends in the existing f64
        // journal field. Old records remain readable and are never rewritten.
        settings.temperature = settings
            .temperature
            .map(|value| -> Result<f64> {
                Ok(serde_json::from_str(&serde_json::to_string(
                    &(value as f32),
                )?)?)
            })
            .transpose()?;
        let transport = transport::Transport::new(&settings)?;
        let mut builder = svir::Client::openai(&settings.base_url)
            .http(transport)
            .lean_retry(false)
            .think(svir::Think::Keep)
            .limits(limits(&settings))
            .include_usage(settings.include_usage)
            // Transport retains per-read timeouts; Engine owns the total deadline.
            .no_idle_timeout();
        if let Some(header) = config.authorization()? {
            let key = header
                .to_str()
                .ok()
                .and_then(|value| value.strip_prefix("Bearer "))
                .ok_or_else(|| anyhow::anyhow!("Invalid provider credential"))?;
            builder = builder.api_key(key);
        }
        Ok(Self {
            client: builder
                .build()
                .map_err(|_| anyhow::anyhow!("Cannot initialize nano provider client"))?,
            settings,
            active: None,
        })
    }

    fn request(&self, request: ModelRequest) -> Result<Request, ProviderError> {
        let output = request
            .max_output_tokens
            .min(self.settings.max_output_tokens.unwrap_or(u64::MAX));
        if output == 0 {
            return Err(failure(ProviderErrorKind::ContextOverflow));
        }
        let mut encoded = Request::new(&self.settings.model)
            .max_tokens(output)
            .include_usage(self.settings.include_usage)
            .send_reasoning(true);
        if let Some(value) = self.settings.temperature {
            encoded = encoded.temperature(value as f32);
        }
        if let Some(value) = self.settings.reasoning_effort {
            encoded = encoded.reasoning(match value {
                ReasoningEffort::None => svir::Effort::Off,
                ReasoningEffort::Minimal => svir::Effort::Minimal,
                ReasoningEffort::Low => svir::Effort::Low,
                ReasoningEffort::Medium => svir::Effort::Medium,
                ReasoningEffort::High => svir::Effort::High,
                ReasoningEffort::XHigh => svir::Effort::XHigh,
                ReasoningEffort::Max => svir::Effort::Max,
            });
        }
        for (index, message) in request.messages.into_iter().enumerate() {
            encoded = match message {
                Message::System { text } if index == 0 => encoded.system(text),
                Message::System { .. } => return Err(failure(ProviderErrorKind::Unsupported)),
                Message::User { text } => encoded.user(text),
                Message::Assistant { response } => encoded.assistant(continuation(response)?),
                Message::Tool {
                    provider_call_id,
                    outcome,
                } => encoded.tool_result(
                    provider_call_id,
                    // Keep Nano's success/failed/unknown JSON envelope unchanged.
                    serde_json::to_string(&outcome)
                        .map_err(|_| failure(ProviderErrorKind::Protocol))?,
                ),
            };
        }
        for tool in request.tools {
            encoded = encoded
                .tool(svir::Tool::new(tool.name, tool.description).schema(tool.input_schema));
        }
        Ok(encoded)
    }

    #[cfg(test)]
    pub(crate) fn body(&self, request: ModelRequest) -> Result<Vec<u8>, ProviderError> {
        let request = self.request(request)?;
        let mut encoder = Encoder::new();
        if let Some(window) = self.settings.context_tokens {
            encoder = encoder.context_tokens(window);
        }
        encoder
            .encode(&request)
            .map_err(map_error)?
            .into_bytes()
            .map(|b| b.to_vec())
            .map_err(map_error)
    }
}

impl Provider for OpenAi {
    fn settings(&self) -> Option<ProviderSettings> {
        Some(self.settings.clone())
    }

    fn estimate_input_tokens(&self, request: &ModelRequest) -> Result<u64, ProviderError> {
        Encoder::new()
            .encode(&self.request(request.clone())?)
            .map(|body| body.len())
            .map_err(map_error)
    }

    fn cancel(&mut self) {
        self.active = None;
    }

    async fn start(&mut self, request: ModelRequest) -> Result<(), ProviderError> {
        self.active = None;
        let request = self.request(request)?;
        // Same encoder as the client; Engine retains projection/reservations.
        if let Some(window) = self.settings.context_tokens {
            Encoder::new()
                .context_tokens(window)
                .encode(&request)
                .map_err(map_error)?;
        }
        self.active = Some(self.client.stream(request).await.map_err(map_error)?);
        Ok(())
    }

    async fn next_event(&mut self) -> Result<Option<ProviderEvent>, ProviderError> {
        // Own the stream in this future: dropping an interrupted read closes it.
        let Some(mut active) = self.active.take() else {
            return Ok(None);
        };
        loop {
            let Some(event) = active.next().await else {
                return Ok(None);
            };
            let event = event.map_err(|error| {
                map_error(error).with_diagnostic(ProviderDiagnostic::EventStream)
            })?;
            if let Some(event) = event_to_provider(event)? {
                if !matches!(event, ProviderEvent::Completed { .. }) {
                    self.active = Some(active);
                }
                return Ok(Some(event));
            }
            // Reasoning is retained as continuation, not displayed. Ready
            // fragments must not starve cancellation while it is assembled.
            tokio::task::yield_now().await;
        }
    }
}

fn continuation(response: ModelResponse) -> Result<Completion, ProviderError> {
    let mut done = Completion::new(match response.finish {
        FinishReason::Stop => svir::FinishReason::Stop,
        FinishReason::ToolCalls => svir::FinishReason::ToolCalls,
        FinishReason::Length => svir::FinishReason::Length,
    });
    done.text = response.text;
    if response.finish != FinishReason::Length {
        done.calls = response
            .calls
            .into_iter()
            .map(|call| svir::ToolCall::new(call.provider_call_id, call.name, call.arguments))
            .collect();
    }
    if let Some(data) = response.continuation {
        for (key, value) in data
            .as_object()
            .ok_or_else(|| failure(ProviderErrorKind::Unsupported))?
        {
            let source = match key.as_str() {
                "reasoning" => svir::ReasoningSource::Reasoning,
                "reasoning_content" => svir::ReasoningSource::ReasoningContent,
                _ => return Err(failure(ProviderErrorKind::Unsupported)),
            };
            done.reasoning.push(svir::Reasoning::new(
                source,
                value
                    .as_str()
                    .ok_or_else(|| failure(ProviderErrorKind::Unsupported))?,
            ));
        }
    }
    Ok(done)
}

pub(super) fn limits(settings: &ProviderSettings) -> svir::Limits {
    svir::Limits::default()
        .wire_bytes(settings.wire_bytes)
        .event_bytes(settings.event_bytes)
        .tool_calls(settings.max_tool_calls)
}

pub(super) fn event_to_provider(event: Event) -> Result<Option<ProviderEvent>, ProviderError> {
    Ok(Some(match event {
        Event::Text(text) => ProviderEvent::TextDelta(text),
        Event::ToolCallDelta(call) if call.arguments.is_empty() => return Ok(None),
        Event::ToolCallDelta(call) => ProviderEvent::ArgumentsDelta(call.arguments),
        Event::Reasoning(_) => return Ok(None),
        Event::Completed(done) => {
            let finish = match done.finish {
                svir::FinishReason::Stop => FinishReason::Stop,
                svir::FinishReason::ToolCalls => FinishReason::ToolCalls,
                svir::FinishReason::Length => FinishReason::Length,
                _ => {
                    return Err(failure(ProviderErrorKind::Unsupported)
                        .with_diagnostic(ProviderDiagnostic::EventStream));
                }
            };
            let mut reasoning = serde_json::Map::new();
            for part in done.reasoning {
                let key = match part.source {
                    svir::ReasoningSource::Reasoning => "reasoning",
                    svir::ReasoningSource::ReasoningContent => "reasoning_content",
                    _ => return Err(failure(ProviderErrorKind::Unsupported)),
                };
                reasoning.insert(key.into(), part.text.into());
            }
            let calls = if finish == FinishReason::Length {
                // Raw audit calls in the existing Nano journal. Engine/replay
                // exclude every Length call from execution and future history.
                done.cut_calls
                    .into_iter()
                    .map(|call| ToolCall {
                        provider_call_id: call.id.unwrap_or_default(),
                        name: call.name.unwrap_or_default(),
                        arguments: call.arguments,
                    })
                    .collect()
            } else {
                done.calls
                    .into_iter()
                    .map(|call| ToolCall {
                        provider_call_id: call.id,
                        name: call.name,
                        arguments: call.arguments,
                    })
                    .collect()
            };
            ProviderEvent::Completed {
                response: ModelResponse {
                    finish,
                    text: done.text,
                    calls,
                    continuation: (!reasoning.is_empty()).then_some(reasoning.into()),
                },
                usage: done.usage.map(|usage| Usage {
                    input_tokens: usage.input,
                    output_tokens: usage.output,
                    reported: true,
                }),
            }
        }
        _ => return Err(failure(ProviderErrorKind::Unsupported)),
    }))
}

pub(super) fn failure(kind: ProviderErrorKind) -> ProviderError {
    let retryable = matches!(
        kind,
        ProviderErrorKind::Transport
            | ProviderErrorKind::Timeout
            | ProviderErrorKind::RateLimited
            | ProviderErrorKind::TruncatedStream
    );
    ProviderError::new(kind, retryable)
}

pub(super) fn map_error(error: svir::Error) -> ProviderError {
    use std::error::Error;
    let kind = match error.kind() {
        svir::ErrorKind::Transport => ProviderErrorKind::Transport,
        svir::ErrorKind::Timeout => ProviderErrorKind::Timeout,
        svir::ErrorKind::RateLimited => ProviderErrorKind::RateLimited,
        svir::ErrorKind::TruncatedStream => ProviderErrorKind::TruncatedStream,
        svir::ErrorKind::Authentication => ProviderErrorKind::Authentication,
        svir::ErrorKind::ContextOverflow => ProviderErrorKind::ContextOverflow,
        svir::ErrorKind::Protocol | svir::ErrorKind::Server => ProviderErrorKind::Protocol,
        svir::ErrorKind::ResponseLimit => ProviderErrorKind::ResponseLimit,
        _ => ProviderErrorKind::Unsupported,
    };
    let mut out = failure(kind);
    out.retry_after_ms = error
        .retry_after()
        .map_or(0, |delay| delay.as_millis().min(30_000) as u64);
    out.diagnostic = if error
        .source()
        .is_some_and(|source| source.is::<transport::UnexpectedContentType>())
    {
        Some(ProviderDiagnostic::UnexpectedContentType)
    } else {
        error
            .status()
            .map(|status| ProviderDiagnostic::Http { status })
    };
    out
}
