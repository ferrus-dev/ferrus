//! Agent adapters for the supported supervisor and executor backends.
//!
//! This module centralizes how Ferrus names agent implementations, spawns them
//! interactively or headlessly, and derives the MCP configuration used by HQ.

pub(crate) mod claude;
pub(crate) mod codex;
pub(crate) mod goose;
pub(crate) mod opencode;
pub(crate) mod qwen;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use crate::agent_id::DEFAULT_AGENT_INDEX;

/// Describes one MCP server entry for a spawned Ferrus agent.
///
/// Ferrus writes these values into client-facing configuration so external
/// tools can reconnect to the correct `ferrus serve` process for a given role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfigEntry {
    /// Executable that should be launched for the MCP server.
    pub command: String,
    /// Arguments passed to [`Self::command`] when the MCP server starts.
    pub args: Vec<String>,
    /// Optional model override understood by the target client.
    pub model: Option<String>,
}

/// Best-effort agent configuration that HQ can show in its startup header.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AgentDisplayConfig {
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl AgentDisplayConfig {
    pub(crate) fn from_model(model: Option<&str>) -> Self {
        Self {
            model: normalized_model(model),
            effort: None,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.model.is_none() && self.effort.is_none()
    }

    pub(crate) fn merge_missing(mut self, fallback: Self) -> Self {
        if self.model.is_none() {
            self.model = fallback.model;
        }
        if self.effort.is_none() {
            self.effort = fallback.effort;
        }
        self
    }
}

/// Describes how Ferrus intends to run an agent process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRunMode<'a> {
    Interactive { prompt: Option<&'a str> },
    Headless { prompt: &'a str },
}

/// Declares how a headless prompt should be transported to the child process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessPromptTransport {
    /// Pass prompt as a regular CLI argument.
    Argv,
    /// Pass prompt via stdin and close stdin after writing.
    Stdin,
}

/// Behavior required from a supervisor-capable agent implementation.
///
/// Supervisor agents support both interactive sessions for humans and
/// headless sessions for HQ-managed workflows.
pub trait SupervisorAgent: Send + Sync {
    /// Returns the stable configuration name for this agent backend.
    fn name(&self) -> &'static str;

    /// Builds the command used when a human or HQ drives the supervisor.
    ///
    /// # Errors
    ///
    /// Returns an error when Ferrus cannot resolve the launcher command for
    /// the selected backend and mode.
    fn spawn(&self, mode: AgentRunMode<'_>) -> Result<Command> {
        self.spawn_with_index(mode, DEFAULT_AGENT_INDEX)
    }

    /// Builds the command used for a specific role-scoped MCP server index.
    fn spawn_with_index(&self, mode: AgentRunMode<'_>, index: u32) -> Result<Command>;

    /// Builds a command that returns the backend version string.
    ///
    /// The default implementation reuses the interactive launcher shape and
    /// appends `--version`, which works for regular CLIs and wrapper launchers
    /// like `node <script>`.
    fn version_command(&self) -> Result<Command> {
        let mut cmd = self.spawn(AgentRunMode::Interactive { prompt: None })?;
        cmd.arg("--version");
        Ok(cmd)
    }

    /// Builds the MCP configuration entry for this supervisor instance.
    ///
    /// The default implementation points the client back at the current
    /// `ferrus` executable so HQ can serve tools through the repository's own
    /// binary rather than relying on an external wrapper.
    ///
    /// # Errors
    ///
    /// Returns an error when Ferrus cannot resolve the path to the current
    /// executable.
    fn mcp_config_entry(&self, role: &str, index: u32) -> Result<McpConfigEntry> {
        Ok(McpConfigEntry {
            command: current_exe_string()?,
            args: serve_args(role, self.name(), index),
            model: self.model().map(ToOwned::to_owned),
        })
    }

    /// Returns the optional model override used by this backend.
    fn model(&self) -> Option<&str>;

    /// Returns the model and inference settings HQ should display for this backend.
    ///
    /// Implementations should prefer the Ferrus override from [`Self::model`],
    /// then fall back to a best-effort read of the backend's own config.
    fn display_config(&self) -> AgentDisplayConfig {
        AgentDisplayConfig::from_model(self.model())
    }

    /// Describes how headless prompt text should be delivered.
    fn headless_prompt_transport(&self) -> HeadlessPromptTransport {
        HeadlessPromptTransport::Argv
    }

    /// Validates backend-specific files/settings needed before HQ leaves the dashboard
    /// for an interactive session.
    fn validate_interactive_launch(&self, _role: &str, _index: u32) -> Result<()> {
        Ok(())
    }
}

/// Behavior required from an executor-capable agent implementation.
///
/// Executors mirror the supervisor API because HQ may start them in interactive
/// or headless modes depending on the orchestration context.
pub trait ExecutorAgent: Send + Sync {
    /// Returns the stable configuration name for this agent backend.
    fn name(&self) -> &'static str;

    /// Builds the command used when a human or HQ drives the executor.
    ///
    /// # Errors
    ///
    /// Returns an error when Ferrus cannot resolve the launcher command for
    /// the selected backend and mode.
    fn spawn(&self, mode: AgentRunMode<'_>) -> Result<Command> {
        self.spawn_with_index(mode, DEFAULT_AGENT_INDEX)
    }

    /// Builds the command used for a specific role-scoped MCP server index.
    fn spawn_with_index(&self, mode: AgentRunMode<'_>, index: u32) -> Result<Command>;

    /// Builds a command that returns the backend version string.
    ///
    /// The default implementation reuses the interactive launcher shape and
    /// appends `--version`, which works for regular CLIs and wrapper launchers
    /// like `node <script>`.
    fn version_command(&self) -> Result<Command> {
        let mut cmd = self.spawn(AgentRunMode::Interactive { prompt: None })?;
        cmd.arg("--version");
        Ok(cmd)
    }

    /// Builds the MCP configuration entry for this executor instance.
    ///
    /// # Errors
    ///
    /// Returns an error when Ferrus cannot resolve the path to the current
    /// executable.
    fn mcp_config_entry(&self, role: &str, index: u32) -> Result<McpConfigEntry> {
        Ok(McpConfigEntry {
            command: current_exe_string()?,
            args: serve_args(role, self.name(), index),
            model: self.model().map(ToOwned::to_owned),
        })
    }

    /// Returns the optional model override used by this backend.
    fn model(&self) -> Option<&str>;

    /// Returns the model and inference settings HQ should display for this backend.
    ///
    /// Implementations should prefer the Ferrus override from [`Self::model`],
    /// then fall back to a best-effort read of the backend's own config.
    fn display_config(&self) -> AgentDisplayConfig {
        AgentDisplayConfig::from_model(self.model())
    }

    /// Describes how headless prompt text should be delivered.
    fn headless_prompt_transport(&self) -> HeadlessPromptTransport {
        HeadlessPromptTransport::Argv
    }

    /// Validates backend-specific files/settings needed before HQ leaves the dashboard
    /// for an interactive session.
    fn validate_interactive_launch(&self, _role: &str, _index: u32) -> Result<()> {
        Ok(())
    }
}

/// Parses a configured supervisor agent name into its concrete implementation.
///
/// # Errors
///
/// Returns an error that lists the supported agent names when `agent_type` does
/// not match a registered supervisor backend.
pub fn parse_supervisor_agent(
    agent_type: &str,
    model: Option<&str>,
) -> Result<Arc<dyn SupervisorAgent>> {
    match agent_type {
        claude::NAME => Ok(Arc::new(claude::Supervisor::new(
            model,
            crate::config::load_claude_mcp_isolation(),
        ))),
        codex::NAME => Ok(Arc::new(codex::Supervisor::new(model))),
        goose::NAME => Ok(Arc::new(goose::Supervisor::new(model))),
        opencode::NAME => Ok(Arc::new(opencode::Supervisor::new(model))),
        qwen::NAME => Ok(Arc::new(qwen::Supervisor::new(model))),
        other => bail!(
            "Unknown supervisor agent '{other}'. Supported values: \"claude-code\", \"codex\", \"goose\", \"opencode\", \"qwen-code\"."
        ),
    }
}

/// Parses a configured executor agent name into its concrete implementation.
///
/// # Errors
///
/// Returns an error that lists the supported agent names when `agent_type` does
/// not match a registered executor backend.
pub fn parse_executor_agent(
    agent_type: &str,
    model: Option<&str>,
) -> Result<Arc<dyn ExecutorAgent>> {
    match agent_type {
        claude::NAME => Ok(Arc::new(claude::Executor::new(
            model,
            crate::config::load_claude_mcp_isolation(),
        ))),
        codex::NAME => Ok(Arc::new(codex::Executor::new(model))),
        goose::NAME => Ok(Arc::new(goose::Executor::new(model))),
        opencode::NAME => Ok(Arc::new(opencode::Executor::new(model))),
        qwen::NAME => Ok(Arc::new(qwen::Executor::new(model))),
        other => bail!(
            "Unknown executor agent '{other}'. Supported values: \"claude-code\", \"codex\", \"goose\", \"opencode\", \"qwen-code\"."
        ),
    }
}

pub(crate) fn current_exe_string() -> Result<String> {
    // Persist the exact executable path so generated MCP configs keep working
    // even when Ferrus is launched outside of PATH-based resolution.
    Ok(std::env::current_exe()
        .context("Failed to resolve current executable path")?
        .to_string_lossy()
        .into_owned())
}

pub(crate) fn serve_args(role: &str, agent_name: &str, _index: u32) -> Vec<String> {
    // Ferrus reconnects to agents through `ferrus serve`, so every backend uses
    // the same role-level argument shape. Concrete agent/task/run identity is
    // supplied at runtime through FERRUS_* environment variables.
    vec![
        "serve".to_string(),
        "--role".to_string(),
        role.to_string(),
        "--agent-name".to_string(),
        agent_name.to_string(),
    ]
}

pub(crate) fn normalized_model(model: Option<&str>) -> Option<String> {
    model.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

pub(crate) fn json_config_display_from_paths(
    paths: impl IntoIterator<Item = PathBuf>,
) -> AgentDisplayConfig {
    paths
        .into_iter()
        .find_map(|path| {
            let config = json_config_display_from_path(&path)?;
            (!config.is_empty()).then_some(config)
        })
        .unwrap_or_default()
}

fn json_config_display_from_path(path: &Path) -> Option<AgentDisplayConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    let root: Value = serde_json::from_str(&content).ok()?;
    Some(AgentDisplayConfig {
        model: normalized_model(root.get("model").and_then(Value::as_str)),
        effort: json_config_effort_from_value(&root),
    })
}

fn json_config_effort_from_value(root: &Value) -> Option<String> {
    [
        "effort",
        "effortLevel",
        "reasoning_effort",
        "modelReasoningEffort",
        "model_reasoning_effort",
    ]
    .into_iter()
    .find_map(|key| root.get(key).and_then(Value::as_str))
    .and_then(|value| normalized_model(Some(value)))
}

pub(crate) fn toml_config_display_from_paths(
    paths: impl IntoIterator<Item = PathBuf>,
) -> AgentDisplayConfig {
    paths
        .into_iter()
        .find_map(|path| {
            let config = toml_config_display_from_path(&path)?;
            (!config.is_empty()).then_some(config)
        })
        .unwrap_or_default()
}

fn toml_config_display_from_path(path: &Path) -> Option<AgentDisplayConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    let root = content.parse::<toml::Table>().ok()?;
    Some(toml_config_display_from_table(&root))
}

fn toml_config_display_from_table(root: &toml::Table) -> AgentDisplayConfig {
    let profile_config = root
        .get("profile")
        .and_then(toml::Value::as_str)
        .and_then(|value| normalized_model(Some(value)))
        .and_then(|profile| {
            root.get("profiles")
                .and_then(toml::Value::as_table)
                .and_then(|profiles| profiles.get(&profile))
                .and_then(toml::Value::as_table)
                .map(toml_config_display_from_table)
        });

    let root_config = AgentDisplayConfig {
        model: root
            .get("model")
            .and_then(toml::Value::as_str)
            .and_then(|value| normalized_model(Some(value))),
        effort: toml_config_effort_from_table(root),
    };
    profile_config
        .map(|profile| profile.merge_missing(root_config.clone()))
        .unwrap_or(root_config)
}

fn toml_config_effort_from_table(root: &toml::Table) -> Option<String> {
    [
        "model_reasoning_effort",
        "reasoning_effort",
        "effort",
        "effortLevel",
    ]
    .into_iter()
    .find_map(|key| root.get(key))
    .and_then(toml::Value::as_str)
    .and_then(|value| normalized_model(Some(value)))
}

pub(crate) async fn allow_mcp_server_tools_in_json_settings(
    path: &Path,
    server_key: &str,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let mut root: Value = if path.exists() {
        let content = tokio::fs::read_to_string(path).await?;
        serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))?
    } else {
        serde_json::json!({})
    };

    let permission = mcp_server_tools_permission(server_key);
    let added = add_json_allow_permission(&mut root, &permission, path)?;
    let content = serde_json::to_string_pretty(&root)?;
    tokio::fs::write(path, content).await?;
    if added {
        println!("Allowed {permission} in {}", path.display());
    }
    Ok(())
}

