//! goose-backed supervisor and executor adapters.
//!
//! goose is MCP-native: its tools come from MCP servers it calls "extensions".
//! Rather than writing a project-local config file (goose's `config.yaml` is a
//! single global file and cannot express Ferrus's per-role server scoping), this
//! adapter attaches the role-scoped Ferrus MCP server at launch with
//! `--with-extension`. The server command is `ferrus serve --role <role>`, the
//! same shape every other backend reconnects through, so nothing needs to be
//! registered ahead of time.
//!
//! Model selection uses goose's `GOOSE_MODEL` environment variable (goose has no
//! universal `--model` flag across `run` and `session`); the provider/endpoint is
//! taken from the user's goose configuration (e.g. a local LM Studio or Ollama
//! provider). Headless runs set `GOOSE_MODE=auto` so tool calls are auto-approved
//! and the run never blocks waiting for confirmation.

use super::{
    AgentDisplayConfig, AgentRunMode, ExecutorAgent, SupervisorAgent, current_exe_string,
    normalized_model, serve_args,
};
use crate::agent_id::{DEFAULT_AGENT_INDEX, ROLE_EXECUTOR, ROLE_SUPERVISOR};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Stable agent identifier used in Ferrus configuration and error messages.
pub(crate) const NAME: &str = "goose";
/// Actual CLI executable name used to launch goose.
const EXECUTABLE: &str = "goose";

/// Default ceiling on agent turns for an unattended (headless) run. Bounds a runaway
/// agent -- for example a weak local model that keeps failing to compile and never
/// reaches `/submit` -- so the run terminates instead of looping indefinitely. Override
/// it by exporting `GOOSE_MAX_TURNS` before launching Ferrus (the flag is skipped when
/// that variable is set).
const DEFAULT_MAX_TURNS: u32 = 150;
/// Default ceiling on identical consecutive tool calls. Catches tight loops where the
/// agent repeats the exact same call; complements the broader per-turn limit.
const DEFAULT_MAX_TOOL_REPETITIONS: u32 = 25;

/// Interactive and headless supervisor launcher for goose.
#[derive(Debug, Clone)]
pub struct Supervisor {
    model: Option<String>,
}

/// Interactive and headless executor launcher for goose.
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
    /// Returns the Ferrus-visible identifier for the goose supervisor backend.
    fn name(&self) -> &'static str {
        NAME
    }

    /// Builds the goose command used by Ferrus HQ or an interactive user.
    fn spawn_with_index(&self, mode: AgentRunMode<'_>, _index: u32) -> Result<Command> {
        goose_command(ROLE_SUPERVISOR, mode, self.model())
    }

    fn version_command(&self) -> Result<Command> {
        Ok(version_command())
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn display_config(&self) -> AgentDisplayConfig {
        AgentDisplayConfig::from_model(self.model()).merge_missing(goose_config_display())
    }
}

impl ExecutorAgent for Executor {
    /// Returns the Ferrus-visible identifier for the goose executor backend.
    fn name(&self) -> &'static str {
        NAME
    }

    /// Builds the goose command used by Ferrus HQ or an interactive user.
    fn spawn_with_index(&self, mode: AgentRunMode<'_>, _index: u32) -> Result<Command> {
        goose_command(ROLE_EXECUTOR, mode, self.model())
    }

    fn version_command(&self) -> Result<Command> {
        Ok(version_command())
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn display_config(&self) -> AgentDisplayConfig {
        AgentDisplayConfig::from_model(self.model()).merge_missing(goose_config_display())
    }
}

/// Builds the `--with-extension` command string that attaches the role-scoped
/// Ferrus MCP server. goose expects this as one command string, so each component
/// is shell-quoted before joining.
fn ferrus_extension_command(role: &str) -> Result<String> {
    let exe = current_exe_string()?;
    let args = serve_args(role, NAME, DEFAULT_AGENT_INDEX);
    Ok(quote_command_components(std::iter::once(exe).chain(args)))
}

fn quote_command_components(components: impl IntoIterator<Item = String>) -> String {
    components
        .into_iter()
        .map(|component| quote_command_component(&component))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(not(windows))]
