//! opencode-backed supervisor and executor adapters.
//!
//! opencode launches an interactive TUI by default and runs headlessly through
//! its `run` subcommand. Ferrus normalizes those conventions here so the rest of
//! the orchestration layer can treat it like any other agent backend. opencode
//! stores project configuration (including MCP servers) in `opencode.json` and
//! reads `AGENTS.md` for repository instructions.

use super::{
    AgentDisplayConfig, AgentRunMode, ExecutorAgent, SupervisorAgent, display_path,
    ensure_mcp_config_file_exists, invalid_mcp_config, json_config_display_from_paths,
    normalized_model,
};
use crate::agent_id::mcp_server_name;
use anyhow::{Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Stable agent identifier used in Ferrus configuration and error messages.
pub(crate) const NAME: &str = "opencode";
/// Actual CLI executable name used to launch opencode.
const EXECUTABLE: &str = "opencode";
/// Project-local opencode configuration file.
const CONFIG_FILE: &str = "opencode.json";

/// Interactive and headless supervisor launcher for opencode.
#[derive(Debug, Clone)]
pub struct Supervisor {
    model: Option<String>,
}

/// Interactive and headless executor launcher for opencode.
#[derive(Debug, Clone)]
pub struct Executor {
    model: Option<String>,
}

impl Supervisor {
    pub fn new(model: Option<&str>) -> Self {
        Self {
            model: normalized_model(model),
        }
    }
}

impl Executor {
    pub fn new(model: Option<&str>) -> Self {
        Self {
            model: normalized_model(model),
        }
    }
}

impl SupervisorAgent for Supervisor {
    /// Returns the Ferrus-visible identifier for the opencode supervisor backend.
    fn name(&self) -> &'static str {
        NAME
    }

    /// Builds the opencode command used by Ferrus HQ or an interactive user.
    fn spawn_with_index(&self, mode: AgentRunMode<'_>, _index: u32) -> Result<Command> {
        Ok(opencode_command(mode, self.model()))
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn display_config(&self) -> AgentDisplayConfig {
        AgentDisplayConfig::from_model(self.model()).merge_missing(opencode_config_display())
    }

    fn validate_interactive_launch(&self, role: &str, _index: u32) -> Result<()> {
        validate_interactive_launch(role)
    }
}

impl ExecutorAgent for Executor {
    /// Returns the Ferrus-visible identifier for the opencode executor backend.
    fn name(&self) -> &'static str {
        NAME
    }

    /// Builds the opencode command used by Ferrus HQ or an interactive user.
    fn spawn_with_index(&self, mode: AgentRunMode<'_>, _index: u32) -> Result<Command> {
        Ok(opencode_command(mode, self.model()))
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn display_config(&self) -> AgentDisplayConfig {
        AgentDisplayConfig::from_model(self.model()).merge_missing(opencode_config_display())
    }

    fn validate_interactive_launch(&self, role: &str, _index: u32) -> Result<()> {
        validate_interactive_launch(role)
    }
}

#[inline(always)]
fn opencode_command(mode: AgentRunMode<'_>, model: Option<&str>) -> Command {
    let mut cmd = Command::new(EXECUTABLE);
    match mode {
        AgentRunMode::Interactive { prompt } => {
            // Interactive sessions start the opencode TUI; a seed prompt is optional.
            if let Some(model) = model {
                cmd.arg("--model").arg(model);
            }
            if let Some(prompt) = prompt {
                cmd.arg("--prompt").arg(prompt);
            }
        }
        AgentRunMode::Headless { prompt } => {
            // Headless sessions use the `run` subcommand, which prints and exits.
            cmd.arg("run");
            if let Some(model) = model {
                cmd.arg("--model").arg(model);
            }
            cmd.arg(prompt);
        }
    }
    cmd
}

/// Project-local opencode configuration path.
pub(crate) fn opencode_config_path() -> &'static Path {
    Path::new(CONFIG_FILE)
}