pub(crate) fn invalid_mcp_config(message: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("Invalid MCP configuration:\n{message}")
}

pub(crate) fn absolute_display_path(path: &Path) -> std::path::PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(path)
    }
}

pub(crate) fn display_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

pub(crate) fn absolute_display_path_string(path: &Path) -> String {
    display_path(&absolute_display_path(path))
}

pub(crate) fn ensure_mcp_config_file_exists(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!(invalid_mcp_config(format!(
            "MCP config file not found: {}",
            absolute_display_path_string(path)
        )));
    }
    Ok(())
}

pub(crate) fn validate_json_mcp_server(path: &Path, key: &str) -> Result<()> {
    ensure_mcp_config_file_exists(path)?;
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", display_path(path)))?;
    let root: Value = serde_json::from_str(&content).map_err(|err| {
        invalid_mcp_config(format!("Failed to parse {}: {err}", display_path(path)))
    })?;
    let servers = root
        .get("mcpServers")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            invalid_mcp_config(format!(
                "{} mcpServers is not an object",
                display_path(path)
            ))
        })?;
    if !servers.contains_key(key) {
        bail!(invalid_mcp_config(format!(
            "MCP server `{key}` not found in {}",
            display_path(path)
        )));
    }
    Ok(())
}

pub(crate) fn validate_toml_mcp_server(path: &Path, key: &str) -> Result<()> {
    ensure_mcp_config_file_exists(path)?;
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", display_path(path)))?;
    let root: toml::Value = toml::from_str(&content).map_err(|err| {
        invalid_mcp_config(format!("Failed to parse {}: {err}", display_path(path)))
    })?;
    let servers = root
        .get("mcp_servers")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| {
            invalid_mcp_config(format!("{} mcp_servers is not a table", display_path(path)))
        })?;
    if !servers.contains_key(key) {
        bail!(invalid_mcp_config(format!(
            "MCP server `{key}` not found in {}",
            display_path(path)
        )));
    }
    Ok(())
}

