use super::*;

struct CurrentDirGuard {
    previous: std::path::PathBuf,
}

impl CurrentDirGuard {
    fn change_to(path: &std::path::Path) -> Self {
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(path).unwrap();
        Self { previous }
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
    }
}

#[test]
fn agents_md_supervisor_section_requires_user_approval_before_create_task() {
    let section = agents_md_section(ROLE_SUPERVISOR, "<!-- marker -->");
    assert!(section.contains("supporting context only"));
    assert!(section.contains("must not override"));
}

#[test]
fn claude_md_supervisor_section_requires_user_approval_before_create_task() {
    let section = claude_md_section(ROLE_SUPERVISOR, "<!-- marker -->");
    assert!(section.contains("supporting context only"));
    assert!(section.contains("must not override"));
}

#[test]
fn agents_md_executor_section_forbids_consulting_about_tool_availability() {
    let section = agents_md_section(ROLE_EXECUTOR, "<!-- marker -->");
    assert!(section.contains("supporting context only"));
    assert!(section.contains("must not override"));
}

#[test]
fn agents_md_executor_section_uses_ask_human_when_truly_stuck() {
    let section = agents_md_section(ROLE_EXECUTOR, "<!-- marker -->");
    assert!(section.contains("initial prompt and Ferrus MCP tools"));
    assert!(!section.contains("Full workflow"));
}

#[test]
fn claude_md_executor_section_forbids_consulting_about_tool_availability() {
    let section = claude_md_section(ROLE_EXECUTOR, "<!-- marker -->");
    assert!(section.contains("supporting context only"));
    assert!(section.contains("must not override"));
}

#[test]
fn claude_md_executor_section_uses_ask_human_when_truly_stuck() {
    let section = claude_md_section(ROLE_EXECUTOR, "<!-- marker -->");
    assert!(section.contains("initial prompt and Ferrus MCP tools"));
    assert!(!section.contains("Full workflow"));
}

#[test]
fn normalize_model_update_treats_blank_as_clear() {
    assert_eq!(normalize_model_update(None), None);
    assert_eq!(normalize_model_update(Some("")), Some(None));
    assert_eq!(
        normalize_model_update(Some("gpt-5.4")),
        Some(Some("gpt-5.4"))
    );
}

#[test]
fn normalize_model_treats_blank_as_none() {
    assert_eq!(normalize_model(None), None);
    assert_eq!(normalize_model(Some("")), None);
    assert_eq!(normalize_model(Some(" ")), None);
    assert_eq!(normalize_model(Some("gpt-5.4")), Some("gpt-5.4"));
}

#[tokio::test]
async fn claude_supervisor_registration_reuses_role_only_entry() {
    let _lock = crate::test_support::cwd_lock().lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _cwd_guard = CurrentDirGuard::change_to(temp.path());
    tokio::fs::write("ferrus.toml", "[checks]\n[limits]\n")
        .await
        .unwrap();

    register_claude_code(ROLE_SUPERVISOR, crate::agents::claude::NAME, None, true)
        .await
        .unwrap();
    register_claude_code(ROLE_SUPERVISOR, crate::agents::claude::NAME, None, true)
        .await
        .unwrap();

    let content = tokio::fs::read_to_string(".claude/mcp-supervisor.json")
        .await
        .unwrap();
    let root: serde_json::Value = serde_json::from_str(&content).unwrap();
    let servers = root
        .get("mcpServers")
        .and_then(serde_json::Value::as_object)
        .unwrap();
    assert!(servers.contains_key("ferrus-supervisor"));
    assert_eq!(servers.len(), 1);
    assert!(!servers.contains_key("ferrus-executor"));
}

