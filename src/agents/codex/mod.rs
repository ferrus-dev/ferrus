//! Codex-backed supervisor and executor adapters.
//!
//! These wrappers isolate the CLI details needed to launch Codex in the shapes
//! Ferrus expects for interactive and headless sessions.

use super::{
    AgentRunMode, ExecutorAgent, HeadlessPromptTransport, SupervisorAgent, normalized_model,
    validate_toml_mcp_server,
};
use crate::agent_id::{ROLE_EXECUTOR, ROLE_SUPERVISOR, legacy_mcp_server_name, mcp_server_name};
use anyhow::Result;
#[cfg(windows)]
use anyhow::anyhow;
#[cfg(windows)]
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

fn apply_opposite_role_mcp_override(command: &mut Command, role: &str, index: u32) {
    let config_paths = codex_config_paths();
    apply_opposite_role_mcp_override_with_paths(command, role, index, &config_paths);
}

fn apply_opposite_role_mcp_override_with_paths(
    command: &mut Command,
    role: &str,
    index: u32,
    config_paths: &[std::path::PathBuf],
) {
    let opposite_role = match role {
        ROLE_SUPERVISOR => ROLE_EXECUTOR,
        ROLE_EXECUTOR => ROLE_SUPERVISOR,
        _ => return,
    };
    let opposite_server = mcp_server_name(opposite_role);
    let legacy_opposite_server = legacy_mcp_server_name(opposite_role, index);
    if codex_mcp_server_configured_in_paths(&opposite_server, config_paths) {
        command
            .arg("--config")
            .arg(format!("mcp_servers.{opposite_server}.enabled=false"));
    }
    if codex_mcp_server_configured_in_paths(&legacy_opposite_server, config_paths) {
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
        .expect("path override lock poisoned")
        .clone()
}

#[cfg(all(windows, test))]
fn set_windows_test_path_override(value: Option<std::ffi::OsString>) {
    *windows_test_path_override_lock()
        .lock()
        .expect("path override lock poisoned") = value;
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

pub(crate) fn apply_tool_approval_overrides(role: &str, entry: &mut toml::Table) {
    let tools = entry
        .entry("tools")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .expect("tools must be a TOML table");

    for tool in auto_approved_tools(role) {
        let mut tool_config = toml::Table::new();
        tool_config.insert(
            "approval_mode".to_string(),
            toml::Value::String("approve".to_string()),
        );
        tools.insert(tool.to_string(), toml::Value::Table(tool_config));
    }
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
        ],
        ROLE_SUPERVISOR => &[
            "enqueue_task",
            "create_spec",
            "wait_for_review",
            "review_pending",
            "approve",
            "reject",
            "wait_for_consultation",
            "respond_consult",
            "ask_human",
            "wait_for_answer",
        ],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_id::DEFAULT_AGENT_INDEX;
    #[cfg(not(windows))]
    use crate::agents::tests::assert_program_and_args;
    #[cfg(windows)]
    use std::ffi::OsString;
    #[cfg(windows)]
    use std::sync::Mutex;
    #[cfg(windows)]
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[cfg(windows)]
    struct PathGuard {
        original: Option<OsString>,
    }

    #[cfg(windows)]
    impl PathGuard {
        fn set(path: &std::path::Path) -> Self {
            let original = windows_test_path_override();
            set_windows_test_path_override(Some(path.as_os_str().to_os_string()));
            Self { original }
        }
    }

    #[cfg(windows)]
    impl Drop for PathGuard {
        fn drop(&mut self) {
            set_windows_test_path_override(self.original.take());
        }
    }

    #[cfg(windows)]
    fn assert_windows_program_and_args(command: Result<Command>, tail_args: &[&str]) {
        let Ok(command) = command else {
            let error = command.unwrap_err().to_string();
            assert!(
                error.contains("Failed to resolve Codex Windows launcher"),
                "expected structured launcher resolution error, got: {error}"
            );
            return;
        };
        let program = command.get_program().to_string_lossy().into_owned();
        let actual = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        if program.ends_with("node.exe") || program == "node.exe" {
            assert!(
                !actual.is_empty(),
                "expected codex.js arg + launcher args, got: {actual:?}"
            );
            assert!(
                actual[0].ends_with("node_modules\\@openai\\codex\\bin\\codex.js"),
                "expected codex.js path, got {}",
                actual[0]
            );
            let expected_tail = tail_args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            assert_eq!(actual[1..], expected_tail);
            return;
        }
        panic!("unexpected launcher program: {program} with args {actual:?}");
    }

    #[cfg(windows)]
    fn assert_windows_version_command_shape(command: Result<Command>, expected_node: &str) {
        let command = command.expect("version command should resolve");
        let program = command.get_program().to_string_lossy().into_owned();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            program.ends_with(expected_node),
            "expected launcher program ending with {expected_node}, got {program}"
        );
        assert_eq!(args.len(), 2, "expected codex.js + --version args");
        assert!(
            args[0].ends_with("node_modules\\@openai\\codex\\bin\\codex.js"),
            "expected first arg to be codex.js path, got {}",
            args[0]
        );
        assert_eq!(args[1], "--version");
    }

    fn command_args(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    }

    fn assert_no_opposite_disable_for_missing_server(args: &[String], opposite_role: &str) {
        let server = mcp_server_name(opposite_role);
        assert!(
            !args
                .iter()
                .any(|arg| arg == &format!("mcp_servers.{server}.enabled=false")),
            "missing opposite role should not be disabled through a synthetic MCP entry: {args:?}"
        );
    }

    #[test]
    fn codex_supervisor_builds_interactive_command() {
        let agent = Supervisor::new(None);
        #[cfg(not(windows))]
        {
            let command = agent
                .spawn(AgentRunMode::Interactive {
                    prompt: Some("plan"),
                })
                .unwrap();
            assert_eq!(command.get_program().to_string_lossy(), EXECUTABLE);
            let args = command_args(&command);
            assert_eq!(args.first().map(String::as_str), Some("plan"));
        }
        #[cfg(windows)]
        if let Ok(command) = agent.spawn(AgentRunMode::Interactive {
            prompt: Some("plan"),
        }) {
            let args = command_args(&command);
            assert!(args.iter().any(|arg| arg == "plan"));
        }
    }

    #[test]
    fn codex_executor_builds_headless_command() {
        let agent = Executor::new(None);
        #[cfg(not(windows))]
        {
            let command = agent
                .spawn(AgentRunMode::Headless { prompt: "run" })
                .unwrap();
            assert_eq!(command.get_program().to_string_lossy(), EXECUTABLE);
            let args = command_args(&command);
            assert_eq!(&args[..2], ["exec", "run"]);
        }
        #[cfg(windows)]
        assert_windows_program_and_args(
            agent.spawn(AgentRunMode::Headless { prompt: "run" }),
            &["exec", "-"],
        );
    }

    #[test]
    fn codex_does_not_disable_missing_opposite_role() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut command = Command::new("codex");
        apply_opposite_role_mcp_override_with_paths(
            &mut command,
            ROLE_EXECUTOR,
            DEFAULT_AGENT_INDEX,
            &[dir.path().join("config.toml")],
        );
        let args = command_args(&command);
        assert_no_opposite_disable_for_missing_server(&args, ROLE_SUPERVISOR);
    }

    #[test]
    fn codex_disables_configured_opposite_role_only() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "[mcp_servers.ferrus-supervisor]\ncommand = \"ferrus\"\nargs = []\n",
        )
        .unwrap();
        let mut command = Command::new("codex");
        apply_opposite_role_mcp_override_with_paths(
            &mut command,
            ROLE_EXECUTOR,
            DEFAULT_AGENT_INDEX,
            &[config_path],
        );
        let args = command_args(&command);
        assert!(
            args.iter()
                .any(|arg| arg == "mcp_servers.ferrus-supervisor.enabled=false"),
            "expected configured opposite role to be disabled in {args:?}"
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg == "mcp_servers.ferrus-supervisor-1.enabled=false"),
            "missing legacy opposite role should not be synthesized in {args:?}"
        );
    }

    #[test]
    fn codex_model_override_is_part_of_spawned_command() {
        let agent = Executor::new(Some("gpt-5.4"));
        #[cfg(not(windows))]
        {
            let command = agent
                .spawn(AgentRunMode::Headless { prompt: "run" })
                .unwrap();
            assert_eq!(command.get_program().to_string_lossy(), EXECUTABLE);
            let args = command_args(&command);
            assert_eq!(&args[..4], ["exec", "--model", "gpt-5.4", "run"]);
        }
        #[cfg(windows)]
        assert_windows_program_and_args(
            agent.spawn(AgentRunMode::Headless { prompt: "run" }),
            &["exec", "--model", "gpt-5.4", "-"],
        );
    }

    #[test]
    fn codex_supervisor_config_entry_uses_expected_args() {
        let entry = Supervisor::new(Some("gpt-5.4"))
            .mcp_config_entry("supervisor", 1)
            .unwrap();
        assert!(!entry.command.is_empty());
        assert_eq!(
            entry.args,
            vec!["serve", "--role", "supervisor", "--agent-name", "codex",]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(entry.model.as_deref(), Some("gpt-5.4"));
    }

    #[test]
    fn codex_config_entry_uses_expected_args() {
        let entry = Executor::new(None).mcp_config_entry("executor", 3).unwrap();
        assert!(!entry.command.is_empty());
        assert_eq!(
            entry.args,
            vec!["serve", "--role", "executor", "--agent-name", "codex",]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(entry.model, None);
    }

    #[test]
    fn codex_interactive_preflight_reports_missing_config() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let agent = Supervisor::new(None);

        let err = agent
            .validate_interactive_launch(ROLE_SUPERVISOR, 1)
            .unwrap_err();
        let message = err.to_string();

        assert!(message.contains("Invalid MCP configuration"));
        assert!(message.contains(".codex/config.toml"));
        std::env::set_current_dir(previous).unwrap();
    }

    #[test]
    fn codex_interactive_preflight_reports_missing_role_server() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        std::fs::create_dir_all(".codex").unwrap();
        std::fs::write(
            ".codex/config.toml",
            "[mcp_servers.ferrus-executor-1]\ncommand = \"ferrus\"\nargs = []\n",
        )
        .unwrap();
        let agent = Supervisor::new(None);

        let err = agent
            .validate_interactive_launch(ROLE_SUPERVISOR, 1)
            .unwrap_err();
        let message = err.to_string();

        assert!(message.contains("MCP server `ferrus-supervisor` not found"));
        std::env::set_current_dir(previous).unwrap();
    }

    #[test]
    fn codex_interactive_preflight_accepts_registered_role_server() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        std::fs::create_dir_all(".codex").unwrap();
        std::fs::write(
            ".codex/config.toml",
            "[mcp_servers.ferrus-supervisor]\ncommand = \"ferrus\"\nargs = []\n",
        )
        .unwrap();
        let agent = Supervisor::new(None);

        agent
            .validate_interactive_launch(ROLE_SUPERVISOR, 1)
            .unwrap();
        std::env::set_current_dir(previous).unwrap();
    }

    #[test]
    fn codex_approves_executor_tools_by_role() {
        let mut entry = toml::Table::new();
        apply_tool_approval_overrides("executor", &mut entry);
        let tools = entry.get("tools").and_then(toml::Value::as_table).unwrap();
        assert!(tools.contains_key("wait_for_task"));
        assert!(tools.contains_key("submit"));
        assert!(!tools.contains_key("approve"));
    }

    #[test]
    fn codex_approves_supervisor_tools_by_role() {
        let mut entry = toml::Table::new();
        apply_tool_approval_overrides("supervisor", &mut entry);
        let tools = entry.get("tools").and_then(toml::Value::as_table).unwrap();
        assert!(tools.contains_key("enqueue_task"));
        assert!(tools.contains_key("create_spec"));
        assert!(tools.contains_key("wait_for_consultation"));
        assert!(!tools.contains_key("create_task"));
        assert!(!tools.contains_key("submit"));
    }

    #[test]
    fn codex_role_tool_approval_sets_fit_mcp_first_page() {
        assert!(auto_approved_tools(ROLE_EXECUTOR).len() <= 10);
        assert!(auto_approved_tools(ROLE_SUPERVISOR).len() <= 10);
    }

    #[test]
    fn codex_headless_prompt_preserves_newlines() {
        let agent = Executor::new(None);
        #[cfg(not(windows))]
        {
            let command = agent
                .spawn(AgentRunMode::Headless {
                    prompt: "line one\n\nline two",
                })
                .unwrap();
            assert_eq!(command.get_program().to_string_lossy(), EXECUTABLE);
            let args = command_args(&command);
            assert_eq!(&args[..2], ["exec", "line one\n\nline two"]);
        }
        #[cfg(windows)]
        assert_windows_program_and_args(
            agent.spawn(AgentRunMode::Headless {
                prompt: "line one\n\nline two",
            }),
            &["exec", "-"],
        );
    }

    #[test]
    fn codex_uses_expected_headless_prompt_transport() {
        #[cfg(windows)]
        let expected = HeadlessPromptTransport::Stdin;
        #[cfg(not(windows))]
        let expected = HeadlessPromptTransport::Argv;
        assert_eq!(Executor::new(None).headless_prompt_transport(), expected);
        assert_eq!(Supervisor::new(None).headless_prompt_transport(), expected);
    }

    #[test]
    fn codex_version_command_uses_expected_shape() {
        let agent = Supervisor::new(None);
        #[cfg(not(windows))]
        assert_program_and_args(agent.version_command().unwrap(), EXECUTABLE, &["--version"]);

        #[cfg(windows)]
        {
            let _lock = ENV_LOCK.lock().expect("env lock poisoned");
            let temp = tempfile::TempDir::new().expect("tempdir");
            let bin_dir = temp.path().join("npm");
            std::fs::create_dir_all(&bin_dir).expect("create shim dir");
            std::fs::write(bin_dir.join(WINDOWS_CMD_EXECUTABLE), "@echo off\n").expect("shim");
            let codex_js = bin_dir
                .join("node_modules")
                .join("@openai")
                .join("codex")
                .join("bin")
                .join("codex.js");
            std::fs::create_dir_all(codex_js.parent().expect("codex.js parent"))
                .expect("create codex.js parent");
            std::fs::write(&codex_js, "console.log('codex');").expect("codex.js");
            std::fs::write(bin_dir.join("node.exe"), "").expect("node.exe");

            let _guard = PathGuard::set(&bin_dir);
            assert_windows_version_command_shape(agent.version_command(), "node.exe");
        }
    }
}
