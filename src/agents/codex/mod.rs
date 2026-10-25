//! Codex-backed supervisor and executor adapters.
//!
//! These wrappers isolate the CLI details needed to launch Codex in the shapes
//! Ferrus expects for interactive and headless sessions.

use super::{
    AgentDisplayConfig, AgentRunMode, ExecutorAgent, HeadlessPromptTransport, SupervisorAgent,
    normalized_model, toml_config_display_from_paths, validate_toml_mcp_server,
};
use crate::agent_id::{ROLE_EXECUTOR, ROLE_SUPERVISOR, legacy_mcp_server_name, mcp_server_name};
#[cfg(windows)]
use anyhow::anyhow;
use anyhow::{Context as _, Result};
use std::collections::BTreeSet;
#[cfg(windows)]
use std::path::PathBuf;
#[cfg(not(windows))]
use std::path::PathBuf;
use std::process::Command;

/// Stable agent identifier used in Ferrus configuration and error messages.
pub(crate) const NAME: &str = "codex";
/// Actual CLI executable name used to launch Codex.
#[cfg(not(windows))]
const EXECUTABLE: &str = "codex";
#[cfg(windows)]
const WINDOWS_CMD_EXECUTABLE: &str = "codex.cmd";
#[cfg(windows)]
const WINDOWS_POWERSHELL_EXECUTABLE: &str = "codex.ps1";

/// Interactive and headless supervisor launcher for the Codex CLI.
#[derive(Debug, Clone)]
pub struct Supervisor {
    model: Option<String>,
}

/// Interactive and headless executor launcher for the Codex CLI.
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
    /// Returns the Ferrus-visible identifier for the Codex supervisor backend.
    fn name(&self) -> &'static str {
        NAME
    }

    /// Builds the Codex command used by Ferrus HQ or an interactive user.
    fn spawn_with_index(&self, mode: AgentRunMode<'_>, index: u32) -> Result<Command> {
        codex_command(mode, self.model(), ROLE_SUPERVISOR, index)
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn display_config(&self) -> AgentDisplayConfig {
        AgentDisplayConfig::from_model(self.model()).merge_missing(codex_config_display())
    }

    fn version_command(&self) -> Result<Command> {
        codex_version_command()
    }

    fn headless_prompt_transport(&self) -> HeadlessPromptTransport {
        codex_headless_prompt_transport()
    }

    fn validate_interactive_launch(&self, role: &str, index: u32) -> Result<()> {
        validate_interactive_launch(role, index)
    }
}

impl ExecutorAgent for Executor {
    /// Returns the Ferrus-visible identifier for the Codex executor backend.
    fn name(&self) -> &'static str {
        NAME
    }

    /// Builds the Codex command used by Ferrus HQ or an interactive user.
    fn spawn_with_index(&self, mode: AgentRunMode<'_>, index: u32) -> Result<Command> {
        codex_command(mode, self.model(), ROLE_EXECUTOR, index)
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn display_config(&self) -> AgentDisplayConfig {
        AgentDisplayConfig::from_model(self.model()).merge_missing(codex_config_display())
    }

    fn version_command(&self) -> Result<Command> {
        codex_version_command()
    }

    fn headless_prompt_transport(&self) -> HeadlessPromptTransport {
        codex_headless_prompt_transport()
    }

    fn validate_interactive_launch(&self, role: &str, index: u32) -> Result<()> {
        validate_interactive_launch(role, index)
    }
}

#[inline(always)]
fn codex_command(
    mode: AgentRunMode<'_>,
    model: Option<&str>,
    role: &str,
    index: u32,
) -> Result<Command> {
    #[cfg(windows)]
    let mut cmd = windows_codex_command()?;
    #[cfg(not(windows))]
    let mut cmd = Command::new(EXECUTABLE);
    match mode {
        AgentRunMode::Interactive { prompt } => {
            if let Some(model) = model {
                cmd.arg("--model").arg(model);
            }

            if let Some(prompt) = prompt {
                cmd.arg(prompt);
            }
        }
        AgentRunMode::Headless { prompt } => {
            // `codex exec` is the non-interactive entrypoint that runs a single prompt
            // and exits, which matches Ferrus executor and supervisor automation.
            cmd.arg("exec");
            if let Some(model) = model {
                cmd.arg("--model").arg(model);
            }

            #[cfg(windows)]
            {
                let _ = prompt;
                cmd.arg("-");
            }
            #[cfg(not(windows))]
            cmd.arg(prompt);
        }
    }

    apply_opposite_role_mcp_override(&mut cmd, role, index);
    Ok(cmd)
}

