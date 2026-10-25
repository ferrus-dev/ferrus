//! Host HTTP policy and typed diagnostics below svir's protocol implementation.

use super::ProviderSettings;
use anyhow::Result;
use futures_util::{StreamExt, stream};
use reqwest::{
    Client,
    header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderValue},
};
use std::time::Duration;
use svir::{
    Error, ErrorKind,
    http::{Backend, BoxBody, HttpRequest, HttpResponse, Method},
};

pub(super) struct Transport {
    client: Client,
    event_bytes: usize,
}

#[derive(Debug, thiserror::Error)]
#[error("Unexpected provider content type")]
pub(super) struct UnexpectedContentType;

impl Transport {
    pub(super) fn new(settings: &ProviderSettings) -> Result<Self> {
        let timeout = Duration::from_millis(settings.request_timeout_ms);
        Ok(Self {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .no_proxy()
                .connect_timeout(timeout.min(Duration::from_secs(10)))
                .read_timeout(timeout)
                .build()
                .map_err(|_| anyhow::anyhow!("Cannot initialize nano HTTP client"))?,
            event_bytes: settings.event_bytes,
        })
    }
}

impl Backend for Transport {
    type Body = BoxBody;

    async fn send(&self, request: HttpRequest) -> Result<HttpResponse<BoxBody>, Error> {
        let method = match request.method {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            _ => return Err(Error::new(ErrorKind::Unsupported)),
        };
        let mut outgoing = self.client.request(method, request.url);
        for (name, value) in request.headers {
            let mut value =
                HeaderValue::from_str(&value).map_err(|_| Error::new(ErrorKind::Config))?;
            value.set_sensitive(!matches!(name.as_str(), "content-type" | "accept"));
            outgoing = outgoing.header(name, value);
        }
        if let Some(mut body) = request.body {
            let mut bytes = Vec::new();
            while let Some(chunk) = body.stream.next().await {
                bytes.extend_from_slice(&chunk?);
            }
            outgoing = outgoing.header(CONTENT_LENGTH, body.length).body(bytes);
        }
        let mut response = outgoing.send().await.map_err(transport_error)?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(key, value)| {
                Some((key.as_str().to_owned(), value.to_str().ok()?.to_owned()))
            })
            .collect();
        if response.status().is_success() {
            if !response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|value| {
                    value
                        .split(';')
                        .next()
                        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
                })
            {
                return Err(Error::new(ErrorKind::Unsupported).with_source(UnexpectedContentType));
            }
            let body = stream::try_unfold(response, |mut response| async move {
                Ok(response
                    .chunk()
                    .await
                    .map_err(transport_error)?
                    .map(|chunk| (chunk, response)))
            });
            return Ok(HttpResponse::new(status, headers, Box::pin(body)));
        }
        // Keep bounded validation-error reads and immediate handling of other
        // HTTP failures, even if the server never finishes its error body.
        let mut bytes = Vec::new();
        if matches!(status, 400 | 413 | 422) {
            while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
                if chunk.len() > self.event_bytes.saturating_sub(bytes.len()) {
                    return Err(Error::new(ErrorKind::ResponseLimit).with_status(status));
                }
                bytes.extend_from_slice(&chunk);
            }
            // svir inspects a bounded prefix of an error body. Preserve Nano's
            // explicit overflow codes even when a long message precedes them.
            let code = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|value| value.pointer("/error/code")?.as_str().map(str::to_owned));
            if matches!(
                code.as_deref(),
                Some("context_length_exceeded" | "context_window_exceeded")
            ) {
                let mut error = Error::new(ErrorKind::ContextOverflow).with_status(status);
                if let Some(delay) = response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                {
                    error = error.with_retry_after(Duration::from_secs(delay.min(30)));
                }
                return Err(error);
            }
        }
        let body: BoxBody = Box::pin(stream::iter([Ok(bytes::Bytes::from(bytes))]));
        Ok(HttpResponse::new(status, headers, body))
    }
}

fn transport_error(error: reqwest::Error) -> Error {
    // Never forward raw URLs, credentials, response bodies, or headers.
    Error::new(if error.is_timeout() {
        ErrorKind::Timeout
    } else {
        ErrorKind::Transport
    })
}
