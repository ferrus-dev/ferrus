//! Qwen Code-backed supervisor and executor adapters.
//!
//! Ferrus uses this module to normalize Qwen's CLI conventions so the rest of
//! the orchestration layer can treat it like any other agent backend.

use super::{
    AgentRunMode, ExecutorAgent, SupervisorAgent, allow_mcp_server_tools_in_json_settings,
    normalized_model, validate_json_mcp_server,
};
use crate::agent_id::{legacy_mcp_server_name, mcp_server_name};
use anyhow::Result;
use std::process::Command;

/// Stable agent identifier used in Ferrus configuration and error messages.
pub(crate) const NAME: &str = "qwen-code";
/// Actual CLI executable name used to launch Qwen Code.
const EXECUTABLE: &str = "qwen";

/// Interactive and headless supervisor launcher for Qwen Code.
#[derive(Debug, Clone)]
pub struct Supervisor {
    model: Option<String>,
}

/// Interactive and headless executor launcher for Qwen Code.
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
    /// Returns the Ferrus-visible identifier for the Qwen supervisor backend.
    fn name(&self) -> &'static str {
        NAME
    }

    /// Builds the Qwen command used by Ferrus HQ or an interactive user.
    fn spawn_with_index(&self, mode: AgentRunMode<'_>, _index: u32) -> Result<Command> {
        Ok(qwen_command(mode, self.model()))
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn validate_interactive_launch(&self, role: &str, index: u32) -> Result<()> {
        validate_interactive_launch(role, index)
    }
}

impl ExecutorAgent for Executor {
    /// Returns the Ferrus-visible identifier for the Qwen executor backend.
    fn name(&self) -> &'static str {
        NAME
    }

    /// Builds the Qwen command used by Ferrus HQ or an interactive user.
    fn spawn_with_index(&self, mode: AgentRunMode<'_>, _index: u32) -> Result<Command> {
        Ok(qwen_command(mode, self.model()))
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn validate_interactive_launch(&self, role: &str, index: u32) -> Result<()> {
        validate_interactive_launch(role, index)
    }
}

#[inline(always)]
fn qwen_command(mode: AgentRunMode<'_>, model: Option<&str>) -> Command {
    let mut cmd = Command::new(EXECUTABLE);
    if let Some(model) = model {
        cmd.arg("--model").arg(model);
    }
    match mode {
        AgentRunMode::Interactive { prompt } => {
            if let Some(prompt) = prompt {
                cmd.arg("-i").arg(prompt);
            }
        }
        AgentRunMode::Headless { prompt } => {
            // Qwen follows Claude's print-and-exit pattern via `-p`.
            cmd.arg("-p").arg(prompt);
        }
    }
    cmd
}

pub(crate) async fn allow_mcp_server_tools(server_key: &str) -> Result<()> {
    allow_mcp_server_tools_in_json_settings(std::path::Path::new(".qwen/settings.json"), server_key)
        .await
}

fn validate_interactive_launch(role: &str, index: u32) -> Result<()> {
    let path = std::path::Path::new(".qwen/settings.json");
    let primary = mcp_server_name(role);
    match validate_json_mcp_server(path, &primary) {
        Ok(()) => Ok(()),
        Err(primary_err) => {
            let legacy = legacy_mcp_server_name(role, index);
            validate_json_mcp_server(path, &legacy).map_err(|_| primary_err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::tests::assert_program_and_args;

    #[test]
    fn qwen_supervisor_builds_interactive_command() {
        let agent = Supervisor::new(None);
        assert_program_and_args(
            agent
                .spawn(AgentRunMode::Interactive {
                    prompt: Some("plan"),
                })
                .unwrap(),
            "qwen",
            &["-i", "plan"],
        );
    }

    #[test]
    fn qwen_executor_builds_headless_command() {
        let agent = Executor::new(None);
        assert_program_and_args(
            agent
                .spawn(AgentRunMode::Headless { prompt: "run" })
                .unwrap(),
            "qwen",
            &["-p", "run"],
        );
    }

    #[test]
    fn qwen_model_override_is_part_of_spawned_command() {
        let agent = Supervisor::new(Some("qwen3-coder-plus"));
        assert_program_and_args(
            agent
                .spawn(AgentRunMode::Headless { prompt: "review" })
                .unwrap(),
            "qwen",
            &["--model", "qwen3-coder-plus", "-p", "review"],
        );
    }

    #[test]
    fn qwen_config_entry_uses_expected_args() {
        let entry = Supervisor::new(Some("qwen3-coder-plus"))
            .mcp_config_entry("supervisor", 2)
            .unwrap();
        assert!(!entry.command.is_empty());
        assert_eq!(
            entry.args,
            vec!["serve", "--role", "supervisor", "--agent-name", "qwen-code",]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(entry.model.as_deref(), Some("qwen3-coder-plus"));
    }

    #[test]
    fn qwen_executor_config_entry_uses_expected_args() {
        let entry = Executor::new(None).mcp_config_entry("executor", 4).unwrap();
        assert!(!entry.command.is_empty());
        assert_eq!(
            entry.args,
            vec!["serve", "--role", "executor", "--agent-name", "qwen-code",]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(entry.model, None);
    }

    #[test]
    fn qwen_interactive_preflight_reports_missing_role_server() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        std::fs::create_dir_all(".qwen").unwrap();
        std::fs::write(".qwen/settings.json", r#"{"mcpServers":{}}"#).unwrap();
        let agent = Executor::new(None);

        let err = agent
            .validate_interactive_launch(crate::agent_id::ROLE_EXECUTOR, 1)
            .unwrap_err();
        let message = err.to_string();

        assert!(message.contains("MCP server `ferrus-executor` not found"));
        std::env::set_current_dir(previous).unwrap();
    }

    #[test]
    fn qwen_interactive_preflight_accepts_registered_role_server() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        std::fs::create_dir_all(".qwen").unwrap();
        std::fs::write(
            ".qwen/settings.json",
            r#"{"mcpServers":{"ferrus-executor":{"command":"ferrus","args":[]}}}"#,
        )
        .unwrap();
        let agent = Executor::new(None);

        agent
            .validate_interactive_launch(crate::agent_id::ROLE_EXECUTOR, 1)
            .unwrap();
        std::env::set_current_dir(previous).unwrap();
    }
}