fn codex_version_command() -> Result<Command> {
    #[cfg(windows)]
    let mut cmd = windows_codex_command()?;
    #[cfg(not(windows))]
    let mut cmd = Command::new(EXECUTABLE);
    cmd.arg("--version");
    Ok(cmd)
}

fn apply_opposite_role_mcp_override(command: &mut Command, role: &str, _index: u32) {
    let config_paths = codex_config_paths();
    apply_opposite_role_mcp_override_with_paths(command, role, &config_paths);
}

fn apply_opposite_role_mcp_override_with_paths(
    command: &mut Command,
    role: &str,
    config_paths: &[std::path::PathBuf],
) {
    let opposite_role = match role {
        ROLE_SUPERVISOR => ROLE_EXECUTOR,
        ROLE_EXECUTOR => ROLE_SUPERVISOR,
        _ => return,
    };
    let opposite_server = mcp_server_name(opposite_role);
    if codex_mcp_server_configured_in_paths(&opposite_server, config_paths) {
        command
            .arg("--config")
            .arg(format!("mcp_servers.{opposite_server}.enabled=false"));
    }

    for legacy_opposite_server in
        codex_legacy_mcp_servers_configured_in_paths(opposite_role, config_paths)
    {
        command.arg("--config").arg(format!(
            "mcp_servers.{legacy_opposite_server}.enabled=false"
        ));
    }
}

fn codex_mcp_server_configured_in_paths(server: &str, paths: &[std::path::PathBuf]) -> bool {
    paths
        .iter()
        .any(|path| toml_config_has_mcp_server(path, server))
}

fn codex_legacy_mcp_servers_configured_in_paths(
    role: &str,
    paths: &[std::path::PathBuf],
) -> Vec<String> {
    let mut servers = BTreeSet::new();
    for path in paths {
        servers.extend(toml_config_legacy_mcp_servers_for_role(path, role));
    }

    servers.into_iter().collect()
}

fn toml_config_legacy_mcp_servers_for_role(path: &std::path::Path, role: &str) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(root) = content.parse::<toml::Table>() else {
        return Vec::new();
    };
    let Some(servers) = root.get("mcp_servers").and_then(toml::Value::as_table) else {
        return Vec::new();
    };
    servers
        .keys()
        .filter(|server| is_legacy_mcp_server_for_role(server, role))
        .cloned()
        .collect()
}

fn is_legacy_mcp_server_for_role(server: &str, role: &str) -> bool {
    let prefix = format!("ferrus-{role}-");
    server
        .strip_prefix(&prefix)
        .is_some_and(|index| !index.is_empty() && index.parse::<u32>().is_ok())
}

fn codex_config_paths() -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = std::env::var_os("CODEX_HOME").map(std::path::PathBuf::from) {
        paths.push(home.join("config.toml"));
    } else if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".codex").join("config.toml"));
    }

    paths.push(std::path::PathBuf::from(".codex").join("config.toml"));
    paths
}

fn codex_model_config_paths() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from(".codex").join("config.toml")];
    if let Some(home) = std::env::var_os("CODEX_HOME").map(PathBuf::from) {
        paths.push(home.join("config.toml"));
    } else if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".codex").join("config.toml"));
    }

    paths
}

fn codex_config_display() -> AgentDisplayConfig {
    toml_config_display_from_paths(codex_model_config_paths())
}

fn toml_config_has_mcp_server(path: &std::path::Path, server: &str) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(root) = content.parse::<toml::Table>() else {
        return false;
    };
    root.get("mcp_servers")
        .and_then(toml::Value::as_table)
        .is_some_and(|servers| servers.contains_key(server))
}

fn validate_interactive_launch(role: &str, index: u32) -> Result<()> {
    let path = std::path::Path::new(".codex/config.toml");
    let primary = mcp_server_name(role);
    match validate_toml_mcp_server(path, &primary) {
        Ok(()) => Ok(()),
        Err(primary_err) => {
            let legacy = legacy_mcp_server_name(role, index);
            validate_toml_mcp_server(path, &legacy).map_err(|_| primary_err)
        }
    }
}

fn codex_headless_prompt_transport() -> HeadlessPromptTransport {
    #[cfg(windows)]
    {
        HeadlessPromptTransport::Stdin
    }
    #[cfg(not(windows))]
    {
        HeadlessPromptTransport::Argv
    }
}

#[cfg(windows)]
fn resolve_windows_npm_shim_path() -> Option<PathBuf> {
    windows_path_dirs().into_iter().find_map(|path| {
        [
            path.join(WINDOWS_CMD_EXECUTABLE),
            path.join(WINDOWS_POWERSHELL_EXECUTABLE),
        ]
        .into_iter()
        .find(|candidate| candidate.is_file())
    })
}

