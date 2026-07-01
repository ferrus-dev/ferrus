//! Claude Code-backed supervisor and executor adapters.
//!
//! Ferrus uses this module to normalize Claude's CLI conventions so the rest of
//! the orchestration layer can treat it like any other agent backend.

use super::{
    AgentRunMode, ExecutorAgent, SupervisorAgent, allow_mcp_server_tools_in_json_settings,
    normalized_model, validate_json_mcp_server,
};
use crate::agent_id::{ROLE_EXECUTOR, ROLE_SUPERVISOR, legacy_mcp_server_name, mcp_server_name};
use crate::config::ClaudeMcpIsolation;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Stable agent identifier used in Ferrus configuration and error messages.
pub(crate) const NAME: &str = "claude-code";
/// Actual CLI executable name used to launch Claude Code.
const EXECUTABLE: &str = "claude";

/// Interactive and headless supervisor launcher for Claude Code.
#[derive(Debug, Clone)]
pub struct Supervisor {
    model: Option<String>,
    mcp_isolation: ClaudeMcpIsolation,
}

/// Interactive and headless executor launcher for Claude Code.
#[derive(Debug, Clone)]
pub struct Executor {
    model: Option<String>,
    mcp_isolation: ClaudeMcpIsolation,
}

impl Supervisor {
    pub fn new(model: Option<&str>, mcp_isolation: ClaudeMcpIsolation) -> Self {
        Self {
            model: normalized_model(model),
            mcp_isolation,
        }
    }
}

impl Executor {
    pub fn new(model: Option<&str>, mcp_isolation: ClaudeMcpIsolation) -> Self {
        Self {
            model: normalized_model(model),
            mcp_isolation,
        }
    }
}

impl SupervisorAgent for Supervisor {
    /// Returns the Ferrus-visible identifier for the Claude supervisor backend.
    fn name(&self) -> &'static str {
        NAME
    }

    /// Builds the Claude command used by Ferrus HQ or an interactive user.
    fn spawn_with_index(&self, mode: AgentRunMode<'_>, _index: u32) -> Result<Command> {
        Ok(claude_command(
            ROLE_SUPERVISOR,
            mode,
            self.model(),
            self.mcp_isolation,
        ))
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn validate_interactive_launch(&self, role: &str, index: u32) -> Result<()> {
        validate_interactive_launch(role, index)
    }
}

impl ExecutorAgent for Executor {
    /// Returns the Ferrus-visible identifier for the Claude executor backend.
    fn name(&self) -> &'static str {
        NAME
    }

    /// Builds the Claude command used by Ferrus HQ or an interactive user.
    fn spawn_with_index(&self, mode: AgentRunMode<'_>, _index: u32) -> Result<Command> {
        Ok(claude_command(
            ROLE_EXECUTOR,
            mode,
            self.model(),
            self.mcp_isolation,
        ))
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn validate_interactive_launch(&self, role: &str, index: u32) -> Result<()> {
        validate_interactive_launch(role, index)
    }
}

#[inline(always)]
fn claude_command(
    role: &str,
    mode: AgentRunMode<'_>,
    model: Option<&str>,
    isolation: ClaudeMcpIsolation,
) -> Command {
    let mut cmd = Command::new(EXECUTABLE);
    cmd.arg("--mcp-config")
        .arg(claude_role_mcp_config_path(role));
    if isolation == ClaudeMcpIsolation::FerrusOnly {
        cmd.arg("--strict-mcp-config");
    }
    if let Some(model) = model {
        cmd.arg("--model").arg(model);
    }
    match mode {
        AgentRunMode::Interactive { prompt } => {
            if let Some(prompt) = prompt {
                cmd.arg("--").arg(prompt);
            }
        }
        AgentRunMode::Headless { prompt } => {
            // Claude's print-and-exit flow is exposed as `-p`, so Ferrus uses that
            // form to run headless tasks without opening an interactive TUI.
            cmd.arg("-p").arg(prompt);
        }
    }
    cmd
}

pub(crate) async fn allow_mcp_server_tools(server_key: &str) -> Result<()> {
    allow_mcp_server_tools_in_json_settings(
        std::path::Path::new(".claude/settings.local.json"),
        server_key,
    )
    .await
}

pub(crate) fn claude_role_mcp_config_path(role: &str) -> PathBuf {
    Path::new(".claude").join(format!("mcp-{role}.json"))
}