#[tokio::test]
async fn claude_executor_registration_is_role_scoped_and_role_only() {
    let _lock = crate::test_support::cwd_lock().lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _cwd_guard = CurrentDirGuard::change_to(temp.path());
    tokio::fs::write("ferrus.toml", "[checks]\n[limits]\n")
        .await
        .unwrap();

    register_claude_code(ROLE_SUPERVISOR, crate::agents::claude::NAME, None, true)
        .await
        .unwrap();
    register_claude_code(ROLE_EXECUTOR, crate::agents::claude::NAME, None, true)
        .await
        .unwrap();
    register_claude_code(ROLE_SUPERVISOR, crate::agents::claude::NAME, None, true)
        .await
        .unwrap();

    let supervisor_content = tokio::fs::read_to_string(".claude/mcp-supervisor.json")
        .await
        .unwrap();
    let supervisor_root: serde_json::Value = serde_json::from_str(&supervisor_content).unwrap();
    let supervisor_servers = supervisor_root
        .get("mcpServers")
        .and_then(serde_json::Value::as_object)
        .unwrap();
    assert!(supervisor_servers.contains_key("ferrus-supervisor"));
    assert_eq!(supervisor_servers.len(), 1);
    assert!(!supervisor_servers.contains_key("ferrus-executor"));

    let executor_content = tokio::fs::read_to_string(".claude/mcp-executor.json")
        .await
        .unwrap();
    let executor_root: serde_json::Value = serde_json::from_str(&executor_content).unwrap();
    let executor_servers = executor_root
        .get("mcpServers")
        .and_then(serde_json::Value::as_object)
        .unwrap();
    assert!(executor_servers.contains_key("ferrus-executor"));
    assert!(!executor_servers.contains_key("ferrus-supervisor"));
}

#[tokio::test]
async fn model_flag_requires_matching_agent_flag() {
    let err = run(None, Some("gpt-5.4".to_string()), None, None)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("--supervisor-model requires --supervisor"));
}

#[test]
fn missing_mcp_config_launch_check_is_non_fatal() {
    let check = mcp_launch_check(ROLE_EXECUTOR, crate::agents::codex::NAME, || {
        anyhow::bail!(
            "Invalid MCP configuration:\nMCP config file not found: /tmp/project/.codex/config.toml"
        )
    });

    assert!(!check.ok);
    assert!(!check.fatal);
    assert!(check.message.contains("MCP config is not registered"));
    assert!(check.message.contains("ferrus register --executor codex"));
}

#[tokio::test]
async fn claude_registration_sets_default_mcp_isolation_when_missing() {
    let _lock = crate::test_support::cwd_lock().lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _cwd_guard = CurrentDirGuard::change_to(temp.path());
    tokio::fs::write("ferrus.toml", "[checks]\n[limits]\n")
        .await
        .unwrap();

    register_claude_code(ROLE_SUPERVISOR, crate::agents::claude::NAME, None, true)
        .await
        .unwrap();

    let ferrus_toml = tokio::fs::read_to_string("ferrus.toml").await.unwrap();
    assert!(ferrus_toml.contains("[agents.claude]"));
    assert!(ferrus_toml.contains("mcp_isolation = \"merge-user\""));
}

#[tokio::test]
async fn claude_registration_does_not_overwrite_existing_mcp_isolation() {
    let _lock = crate::test_support::cwd_lock().lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _cwd_guard = CurrentDirGuard::change_to(temp.path());
    tokio::fs::write(
        "ferrus.toml",
        "[checks]\n[limits]\n[agents.claude]\nmcp_isolation = \"ferrus-only\"\n",
    )
    .await
    .unwrap();

    register_claude_code(ROLE_EXECUTOR, crate::agents::claude::NAME, None, true)
        .await
        .unwrap();

    let ferrus_toml = tokio::fs::read_to_string("ferrus.toml").await.unwrap();
    assert!(ferrus_toml.contains("mcp_isolation = \"ferrus-only\""));
    assert!(!ferrus_toml.contains("mcp_isolation = \"merge-user\""));
}

