//! Explicit, host-local provider configuration. No global client or credential environment.

use super::{private, provider::ProviderSettings};
use anyhow::{Context, Result, ensure};
use reqwest::{Url, header::HeaderValue};
use serde::Deserialize;
use std::{
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub base_url: String,
    pub model: String,
    pub api_key_file: Option<PathBuf>,
    /// Explicit owner-only stdio MCP configuration. Never sent to the provider.
    #[cfg(feature = "nano-mcp")]
    pub mcp_config_file: Option<PathBuf>,
    #[serde(default = "default_context")]
    pub context_tokens: u64,
    #[serde(default = "default_output")]
    pub max_output_tokens: u64,
    #[serde(default)]
    pub temperature: f64,
    #[serde(default = "default_timeout")]
    pub request_timeout_ms: u64,
    #[serde(default = "default_wire")]
    pub wire_bytes: usize,
    #[serde(default = "default_event")]
    pub event_bytes: usize,
    #[serde(default = "default_calls")]
    pub max_tool_calls: usize,
    #[serde(default = "default_usage")]
    pub include_usage: bool,
}

fn default_context() -> u64 {
    32_768
}
fn default_output() -> u64 {
    4096
}
fn default_timeout() -> u64 {
    120_000
}
fn default_wire() -> usize {
    4 * 1024 * 1024
}
fn default_event() -> usize {
    256 * 1024
}
fn default_calls() -> usize {
    64
}
fn default_usage() -> bool {
    true
}

impl Config {
    /// The native launcher calls this explicitly; normal Ferrus config never reads it.
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let bytes = read_private(path, 16 * 1024)?;
        let text = std::str::from_utf8(&bytes).context("Nano settings must be UTF-8")?;
        // TOML diagnostics may quote unknown secret-bearing fields: do not return them.
        toml::from_str(text).map_err(|_| anyhow::anyhow!("Invalid nano provider settings"))
    }

    pub(crate) fn validate(&self) -> Result<(Url, ProviderSettings)> {
        let mut base =
            Url::parse(&self.base_url).map_err(|_| anyhow::anyhow!("Invalid provider URL"))?;

        ensure!(
            base.username().is_empty()
                && base.password().is_none()
                && base.query().is_none()
                && base.fragment().is_none()
                && base.host_str().is_some(),
            "Provider URL must not contain credentials, query, or fragment"
        );

        let local = matches!(base.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        ensure!(
            base.scheme() == "https" || (base.scheme() == "http" && local),
            "Provider requires HTTPS or loopback HTTP"
        );

        ensure!(
            base.path().trim_end_matches('/') == "/v1",
            "This adapter requires a /v1 base URL"
        );

        ensure!(
            !self.model.trim().is_empty()
                && self.model.len() <= 256
                && !self.model.chars().any(char::is_control),
            "Invalid model identifier"
        );

        ensure!(
            self.max_output_tokens > 0
                && self.max_output_tokens < self.context_tokens
                && self.context_tokens <= 16_777_216,
            "Invalid provider context/output limits"
        );

        ensure!(
            self.temperature.is_finite() && (0.0..=2.0).contains(&self.temperature),
            "Invalid temperature"
        );

        ensure!(
            (1..=3_600_000).contains(&self.request_timeout_ms)
                && (1024..=64 * 1024 * 1024).contains(&self.wire_bytes)
                && (256..=1024 * 1024).contains(&self.event_bytes)
                && self.event_bytes <= self.wire_bytes
                && (1..=256).contains(&self.max_tool_calls),
            "Invalid provider transport limits"
        );

        base.set_path("/v1");

        let settings = ProviderSettings {
            api: "openai_chat_completions_v1".into(),
            base_url: base.to_string(),
            model: self.model.clone(),
            context_tokens: self.context_tokens,
            max_output_tokens: self.max_output_tokens,
            temperature: self.temperature,
            request_timeout_ms: self.request_timeout_ms,
            wire_bytes: self.wire_bytes,
            event_bytes: self.event_bytes,
            max_tool_calls: self.max_tool_calls,
            include_usage: self.include_usage,
        };

        base.set_path("/v1/chat/completions");

        Ok((base, settings))
    }

    pub(crate) fn authorization(&self) -> Result<Option<HeaderValue>> {
        let Some(path) = &self.api_key_file else {
            return Ok(None);
        };

        ensure!(
            path.is_absolute(),
            "Credential file must be an absolute host-local path"
        );

        let bytes = read_private(path, 4096)?;
        let key = std::str::from_utf8(&bytes)
            .map_err(|_| anyhow::anyhow!("Invalid provider credential"))?
            .trim();

        ensure!(
            !key.is_empty() && key.bytes().all(|b| b.is_ascii_graphic()),
            "Invalid provider credential"
        );

        let mut header = HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| anyhow::anyhow!("Invalid provider credential"))?;

        header.set_sensitive(true);
        Ok(Some(header))
    }
}

fn read_private(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = private::read_only_file(path)
        .map_err(|_| anyhow::anyhow!("Nano host file must exist and be owner-only"))?;

    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("Cannot read nano host file"))?;

    ensure!(bytes.len() <= limit, "Nano host file exceeds size limit");
    Ok(bytes)
}
