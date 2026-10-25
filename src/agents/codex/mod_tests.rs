//! Codex command construction, model settings, and MCP isolation across platforms.

use super::*;
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
    apply_opposite_role_mcp_override_with_paths(&mut command, ROLE_EXECUTOR, &[config_path]);
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
fn codex_disables_all_configured_legacy_opposite_role_servers() {
    let dir = tempfile::TempDir::new().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[mcp_servers.ferrus-supervisor-1]
command = "ferrus"
args = []

[mcp_servers.ferrus-supervisor-3]
command = "ferrus"
args = []

[mcp_servers.ferrus-executor-1]
command = "ferrus"
args = []
"#,
    )
    .unwrap();
    let mut command = Command::new("codex");
    apply_opposite_role_mcp_override_with_paths(&mut command, ROLE_EXECUTOR, &[config_path]);
    let args = command_args(&command);

    for server in ["ferrus-supervisor-1", "ferrus-supervisor-3"] {
        assert!(
            args.iter()
                .any(|arg| arg == &format!("mcp_servers.{server}.enabled=false")),
            "expected configured legacy opposite role server {server} to be disabled in {args:?}"
        );
    }

    assert!(
        !args
            .iter()
            .any(|arg| arg == "mcp_servers.ferrus-executor-1.enabled=false"),
        "same-role legacy server should stay enabled in {args:?}"
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
    apply_tool_approval_overrides("executor", &mut entry).unwrap();
    let tools = entry.get("tools").and_then(toml::Value::as_table).unwrap();
    assert!(tools.contains_key("wait_for_task"));
    assert!(tools.contains_key("submit"));
    assert!(tools.contains_key("repository_graph_status"));
    assert!(tools.contains_key("repository_search"));
    assert_eq!(
        tools["repository_context"]["approval_mode"].as_str(),
        Some("approve")
    );
    for tool in [
        "project_memory_status",
        "project_context_search",
        "project_context",
    ] {
        assert_eq!(tools[tool]["approval_mode"].as_str(), Some("approve"));
    }

    assert!(!tools.contains_key("approve"));
}

#[test]
fn codex_approves_supervisor_tools_by_role() {
    let mut entry = toml::Table::new();
    apply_tool_approval_overrides("supervisor", &mut entry).unwrap();
    let tools = entry.get("tools").and_then(toml::Value::as_table).unwrap();
    assert!(tools.contains_key("enqueue_task"));
    assert!(tools.contains_key("create_spec"));
    assert!(tools.contains_key("archive_spec"));
    assert!(tools.contains_key("wait_for_consultation"));
    assert!(tools.contains_key("heartbeat"));
    assert!(tools.contains_key("repository_graph_status"));
    assert!(tools.contains_key("repository_search"));
    assert_eq!(
        tools["repository_context"]["approval_mode"].as_str(),
        Some("approve")
    );
    for tool in [
        "project_memory_status",
        "project_context_search",
        "project_context",
    ] {
        assert_eq!(tools[tool]["approval_mode"].as_str(), Some("approve"));
    }

    assert!(!tools.contains_key("create_task"));
    assert!(!tools.contains_key("submit"));
}

#[test]
fn codex_tool_approval_sets_match_role_needs() {
    assert!(auto_approved_tools(ROLE_EXECUTOR).len() <= 16);
    assert!(auto_approved_tools(ROLE_SUPERVISOR).len() <= 18);
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