#[tokio::test]
async fn legacy_mcp_config_warnings_report_indexed_entries() {
    let _lock = crate::test_support::cwd_lock().lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _cwd_guard = CurrentDirGuard::change_to(temp.path());
    tokio::fs::create_dir_all(".codex").await.unwrap();
    tokio::fs::write(
        ".codex/config.toml",
        "[mcp_servers.ferrus-executor-1]\ncommand = \"ferrus\"\nargs = []\n",
    )
    .await
    .unwrap();
    tokio::fs::create_dir_all(".claude").await.unwrap();
    tokio::fs::write(
        ".claude/settings.local.json",
        r#"{"permissions":{"allow":["mcp__ferrus-executor-1__*"]}}"#,
    )
    .await
    .unwrap();
    tokio::fs::create_dir_all(".qwen").await.unwrap();
    tokio::fs::write(
        ".qwen/settings.json",
        r#"{"mcpServers":{"ferrus-supervisor-2":{"command":"ferrus","args":[]}}}"#,
    )
    .await
    .unwrap();

    let warnings = legacy_mcp_config_warnings().await.unwrap();

    assert_eq!(warnings.len(), 3);
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("ferrus-executor-1"))
    );
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("mcp__ferrus-executor-1__*"))
    );
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("ferrus-supervisor-2"))
    );
}

#[tokio::test]
async fn migrate_legacy_mcp_configs_collapses_indexed_entries() {
    let _lock = crate::test_support::cwd_lock().lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _cwd_guard = CurrentDirGuard::change_to(temp.path());
    tokio::fs::create_dir_all(".claude").await.unwrap();
    tokio::fs::write(
        ".claude/mcp-supervisor.json",
        r#"{
  "mcpServers": {
    "ferrus-supervisor-1": {"command": "old-one", "args": ["serve"]},
    "ferrus-supervisor-2": {"command": "old-two", "args": ["serve"]}
  }
}"#,
    )
    .await
    .unwrap();
    tokio::fs::create_dir_all(".codex").await.unwrap();
    tokio::fs::write(
        ".codex/config.toml",
        concat!(
            "[mcp_servers.ferrus-executor]\n",
            "command = \"current\"\n",
            "args = []\n\n",
            "[mcp_servers.ferrus-executor-1]\n",
            "command = \"legacy\"\n",
            "args = []\n",
        ),
    )
    .await
    .unwrap();
    tokio::fs::create_dir_all(".qwen").await.unwrap();
    tokio::fs::write(
        ".qwen/settings.json",
        r#"{
  "permissions": {
    "allow": ["mcp__ferrus-supervisor-1__*", "mcp__unrelated__*"]
  }
}"#,
    )
    .await
    .unwrap();

    let messages = migrate_legacy_mcp_configs().await.unwrap();

    assert_eq!(messages.len(), 3);
    let claude_content = tokio::fs::read_to_string(".claude/mcp-supervisor.json")
        .await
        .unwrap();
    let claude_root: serde_json::Value = serde_json::from_str(&claude_content).unwrap();
    let claude_servers = claude_root
        .get("mcpServers")
        .and_then(serde_json::Value::as_object)
        .unwrap();
    assert!(claude_servers.contains_key("ferrus-supervisor"));
    assert_eq!(
        claude_servers
            .get("ferrus-supervisor")
            .and_then(|entry| entry.get("command"))
            .and_then(serde_json::Value::as_str),
        Some("old-one")
    );
    assert!(!claude_servers.contains_key("ferrus-supervisor-1"));
    assert!(!claude_servers.contains_key("ferrus-supervisor-2"));

    let codex_content = tokio::fs::read_to_string(".codex/config.toml")
        .await
        .unwrap();
    let codex_root: toml::Table = codex_content.parse().unwrap();
    let codex_servers = codex_root
        .get("mcp_servers")
        .and_then(toml::Value::as_table)
        .unwrap();
    assert_eq!(
        codex_servers
            .get("ferrus-executor")
            .and_then(toml::Value::as_table)
            .and_then(|entry| entry.get("command"))
            .and_then(toml::Value::as_str),
        Some("current")
    );
    assert!(!codex_servers.contains_key("ferrus-executor-1"));

    let qwen_content = tokio::fs::read_to_string(".qwen/settings.json")
        .await
        .unwrap();
    let qwen_root: serde_json::Value = serde_json::from_str(&qwen_content).unwrap();
    let qwen_allow = qwen_root
        .get("permissions")
        .and_then(|permissions| permissions.get("allow"))
        .and_then(serde_json::Value::as_array)
        .unwrap();
    assert!(
        qwen_allow
            .iter()
            .any(|permission| { permission.as_str() == Some("mcp__ferrus-supervisor__*") })
    );
    assert!(
        qwen_allow
            .iter()
            .any(|permission| { permission.as_str() == Some("mcp__unrelated__*") })
    );
    assert!(
        !qwen_allow
            .iter()
            .any(|permission| { permission.as_str() == Some("mcp__ferrus-supervisor-1__*") })
    );
}