fn quote_command_component(component: &str) -> String {
    if !component.is_empty()
        && component
            .bytes()
            .all(|byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.' | b'/' | b':' | b'='))
    {
        return component.to_string();
    }
    format!("'{}'", component.replace('\'', "'\"'\"'"))
}

#[cfg(windows)]
fn quote_command_component(component: &str) -> String {
    if !component.is_empty()
        && component.bytes().all(|byte| {
            matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.' | b'/' | b':' | b'\\' | b'=')
        })
    {
        return component.to_string();
    }

    let mut quoted = String::from("\"");
    let mut backslashes = 0usize;
    for ch in component.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.push_str(&"\\".repeat(backslashes));
                quoted.push(ch);
                backslashes = 0;
            }
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

fn goose_command(role: &str, mode: AgentRunMode<'_>, model: Option<&str>) -> Result<Command> {
    let extension = ferrus_extension_command(role)?;
    let mut cmd = Command::new(EXECUTABLE);
    if let Some(model) = model {
        cmd.env("GOOSE_MODEL", model);
    }
    match mode {
        AgentRunMode::Headless { prompt } => {
            // Auto-approve tool calls so an unattended run never blocks on confirmation.
            cmd.env("GOOSE_MODE", "auto");
            cmd.arg("run").arg("--no-session");
            // Loop guards so a thrashing agent fails cleanly instead of running forever.
            // `--max-turns` defers to GOOSE_MAX_TURNS when the user set it explicitly.
            if std::env::var_os("GOOSE_MAX_TURNS").is_none() {
                cmd.arg("--max-turns").arg(DEFAULT_MAX_TURNS.to_string());
            }
            cmd.arg("--max-tool-repetitions")
                .arg(DEFAULT_MAX_TOOL_REPETITIONS.to_string());
            cmd.arg("--with-extension")
                .arg(&extension)
                .arg("--text")
                .arg(prompt);
        }
        AgentRunMode::Interactive { prompt } => match prompt {
            // `run --interactive` processes the seed prompt and then drops into a chat.
            Some(prompt) => {
                cmd.arg("run")
                    .arg("--interactive")
                    .arg("--with-extension")
                    .arg(&extension)
                    .arg("--text")
                    .arg(prompt);
            }
            None => {
                cmd.arg("session").arg("--with-extension").arg(&extension);
            }
        },
    }
    Ok(cmd)
}

fn version_command() -> Command {
    // `goose --version` is independent of the subcommand shape, unlike the default
    // launcher which would append `--version` to a `run`/`session` invocation.
    let mut cmd = Command::new(EXECUTABLE);
    cmd.arg("--version");
    cmd
}

fn goose_config_display() -> AgentDisplayConfig {
    let env_config = AgentDisplayConfig {
        model: std::env::var("GOOSE_MODEL")
            .ok()
            .and_then(|value| normalized_model(Some(&value))),
        effort: None,
    };
    env_config.merge_missing(goose_config_display_from_paths(goose_config_paths()))
}

fn goose_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(config_dir) = dirs::config_dir() {
        paths.push(config_dir.join("goose").join("config.yaml"));
    }
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".config").join("goose").join("config.yaml"));
    }
    paths
}

fn goose_config_display_from_paths(paths: impl IntoIterator<Item = PathBuf>) -> AgentDisplayConfig {
    paths
        .into_iter()
        .find_map(|path| {
            let config = goose_config_display_from_path(&path)?;
            (!config.is_empty()).then_some(config)
        })
        .unwrap_or_default()
}

fn goose_config_display_from_path(path: &Path) -> Option<AgentDisplayConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    Some(goose_display_from_config_lines(content.lines()))
}

fn goose_display_from_config_lines<'a>(
    lines: impl IntoIterator<Item = &'a str>,
) -> AgentDisplayConfig {
    lines
        .into_iter()
        .fold(AgentDisplayConfig::default(), |mut config, line| {
            if config.model.is_none() {
                config.model = goose_config_value_from_line(line, "GOOSE_MODEL:")
                    .or_else(|| goose_config_value_from_line(line, "model:"));
            }
            if config.effort.is_none() {
                config.effort = goose_config_value_from_line(line, "GOOSE_REASONING_EFFORT:")
                    .or_else(|| goose_config_value_from_line(line, "reasoning_effort:"))
                    .or_else(|| goose_config_value_from_line(line, "effort:"));
            }
            config
        })
}