fn validate_interactive_launch(role: &str, index: u32) -> Result<()> {
    let path = claude_role_mcp_config_path(role);
    let primary = mcp_server_name(role);
    match validate_json_mcp_server(&path, &primary) {
        Ok(()) => Ok(()),
        Err(primary_err) => {
            let legacy = legacy_mcp_server_name(role, index);
            validate_json_mcp_server(&path, &legacy).map_err(|_| primary_err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::tests::assert_program_and_args;

    #[test]
    fn claude_supervisor_builds_interactive_command() {
        let agent = Supervisor::new(None, ClaudeMcpIsolation::MergeUser);
        let role_config = claude_role_mcp_config_path(ROLE_SUPERVISOR)
            .to_string_lossy()
            .into_owned();
        assert_program_and_args(
            agent
                .spawn(AgentRunMode::Interactive {
                    prompt: Some("plan"),
                })
                .unwrap(),
            "claude",
            &["--mcp-config", &role_config, "--", "plan"],
        );
    }

    #[test]
    fn claude_executor_builds_headless_command() {
        let agent = Executor::new(None, ClaudeMcpIsolation::MergeUser);
        let role_config = claude_role_mcp_config_path(ROLE_EXECUTOR)
            .to_string_lossy()
            .into_owned();
        assert_program_and_args(
            agent
                .spawn(AgentRunMode::Headless { prompt: "run" })
                .unwrap(),
            "claude",
            &["--mcp-config", &role_config, "-p", "run"],
        );
    }

    #[test]
    fn claude_model_override_is_part_of_spawned_command() {
        let agent = Supervisor::new(Some("claude-opus-4-6"), ClaudeMcpIsolation::MergeUser);
        let role_config = claude_role_mcp_config_path(ROLE_SUPERVISOR)
            .to_string_lossy()
            .into_owned();
        assert_program_and_args(
            agent
                .spawn(AgentRunMode::Headless { prompt: "review" })
                .unwrap(),
            "claude",
            &[
                "--mcp-config",
                &role_config,
                "--model",
                "claude-opus-4-6",
                "-p",
                "review",
            ],
        );
    }

    #[test]
    fn claude_ferrus_only_mode_adds_strict_mcp_config() {
        let role_config = claude_role_mcp_config_path(ROLE_SUPERVISOR)
            .to_string_lossy()
            .into_owned();
        assert_program_and_args(
            claude_command(
                ROLE_SUPERVISOR,
                AgentRunMode::Headless { prompt: "review" },
                None,
                ClaudeMcpIsolation::FerrusOnly,
            ),
            "claude",
            &[
                "--mcp-config",
                &role_config,
                "--strict-mcp-config",
                "-p",
                "review",
            ],
        );
    }

    #[test]
    fn claude_role_mcp_config_paths_are_role_scoped() {
        assert_eq!(
            claude_role_mcp_config_path(ROLE_SUPERVISOR),
            Path::new(".claude/mcp-supervisor.json")
        );
        assert_eq!(
            claude_role_mcp_config_path(ROLE_EXECUTOR),
            Path::new(".claude/mcp-executor.json")
        );
    }

    #[test]
    fn claude_config_entry_uses_expected_args() {
        let entry = Supervisor::new(Some("claude-opus-4-6"), ClaudeMcpIsolation::MergeUser)
            .mcp_config_entry("supervisor", 2)
            .unwrap();
        assert!(!entry.command.is_empty());
        assert_eq!(
            entry.args,
            vec![
                "serve",
                "--role",
                "supervisor",
                "--agent-name",
                "claude-code",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
        assert_eq!(entry.model.as_deref(), Some("claude-opus-4-6"));
    }

    #[test]
    fn claude_executor_config_entry_uses_expected_args() {
        let entry = Executor::new(None, ClaudeMcpIsolation::MergeUser)
            .mcp_config_entry("executor", 4)
            .unwrap();
        assert!(!entry.command.is_empty());
        assert_eq!(
            entry.args,
            vec!["serve", "--role", "executor", "--agent-name", "claude-code",]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(entry.model, None);
    }

    #[test]
    fn claude_interactive_preflight_reports_missing_role_config() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let agent = Supervisor::new(None, ClaudeMcpIsolation::MergeUser);

        let err = agent
            .validate_interactive_launch(ROLE_SUPERVISOR, 1)
            .unwrap_err();
        let message = err.to_string();

        assert!(message.contains("Invalid MCP configuration"));
        assert!(message.contains(".claude/mcp-supervisor.json"));
        std::env::set_current_dir(previous).unwrap();
    }

    #[test]
    fn claude_interactive_preflight_accepts_registered_role_server() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        std::fs::create_dir_all(".claude").unwrap();
        std::fs::write(
            ".claude/mcp-supervisor.json",
            r#"{"mcpServers":{"ferrus-supervisor":{"command":"ferrus","args":[]}}}"#,
        )
        .unwrap();
        let agent = Supervisor::new(None, ClaudeMcpIsolation::MergeUser);

        agent
            .validate_interactive_launch(ROLE_SUPERVISOR, 1)
            .unwrap();
        std::env::set_current_dir(previous).unwrap();
    }
}