#[cfg(windows)]
fn windows_path_dirs() -> Vec<PathBuf> {
    #[cfg(test)]
    if let Some(paths) = windows_test_path_override() {
        return std::env::split_paths(&paths).collect();
    }

    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect())
        .unwrap_or_default()
}

#[cfg(all(windows, test))]
fn windows_test_path_override() -> Option<std::ffi::OsString> {
    windows_test_path_override_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

#[cfg(all(windows, test))]
fn set_windows_test_path_override(value: Option<std::ffi::OsString>) {
    *windows_test_path_override_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = value;
}

#[cfg(all(windows, test))]
fn windows_test_path_override_lock() -> &'static std::sync::Mutex<Option<std::ffi::OsString>> {
    use std::sync::{Mutex, OnceLock};

    static PATH_OVERRIDE: OnceLock<Mutex<Option<std::ffi::OsString>>> = OnceLock::new();
    PATH_OVERRIDE.get_or_init(|| Mutex::new(None))
}

#[cfg(windows)]
fn windows_codex_invocation() -> Result<(PathBuf, PathBuf)> {
    // Windows npm shims (`codex.cmd` / `codex.ps1`) are unreliable for Ferrus-managed
    // interactive/headless sessions, so we resolve the npm-installed Codex JS entrypoint
    // and launch it through node.exe directly. This intentionally depends on the current
    // npm package layout and should eventually be replaced by a native Codex binary
    // launcher when one is consistently available on Windows.
    let shim = resolve_windows_npm_shim_path().ok_or_else(|| {
        anyhow!(
            "Failed to locate codex.cmd or codex.ps1 in PATH; cannot resolve npm base directory \
             for direct Node launcher."
        )
    })?;
    let base_dir = shim.parent().ok_or_else(|| {
        anyhow!(
            "Failed to resolve parent directory for shim path: {}",
            shim.display()
        )
    })?;
    let codex_js = base_dir
        .join("node_modules")
        .join("@openai")
        .join("codex")
        .join("bin")
        .join("codex.js");
    if !codex_js.is_file() {
        return Err(anyhow!(
            "Failed to resolve direct Codex Node launcher. Ferrus currently expects the npm \
             Codex layout at node_modules/@openai/codex/bin/codex.js on Windows. If this \
             Codex installation uses a native/Rust binary layout, configure an explicit \
             Codex executable or reinstall Codex via npm."
        ));
    }

    let local_node = base_dir.join("node.exe");
    let node = if local_node.is_file() {
        local_node
    } else {
        PathBuf::from("node.exe")
    };
    Ok((node, codex_js))
}

#[cfg(windows)]
fn windows_codex_command() -> Result<Command> {
    let (node, codex_js) = windows_codex_invocation()
        .map_err(|error| anyhow!("Failed to resolve Codex Windows launcher: {error}"))?;
    let mut cmd = Command::new(node);
    cmd.arg(codex_js);
    Ok(cmd)
}

pub(crate) fn apply_tool_approval_overrides(role: &str, entry: &mut toml::Table) -> Result<()> {
    let tools = entry
        .entry("tools")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .context("tools must be a TOML table")?;

    for tool in auto_approved_tools(role) {
        let mut tool_config = toml::Table::new();
        tool_config.insert(
            "approval_mode".to_string(),
            toml::Value::String("approve".to_string()),
        );
        tools.insert(tool.to_string(), toml::Value::Table(tool_config));
    }
    Ok(())
}

fn auto_approved_tools(role: &str) -> &'static [&'static str] {
    match role {
        ROLE_EXECUTOR => &[
            "wait_for_task",
            "check",
            "consult",
            "submit",
            "wait_for_consult",
            "wait_for_answer",
            "ask_human",
            "status",
            "reset",
            "heartbeat",
            "repository_graph_status",
            "repository_search",
            "repository_context",
            "project_memory_status",
            "project_context_search",
            "project_context",
        ],
        ROLE_SUPERVISOR => &[
            "enqueue_task",
            "create_spec",
            "archive_spec",
            "wait_for_review",
            "review_pending",
            "approve",
            "reject",
            "wait_for_consultation",
            "respond_consult",
            "ask_human",
            "wait_for_answer",
            "heartbeat",
            "repository_graph_status",
            "repository_search",
            "repository_context",
            "project_memory_status",
            "project_context_search",
            "project_context",
        ],
        _ => &[],
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
