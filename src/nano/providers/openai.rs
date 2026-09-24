//! LM Studio /v1/chat/completions transport with optional Bearer authentication.

use super::sse::{Decoder, error};
use crate::nano::{config::Config, provider::*};
use anyhow::Result;
use reqwest::{
    Client, Response, Url,
    header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue},
};
use serde_json::{Value, json};
use std::time::Duration;

struct Active {
    response: Response,
    decoder: Decoder,
}

pub(crate) struct OpenAi {
    client: Client,
    endpoint: Url,
    authorization: Option<HeaderValue>,
    settings: ProviderSettings,
    active: Option<Active>,
}

impl OpenAi {
    pub(crate) fn new(config: Config) -> Result<Self> {
        let (endpoint, settings) = config.validate()?;
        let authorization = config.authorization()?;
        let timeout = Duration::from_millis(settings.request_timeout_ms);
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .connect_timeout(timeout.min(Duration::from_secs(10)))
            .read_timeout(timeout)
            .timeout(timeout)
            .build()
            .map_err(|_| anyhow::anyhow!("Cannot initialize nano HTTP client"))?;

        Ok(Self {
            client,
            endpoint,
            authorization,
            settings,
            active: None,
        })
    }

    pub(super) fn body(&self, request: ModelRequest) -> Result<Vec<u8>, ProviderError> {
        let (body, output) = self.body_unbounded(request)?;
        if body.len() as u64 > self.settings.context_tokens.saturating_sub(output) {
            return Err(error(ProviderErrorKind::ContextOverflow));
        }
        Ok(body)
    }

    fn body_unbounded(&self, request: ModelRequest) -> Result<(Vec<u8>, u64), ProviderError> {
        let output = request
            .max_output_tokens
            .min(self.settings.max_output_tokens);

        if output == 0 {
            return Err(error(ProviderErrorKind::ContextOverflow));
        }

        let mut messages = Vec::new();
        for message in request.messages {
            messages.push(match message {
                Message::User { text } => json!({"role":"user", "content":text}),
                Message::Tool { provider_call_id, outcome } => json!({
                    "role":"tool", "tool_call_id":provider_call_id,
                    "content":serde_json::to_string(&outcome).map_err(|_| error(ProviderErrorKind::Protocol))?,
                }),
                Message::Assistant { response } => {
                    let mut value = json!({"role":"assistant", "content":response.text});
                    if !response.calls.is_empty() {
                        value["tool_calls"] = Value::Array(response.calls.into_iter().map(|call| json!({
                            "id":call.provider_call_id,"type":"function", "function":{"name":call.name,"arguments":call.arguments}
                        })).collect());
                    }
                    if let Some(continuation) = response.continuation {
                        for (key, data) in continuation.as_object().ok_or_else(|| error(ProviderErrorKind::Unsupported))? {
                            if !matches!(key.as_str(), "reasoning" | "reasoning_content") || !data.is_string() {
                                return Err(error(ProviderErrorKind::Unsupported));
                            }
                            value[key] = data.clone();
                        }
                    }
                    value
                }
            });
        }

        let mut body = json!({"model":self.settings.model, "messages":messages, "stream":true,
            "temperature":self.settings.temperature, "max_tokens":output, "n":1});

        if self.settings.include_usage {
            body["stream_options"] = json!({"include_usage":true});
        }

        if !request.tools.is_empty() {
            body["tools"] = Value::Array(request.tools.into_iter().map(|tool| json!({
                "type":"function", "function":{"name":tool.name,"description":tool.description,"parameters":tool.input_schema}
            })).collect());

            body["tool_choice"] = json!("auto");
        }

        let bytes = serde_json::to_vec(&body).map_err(|_| error(ProviderErrorKind::Protocol))?;
        Ok((bytes, output))
    }
}

impl Provider for OpenAi {
    fn estimate_input_tokens(&self, request: &ModelRequest) -> Result<u64, ProviderError> {
        self.body_unbounded(request.clone())
            .map(|(body, _)| body.len() as u64)
    }
    fn cancel(&mut self) {
        self.active = None;
    }

    fn settings(&self) -> Option<ProviderSettings> {
        Some(self.settings.clone())
    }

    async fn start(&mut self, request: ModelRequest) -> Result<(), ProviderError> {
        self.active = None;

        let body = self.body(request)?;
        let mut request = self
            .client
            .post(self.endpoint.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(body);

        if let Some(header) = &self.authorization {
            request = request.header(AUTHORIZATION, header.clone());
        }

        let mut response = request.send().await.map_err(transport_error)?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let retry_after_ms = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0)
                .saturating_mul(1000)
                .min(30_000);

            if status != 400 && status != 413 && status != 422 {
                let mut failure = error(match status {
                    401 | 403 => ProviderErrorKind::Authentication,
                    429 => ProviderErrorKind::RateLimited,
                    408 | 504 => ProviderErrorKind::Timeout,
                    500 | 502 | 503 => ProviderErrorKind::Transport,
                    _ => ProviderErrorKind::Unsupported,
                });

                failure.retry_after_ms = retry_after_ms;

                return Err(failure);
            }

            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
                if chunk.len() > self.settings.event_bytes.saturating_sub(body.len()) {
                    return Err(error(ProviderErrorKind::ResponseLimit));
                }

                body.extend_from_slice(&chunk);
            }

            let code = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|v| v.get("error")?.get("code")?.as_str().map(str::to_owned));

            let kind = if matches!(
                code.as_deref(),
                Some("context_length_exceeded" | "context_window_exceeded")
            ) {
                ProviderErrorKind::ContextOverflow
            } else {
                ProviderErrorKind::Unsupported
            };

            let mut failure = error(kind);
            failure.retry_after_ms = retry_after_ms;

            return Err(failure);
        }
        if !response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(';')
                    .next()
                    .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
            })
        {
            return Err(error(ProviderErrorKind::Unsupported));
        }

        self.active = Some(Active {
            response,
            decoder: Decoder::new(self.settings.clone()),
        });

        Ok(())
    }

    async fn next_event(&mut self) -> Result<Option<ProviderEvent>, ProviderError> {
        // Own the response inside the future so cancelling a read closes the stream.
        let Some(mut active) = self.active.take() else {
            return Ok(None);
        };

        loop {
            if let Some(event) = active.decoder.next() {
                if !matches!(event, ProviderEvent::Completed { .. }) {
                    self.active = Some(active);
                }
                return Ok(Some(event));
            }

            let chunk = active
                .response
                .chunk()
                .await
                .map_err(transport_error)?
                .ok_or_else(|| error(ProviderErrorKind::TruncatedStream))?;

            active.decoder.push(&chunk)?;
        }
    }
}

fn transport_error(failure: reqwest::Error) -> ProviderError {
    // Never journal raw URLs, headers, or server error bodies.
    error(if failure.is_timeout() {
        ProviderErrorKind::Timeout
    } else {
        ProviderErrorKind::Transport
    })
}