fn mcp_server_tools_permission(server_key: &str) -> String {
    format!("mcp__{server_key}__*")
}

fn add_json_allow_permission(root: &mut Value, permission: &str, path: &Path) -> Result<bool> {
    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} root is not a JSON object", path.display()))?;

    let permissions = root_obj
        .entry("permissions")
        .or_insert_with(|| serde_json::json!({}));
    let permissions_obj = permissions
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} permissions is not an object", path.display()))?;

    let allow = permissions_obj
        .entry("allow")
        .or_insert_with(|| serde_json::json!([]));
    let allow_array = allow
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("{} permissions.allow is not an array", path.display()))?;

    if allow_array
        .iter()
        .any(|value| value.as_str() == Some(permission))
    {
        return Ok(false);
    }
    if allow_array.iter().any(|value| !value.is_string()) {
        bail!(
            "{} permissions.allow must contain only strings",
            path.display()
        );
    }

    allow_array.push(Value::String(permission.to_string()));
    Ok(true)
}

#[cfg(test)]
mod tests {
    //! Agent selection errors, model normalization, and display configuration.

    use super::*;

    pub(crate) fn assert_program_and_args(command: Command, program: &str, args: &[&str]) {
        assert_eq!(command.get_program().to_string_lossy(), program);
        let actual = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let expected = args
            .iter()
            .map(|arg| (*arg).to_string())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    #[test]
    fn unknown_supervisor_agent_is_actionable() {
        let err = match parse_supervisor_agent("unknown", None) {
            Ok(_) => panic!("expected unknown supervisor agent to fail"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("Unknown supervisor agent 'unknown'"));
        assert!(err.contains("claude-code"));
        assert!(err.contains("codex"));
        assert!(err.contains("goose"));
        assert!(err.contains("opencode"));
        assert!(err.contains("qwen-code"));
    }

    #[test]
    fn unknown_executor_agent_is_actionable() {
        let err = match parse_executor_agent("unknown", None) {
            Ok(_) => panic!("expected unknown executor agent to fail"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("Unknown executor agent 'unknown'"));
        assert!(err.contains("claude-code"));
        assert!(err.contains("codex"));
        assert!(err.contains("goose"));
        assert!(err.contains("opencode"));
        assert!(err.contains("qwen-code"));
    }

    #[test]
    fn blank_model_is_normalized_to_none() {
        assert_eq!(normalized_model(None), None);
        assert_eq!(normalized_model(Some("")), None);
        assert_eq!(normalized_model(Some("  ")), None);
        assert_eq!(
            normalized_model(Some("gpt-5.4")),
            Some("gpt-5.4".to_string())
        );
    }

    #[test]
    fn json_config_display_reads_top_level_model_and_effort() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"model":"claude-sonnet-4-5","effortLevel":"high"}"#,
        )
        .unwrap();

        let config = json_config_display_from_paths([path]);
        assert_eq!(config.model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(config.effort.as_deref(), Some("high"));
    }

    #[test]
    fn toml_config_display_prefers_selected_profile_model_and_effort() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"model = "gpt-5"
model_reasoning_effort = "medium"
profile = "work"

[profiles.work]
model = "gpt-5.4"
model_reasoning_effort = "high"
"#,
        )
        .unwrap();