#[tokio::test]
async fn migrate_legacy_mcp_configs_updates_enabled_servers_and_specific_permissions() {
    let _lock = crate::test_support::cwd_lock().lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _cwd_guard = CurrentDirGuard::change_to(temp.path());
    tokio::fs::create_dir_all(".claude").await.unwrap();
    tokio::fs::write(
        ".claude/settings.local.json",
        r#"{
  "enabledMcpjsonServers": ["ferrus-supervisor-1", "unrelated"],
  "permissions": {
    "allow": [
      "mcp__ferrus-supervisor-1__status",
      "mcp__ferrus-supervisor-1__create_task",
      "mcp__unrelated__status"
    ]
  }
}"#,
    )
    .await
    .unwrap();

    let messages = migrate_legacy_mcp_configs().await.unwrap();

    assert!(
        messages
            .iter()
            .any(|message| message.contains("enabled MCP servers"))
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("legacy MCP tool permissions"))
    );

    let content = tokio::fs::read_to_string(".claude/settings.local.json")
        .await
        .unwrap();
    let root: serde_json::Value = serde_json::from_str(&content).unwrap();
    let enabled = root
        .get("enabledMcpjsonServers")
        .and_then(serde_json::Value::as_array)
        .unwrap();
    assert!(enabled.iter().any(|server| server == "ferrus-supervisor"));
    assert!(!enabled.iter().any(|server| server == "ferrus-supervisor-1"));

    let allow = root
        .get("permissions")
        .and_then(|permissions| permissions.get("allow"))
        .and_then(serde_json::Value::as_array)
        .unwrap();
    assert!(
        allow
            .iter()
            .any(|permission| permission == "mcp__ferrus-supervisor__*")
    );
    assert!(
        !allow
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|permission| permission.starts_with("mcp__ferrus-supervisor-1__"))
    );
}

#[tokio::test]
async fn ensure_configured_hq_mcp_configs_creates_missing_role_configs() {
    let _lock = crate::test_support::cwd_lock().lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _cwd_guard = CurrentDirGuard::change_to(temp.path());
    tokio::fs::write(
            "ferrus.toml",
            "[checks]\n[limits]\n[hq.supervisor]\nagent = \"claude-code\"\nmodel = \"\"\n[hq.executor]\nagent = \"codex\"\nmodel = \"\"\n",
        )
        .await
        .unwrap();

    ensure_configured_hq_mcp_configs().await.unwrap();

    let claude_content = tokio::fs::read_to_string(".claude/mcp-supervisor.json")
        .await
        .unwrap();
    let claude_root: serde_json::Value = serde_json::from_str(&claude_content).unwrap();
    assert!(
        claude_root
            .get("mcpServers")
            .and_then(serde_json::Value::as_object)
            .unwrap()
            .contains_key("ferrus-supervisor")
    );

    let codex_content = tokio::fs::read_to_string(".codex/config.toml")
        .await
        .unwrap();
    let codex_root: toml::Table = codex_content.parse().unwrap();
    assert!(
        codex_root
            .get("mcp_servers")
            .and_then(toml::Value::as_table)
            .unwrap()
            .contains_key("ferrus-executor")
    );
    assert!(!std::path::Path::new("AGENTS.md").exists());
    assert!(!std::path::Path::new("CLAUDE.md").exists());
}