fn opencode_config_paths() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from(CONFIG_FILE), PathBuf::from("opencode.jsonc")];
    if let Some(config_dir) = dirs::config_dir() {
        paths.push(config_dir.join("opencode").join(CONFIG_FILE));
        paths.push(config_dir.join("opencode").join("opencode.jsonc"));
    }
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".opencode.json"));
    }
    paths
}

fn opencode_config_display() -> AgentDisplayConfig {
    json_config_display_from_paths(opencode_config_paths())
}

fn validate_interactive_launch(role: &str) -> Result<()> {
    let path = opencode_config_path();
    ensure_mcp_config_file_exists(path)?;
    let content = std::fs::read_to_string(path).map_err(|err| {
        invalid_mcp_config(format!("Failed to read {}: {err}", display_path(path)))
    })?;
    let root: Value = serde_json::from_str(&content).map_err(|err| {
        invalid_mcp_config(format!("Failed to parse {}: {err}", display_path(path)))
    })?;
    let servers = root.get("mcp").and_then(Value::as_object).ok_or_else(|| {
        invalid_mcp_config(format!("{} mcp is not an object", display_path(path)))
    })?;
    let key = mcp_server_name(role);
    if !servers.contains_key(&key) {
        bail!(invalid_mcp_config(format!(
            "MCP server `{key}` not found in {}",
            display_path(path)
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! OpenCode launch arguments, model overrides, and role configuration checks.

    use super::*;
    use crate::agents::tests::assert_program_and_args;

    #[test]
    fn opencode_supervisor_builds_interactive_command() {
        let agent = Supervisor::new(None);
        assert_program_and_args(
            agent
                .spawn(AgentRunMode::Interactive {
                    prompt: Some("plan"),
                })
                .unwrap(),
            "opencode",
            &["--prompt", "plan"],
        );
    }

    #[test]
    fn opencode_executor_builds_headless_command() {
        let agent = Executor::new(None);
        assert_program_and_args(
            agent
                .spawn(AgentRunMode::Headless { prompt: "run" })
                .unwrap(),
            "opencode",
            &["run", "run"],
        );
    }

    #[test]
    fn opencode_model_override_is_part_of_spawned_command() {
        let agent = Executor::new(Some("anthropic/claude-sonnet-4-5"));
        assert_program_and_args(
            agent
                .spawn(AgentRunMode::Headless { prompt: "review" })
                .unwrap(),
            "opencode",
            &["run", "--model", "anthropic/claude-sonnet-4-5", "review"],
        );
    }

    #[test]
    fn opencode_config_entry_uses_expected_args() {
        let entry = Supervisor::new(Some("ollama/qwen3-coder"))
            .mcp_config_entry("supervisor", 2)
            .unwrap();
        assert!(!entry.command.is_empty());
        assert_eq!(
            entry.args,
            vec!["serve", "--role", "supervisor", "--agent-name", "opencode"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(entry.model.as_deref(), Some("ollama/qwen3-coder"));
    }

    #[test]
    fn opencode_interactive_preflight_reports_missing_role_server() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        std::fs::write("opencode.json", r#"{"mcp":{}}"#).unwrap();
        let agent = Executor::new(None);

        let err = agent
            .validate_interactive_launch(crate::agent_id::ROLE_EXECUTOR, 1)
            .unwrap_err();
        let message = err.to_string();

        assert!(message.contains("MCP server `ferrus-executor` not found"));
        std::env::set_current_dir(previous).unwrap();
    }

    #[test]
    fn opencode_interactive_preflight_accepts_registered_role_server() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        std::fs::write(
            "opencode.json",
            r#"{"mcp":{"ferrus-executor":{"type":"local","command":["ferrus"],"enabled":true}}}"#,
        )
        .unwrap();
        let agent = Executor::new(None);

        agent
            .validate_interactive_launch(crate::agent_id::ROLE_EXECUTOR, 1)
            .unwrap();
        std::env::set_current_dir(previous).unwrap();
    }
}