fn goose_config_value_from_line(line: &str, key: &str) -> Option<String> {
    let line = line.trim();
    if line.starts_with('#') {
        return None;
    }
    let value = line.strip_prefix(key)?;
    normalized_model(Some(
        value.trim().trim_matches(|ch| matches!(ch, '"' | '\'')),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_of(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn env_of(command: &Command, key: &str) -> Option<String> {
        command.get_envs().find_map(|(k, v)| {
            (k.to_string_lossy() == key).then(|| {
                v.map(|value| value.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
        })
    }

    #[test]
    fn executor_headless_attaches_role_scoped_ferrus_extension_and_auto_mode() {
        let command = Executor::new(None)
            .spawn(AgentRunMode::Headless { prompt: "do work" })
            .unwrap();
        assert_eq!(command.get_program().to_string_lossy(), "goose");
        let args = args_of(&command);
        assert_eq!(args[0], "run");
        assert!(args.contains(&"--no-session".to_string()));
        assert!(args.contains(&"--text".to_string()));
        assert!(args.contains(&"do work".to_string()));

        // The extension command must carry the executor role-scoped serve invocation.
        let ext_idx = args.iter().position(|a| a == "--with-extension").unwrap();
        let extension = &args[ext_idx + 1];
        assert!(extension.ends_with("serve --role executor --agent-name goose"));

        assert_eq!(env_of(&command, "GOOSE_MODE").as_deref(), Some("auto"));
    }

    #[test]
    fn headless_run_sets_loop_guards() {
        let command = Executor::new(None)
            .spawn(AgentRunMode::Headless { prompt: "x" })
            .unwrap();
        let args = args_of(&command);
        assert!(
            args.contains(&"--max-tool-repetitions".to_string()),
            "headless runs must cap identical tool repetitions"
        );
        // `--max-turns` is applied by default but yields to an explicit GOOSE_MAX_TURNS.
        assert!(
            args.contains(&"--max-turns".to_string())
                || std::env::var_os("GOOSE_MAX_TURNS").is_some(),
            "headless runs must bound total turns unless GOOSE_MAX_TURNS is set"
        );
    }

    #[test]
    fn supervisor_headless_uses_supervisor_role() {
        let command = Supervisor::new(None)
            .spawn(AgentRunMode::Headless { prompt: "review" })
            .unwrap();
        let args = args_of(&command);
        let ext_idx = args.iter().position(|a| a == "--with-extension").unwrap();
        assert!(args[ext_idx + 1].ends_with("serve --role supervisor --agent-name goose"));
    }

    #[cfg(not(windows))]
    #[test]
    fn extension_command_quotes_components_for_posix_shells() {
        let command = quote_command_components([
            "/Applications/Ferrus Beta/ferrus".to_string(),
            "serve".to_string(),
            "--role".to_string(),
            "executor".to_string(),
            "agent's-name".to_string(),
        ]);

        assert_eq!(
            command,
            "'/Applications/Ferrus Beta/ferrus' serve --role executor 'agent'\"'\"'s-name'"
        );
    }

    #[cfg(windows)]
    #[test]
    fn extension_command_quotes_components_for_windows_command_lines() {
        let command = quote_command_components([
            r"C:\Program Files\Ferrus\ferrus.exe".to_string(),
            "serve".to_string(),
            "--role".to_string(),
            "executor".to_string(),
            r#"agent"name"#.to_string(),
        ]);

        assert_eq!(
            command,
            r#""C:\Program Files\Ferrus\ferrus.exe" serve --role executor "agent\"name""#
        );
    }

    #[test]
    fn model_override_is_passed_through_goose_model_env() {
        let command = Executor::new(Some("google/gemma-4-26b-a4b-qat"))
            .spawn(AgentRunMode::Headless { prompt: "x" })
            .unwrap();
        assert_eq!(
            env_of(&command, "GOOSE_MODEL").as_deref(),
            Some("google/gemma-4-26b-a4b-qat")
        );
    }

    #[test]
    fn interactive_without_prompt_uses_session() {
        let command = Supervisor::new(None)
            .spawn(AgentRunMode::Interactive { prompt: None })
            .unwrap();
        let args = args_of(&command);
        assert_eq!(args[0], "session");
        assert!(args.contains(&"--with-extension".to_string()));
        // No auto mode for a human-driven interactive session.
        assert_eq!(env_of(&command, "GOOSE_MODE"), None);
    }

    #[test]
    fn interactive_with_prompt_seeds_run_interactive() {
        let command = Supervisor::new(None)
            .spawn(AgentRunMode::Interactive {
                prompt: Some("plan"),
            })
            .unwrap();
        let args = args_of(&command);
        assert_eq!(args[0], "run");
        assert!(args.contains(&"--interactive".to_string()));
        assert!(args.contains(&"plan".to_string()));
    }

    #[test]
    fn version_command_is_plain() {
        let command = Executor::new(None).version_command().unwrap();
        assert_eq!(command.get_program().to_string_lossy(), "goose");
        assert_eq!(args_of(&command), vec!["--version".to_string()]);
    }

    #[test]
    fn goose_config_model_reads_goose_model_key() {
        let config = goose_display_from_config_lines([
            r#"GOOSE_MODEL: "gpt-oss-120b""#,
            "GOOSE_REASONING_EFFORT: high",
        ]);
        assert_eq!(config.model.as_deref(), Some("gpt-oss-120b"));
        assert_eq!(config.effort.as_deref(), Some("high"));

        let config =
            goose_display_from_config_lines(["GOOSE_PROVIDER: openai", "# GOOSE_MODEL: ignored"]);
        assert!(config.is_empty());
    }
}