        let config = toml_config_display_from_paths([path]);
        assert_eq!(config.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(config.effort.as_deref(), Some("high"));
    }

    #[test]
    fn toml_config_display_falls_back_to_top_level_model() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, r#"model = "gpt-5""#).unwrap();

        let config = toml_config_display_from_paths([path]);
        assert_eq!(config.model.as_deref(), Some("gpt-5"));
        assert_eq!(config.effort, None);
    }

    #[test]
    fn mcp_permission_uses_mcp_server_wildcard() {
        assert_eq!(
            mcp_server_tools_permission("ferrus-supervisor"),
            "mcp__ferrus-supervisor__*"
        );
    }

    #[test]
    fn add_json_allow_permission_preserves_existing_entries() {
        let mut root = serde_json::json!({
            "permissions": {
                "allow": ["Bash(cargo test)"]
            }
        });

        let added = add_json_allow_permission(
            &mut root,
            "mcp__ferrus-executor__*",
            Path::new(".claude/settings.local.json"),
        )
        .unwrap();
        assert!(added);
        assert_eq!(
            root["permissions"]["allow"],
            serde_json::json!(["Bash(cargo test)", "mcp__ferrus-executor__*"])
        );
    }

    #[test]
    fn add_json_allow_permission_is_idempotent() {
        let mut root = serde_json::json!({
            "permissions": {
                "allow": ["mcp__ferrus-supervisor__*"]
            }
        });

        let added = add_json_allow_permission(
            &mut root,
            "mcp__ferrus-supervisor__*",
            Path::new(".qwen/settings.json"),
        )
        .unwrap();
        assert!(!added);
        assert_eq!(
            root["permissions"]["allow"],
            serde_json::json!(["mcp__ferrus-supervisor__*"])
        );
    }
}
