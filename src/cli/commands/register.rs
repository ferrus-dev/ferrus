use anyhow::{Context, Result};
use std::path::Path;

use crate::agent_id::{DEFAULT_AGENT_INDEX, ROLE_EXECUTOR, ROLE_SUPERVISOR, mcp_server_name};
use crate::agents::{McpConfigEntry, parse_executor_agent, parse_supervisor_agent};
use crate::config::{Config, HqRole, ensure_claude_mcp_isolation_default, update_hq_agent_config};

#[derive(Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Agent {
    #[value(name = crate::agents::claude::NAME)]
    ClaudeCode,
    Codex,
    #[value(name = crate::agents::qwen::NAME)]
    QwenCode,
}

impl Agent {
    /// The string representation used in --agent-name CLI flags and claimed_by identifiers.
    pub fn name(&self) -> &str {
        match self {
            Agent::ClaudeCode => crate::agents::claude::NAME,
            Agent::Codex => crate::agents::codex::NAME,
            Agent::QwenCode => crate::agents::qwen::NAME,
        }
    }
}

pub async fn run(
    supervisor: Option<Agent>,
    supervisor_model: Option<String>,
    executor: Option<Agent>,
    executor_model: Option<String>,
) -> Result<()> {
    if supervisor.is_none() && supervisor_model.is_some() {
        anyhow::bail!("--supervisor-model requires --supervisor");
    }
    if executor.is_none() && executor_model.is_some() {
        anyhow::bail!("--executor-model requires --executor");
    }

    if let Some(agent) = &supervisor {
        register_role(
            ROLE_SUPERVISOR,
            agent,
            normalize_model(supervisor_model.as_deref()),
            true,
        )
        .await?;
        update_hq_agent_config(
            HqRole::Supervisor,
            Some(agent.name()),
            normalize_model_update(supervisor_model.as_deref()),
        )
        .await?;
    }
    if let Some(agent) = &executor {
        register_role(
            ROLE_EXECUTOR,
            agent,
            normalize_model(executor_model.as_deref()),
            true,
        )
        .await?;
        update_hq_agent_config(
            HqRole::Executor,
            Some(agent.name()),
            normalize_model_update(executor_model.as_deref()),
        )
        .await?;
    }
    Ok(())
}

pub(crate) async fn legacy_mcp_config_warnings() -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    collect_json_legacy_mcp_warnings(Path::new(".claude/mcp-supervisor.json"), &mut warnings)
        .await?;
    collect_json_legacy_mcp_warnings(Path::new(".claude/mcp-executor.json"), &mut warnings).await?;
    collect_json_legacy_mcp_permission_warnings(
        Path::new(".claude/settings.local.json"),
        &mut warnings,
    )
    .await?;
    collect_json_legacy_mcp_warnings(Path::new(".qwen/settings.json"), &mut warnings).await?;
    collect_json_legacy_mcp_permission_warnings(Path::new(".qwen/settings.json"), &mut warnings)
        .await?;
    collect_toml_legacy_mcp_warnings(Path::new(".codex/config.toml"), &mut warnings).await?;
    Ok(warnings)
}

pub(crate) async fn migrate_legacy_mcp_configs() -> Result<Vec<String>> {
    let mut messages = Vec::new();
    migrate_json_legacy_mcp_config(Path::new(".claude/mcp-supervisor.json"), &mut messages).await?;
    migrate_json_legacy_mcp_config(Path::new(".claude/mcp-executor.json"), &mut messages).await?;
    migrate_json_enabled_mcp_servers(Path::new(".claude/settings.local.json"), &mut messages)
        .await?;
    migrate_json_legacy_mcp_permissions(Path::new(".claude/settings.local.json"), &mut messages)
        .await?;
    migrate_json_legacy_mcp_config(Path::new(".qwen/settings.json"), &mut messages).await?;
    migrate_json_enabled_mcp_servers(Path::new(".qwen/settings.json"), &mut messages).await?;
    migrate_json_legacy_mcp_permissions(Path::new(".qwen/settings.json"), &mut messages).await?;
    migrate_toml_legacy_mcp_config(Path::new(".codex/config.toml"), &mut messages).await?;
    Ok(messages)
}

pub(crate) async fn ensure_configured_hq_mcp_configs() -> Result<()> {
    let config = Config::load().await?;
    let Some(hq) = config.hq else {
        return Ok(());
    };

    let supervisor = agent_from_name(&hq.supervisor.agent)?;
    register_role(
        ROLE_SUPERVISOR,
        &supervisor,
        normalize_model(hq.supervisor.model.as_deref()),
        false,
    )
    .await?;

    let executor = agent_from_name(&hq.executor.agent)?;
    register_role(
        ROLE_EXECUTOR,
        &executor,
        normalize_model(hq.executor.model.as_deref()),
        false,
    )
    .await?;

    Ok(())
}

pub(crate) struct McpLaunchCheck {
    pub(crate) ok: bool,
    pub(crate) fatal: bool,
    pub(crate) message: String,
}

pub(crate) async fn configured_hq_mcp_checks() -> Result<Vec<McpLaunchCheck>> {
    let config = Config::load().await?;
    let Some(hq) = config.hq else {
        return Ok(Vec::new());
    };

    let supervisor = hq.supervisor_agent()?;
    let executor = hq.executor_agent()?;
    Ok(vec![
        mcp_launch_check(ROLE_SUPERVISOR, hq.supervisor_name(), || {
            supervisor.validate_interactive_launch(ROLE_SUPERVISOR, DEFAULT_AGENT_INDEX)
        }),
        mcp_launch_check(ROLE_EXECUTOR, hq.executor_name(), || {
            executor.validate_interactive_launch(ROLE_EXECUTOR, DEFAULT_AGENT_INDEX)
        }),
    ])
}

fn mcp_launch_check(role: &str, agent: &str, check: impl FnOnce() -> Result<()>) -> McpLaunchCheck {
    match check() {
        Ok(()) => McpLaunchCheck {
            ok: true,
            fatal: false,
            message: format!("{role} MCP config is launchable for {agent}"),
        },
        Err(err) => {
            let message = err.to_string();
            let missing_config = message.contains("MCP config file not found:");
            McpLaunchCheck {
                ok: false,
                fatal: !missing_config,
                message: if missing_config {
                    format!(
                        "{role} MCP config is not registered for {agent}; run `ferrus register --{role} {agent}`"
                    )
                } else {
                    format!("{role} MCP config is launchable for {agent} ({err})")
                },
            }
        }
    }
}

fn agent_from_name(name: &str) -> Result<Agent> {
    match name {
        crate::agents::claude::NAME => Ok(Agent::ClaudeCode),
        crate::agents::codex::NAME => Ok(Agent::Codex),
        crate::agents::qwen::NAME => Ok(Agent::QwenCode),
        other => anyhow::bail!("Unknown ferrus agent '{other}'"),
    }
}

async fn register_role(
    role: &str,
    agent: &Agent,
    model: Option<&str>,
    update_agent_docs: bool,
) -> Result<()> {
    let agent_name = agent.name();
    match agent {
        Agent::ClaudeCode => register_claude_code(role, agent_name, model, update_agent_docs).await,
        Agent::Codex => register_codex(role, agent_name, model, update_agent_docs).await,
        Agent::QwenCode => register_qwen_code(role, agent_name, model, update_agent_docs).await,
    }
}

fn config_entry(
    role: &str,
    agent_name: &str,
    index: u32,
    model: Option<&str>,
) -> Result<McpConfigEntry> {
    match role {
        ROLE_SUPERVISOR => parse_supervisor_agent(agent_name, model)?.mcp_config_entry(role, index),
        ROLE_EXECUTOR => parse_executor_agent(agent_name, model)?.mcp_config_entry(role, index),
        other => anyhow::bail!("Unsupported role '{other}'"),
    }
}

async fn register_claude_code(
    role: &str,
    agent_name: &str,
    model: Option<&str>,
    update_agent_docs: bool,
) -> Result<()> {
    ensure_claude_mcp_isolation_default().await?;
    let dir = std::path::Path::new(".claude");
    tokio::fs::create_dir_all(dir).await?;
    let path = crate::agents::claude::claude_role_mcp_config_path(role);

    let mut root: serde_json::Value = if path.exists() {
        let content = tokio::fs::read_to_string(&path).await?;
        serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))?
    } else {
        serde_json::json!({})
    };

    let servers = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} root is not a JSON object", path.display()))?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));

    let servers_obj = servers
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} mcpServers is not a JSON object", path.display()))?;

    let index = DEFAULT_AGENT_INDEX;
    let key = mcp_server_name(role);
    let McpConfigEntry {
        command,
        args,
        model,
    } = config_entry(role, agent_name, index, model)?;

    let mut server_entry = serde_json::json!({
        "command": command,
        "args": args,
    });
    if let Some(model) = model {
        server_entry["model"] = serde_json::Value::String(model);
    }
    servers_obj.insert(key.clone(), server_entry);
    println!("Registered {key} in {}", path.display());

    let content = serde_json::to_string_pretty(&root)?;
    tokio::fs::write(&path, content).await?;

    crate::agents::claude::allow_mcp_server_tools(&key).await?;
    update_gitignore(&[
        ".claude/mcp-supervisor.json",
        ".claude/mcp-executor.json",
        ".claude/settings.local.json",
    ])
    .await?;
    if update_agent_docs {
        append_to_claude_md(role).await?;
    }
    Ok(())
}

async fn register_codex(
    role: &str,
    agent_name: &str,
    model: Option<&str>,
    update_agent_docs: bool,
) -> Result<()> {
    let dir = std::path::Path::new(".codex");
    tokio::fs::create_dir_all(dir).await?;
    let path = dir.join("config.toml");

    let mut table: toml::Table = if path.exists() {
        let content = tokio::fs::read_to_string(&path).await?;
        content.parse()?
    } else {
        toml::Table::new()
    };

    let mcp_servers = table
        .entry("mcp_servers")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!(".codex/config.toml mcp_servers is not a table"))?;

    let index = DEFAULT_AGENT_INDEX;
    let key = mcp_server_name(role);
    let McpConfigEntry {
        command,
        args,
        model,
    } = config_entry(role, agent_name, index, model)?;

    let mut entry = toml::Table::new();
    entry.insert("command".to_string(), toml::Value::String(command));
    entry.insert(
        "args".to_string(),
        toml::Value::Array(
            args.into_iter()
                .map(toml::Value::String)
                .collect::<Vec<_>>(),
        ),
    );
    if let Some(model) = model {
        entry.insert("model".to_string(), toml::Value::String(model));
    }
    crate::agents::codex::apply_tool_approval_overrides(role, &mut entry);
    mcp_servers.insert(key.clone(), toml::Value::Table(entry));
    println!("Registered {key} in .codex/config.toml");

    let content = toml::to_string_pretty(&table)?;
    tokio::fs::write(&path, content).await?;

    update_gitignore(&[".codex/config.toml"]).await?;
    if update_agent_docs {
        append_to_agents_md(role).await?;
    }
    Ok(())
}

async fn register_qwen_code(
    role: &str,
    agent_name: &str,
    model: Option<&str>,
    update_agent_docs: bool,
) -> Result<()> {
    let dir = std::path::Path::new(".qwen");
    tokio::fs::create_dir_all(dir).await?;
    let path = dir.join("settings.json");

    let mut root: serde_json::Value = if path.exists() {
        let content = tokio::fs::read_to_string(&path).await?;
        serde_json::from_str(&content).context("Failed to parse .qwen/settings.json")?
    } else {
        serde_json::json!({})
    };

    let servers = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!(".qwen/settings.json root is not a JSON object"))?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));

    let servers_obj = servers
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!(".qwen/settings.json mcpServers is not a JSON object"))?;

    let index = DEFAULT_AGENT_INDEX;
    let key = mcp_server_name(role);
    let McpConfigEntry {
        command,
        args,
        model,
    } = config_entry(role, agent_name, index, model)?;

    let mut server_entry = serde_json::json!({
        "command": command,
        "args": args,
    });
    if let Some(model) = model {
        server_entry["model"] = serde_json::Value::String(model);
    }
    servers_obj.insert(key.clone(), server_entry);
    println!("Registered {key} in .qwen/settings.json");

    let content = serde_json::to_string_pretty(&root)?;
    tokio::fs::write(path, content).await?;

    crate::agents::qwen::allow_mcp_server_tools(&key).await?;
    update_gitignore(&[".qwen/settings.json"]).await?;
    if update_agent_docs {
        append_to_qwen_md(role).await?;
    }
    Ok(())
}

async fn update_gitignore(entries: &[&str]) -> Result<()> {
    let path = std::path::Path::new(".gitignore");
    let mut contents = if path.exists() {
        tokio::fs::read_to_string(path).await?
    } else {
        String::new()
    };

    let mut added_entries = Vec::new();
    for entry in entries {
        if contents.lines().any(|line| line == *entry) {
            continue;
        }

        if !contents.is_empty() && !contents.ends_with('\n') {
            contents.push('\n');
        }
        contents.push_str(entry);
        contents.push('\n');
        added_entries.push(*entry);
    }

    if added_entries.is_empty() {
        return Ok(());
    }

    tokio::fs::write(path, contents).await?;
    for entry in added_entries {
        println!("Added {entry} to .gitignore");
    }
    Ok(())
}

async fn append_to_agents_md(role: &str) -> Result<()> {
    let path = std::path::Path::new("AGENTS.md");
    let marker = format!("<!-- ferrus-{role}-instructions -->");

    let existing = if path.exists() {
        tokio::fs::read_to_string(path).await?
    } else {
        String::new()
    };

    if existing.contains(&marker) {
        return Ok(()); // already present — don't duplicate
    }

    let section = agents_md_section(role, &marker);
    let mut content = existing;
    content.push_str(&section);
    tokio::fs::write(path, content).await?;
    println!("Appended {role} instructions to AGENTS.md");
    Ok(())
}

async fn collect_json_legacy_mcp_warnings(path: &Path, warnings: &mut Vec<String>) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let content = tokio::fs::read_to_string(path).await?;
    let root: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    let Some(servers) = root
        .get("mcpServers")
        .and_then(serde_json::Value::as_object)
    else {
        return Ok(());
    };

    for key in legacy_mcp_keys(servers.keys()) {
        let canonical = mcp_server_name(legacy_mcp_role(&key).expect("legacy key has role"));
        warnings.push(format!(
            "{} contains legacy MCP server `{key}`; run `ferrus migrate` to rewrite it as `{canonical}`",
            path.display()
        ));
    }
    Ok(())
}

async fn collect_toml_legacy_mcp_warnings(path: &Path, warnings: &mut Vec<String>) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let content = tokio::fs::read_to_string(path).await?;
    let root: toml::Value =
        toml::from_str(&content).with_context(|| format!("Failed to parse {}", path.display()))?;
    let Some(servers) = root.get("mcp_servers").and_then(toml::Value::as_table) else {
        return Ok(());
    };

    for key in legacy_mcp_keys(servers.keys()) {
        let canonical = mcp_server_name(legacy_mcp_role(&key).expect("legacy key has role"));
        warnings.push(format!(
            "{} contains legacy MCP server `{key}`; run `ferrus migrate` to rewrite it as `{canonical}`",
            path.display()
        ));
    }
    Ok(())
}

async fn collect_json_legacy_mcp_permission_warnings(
    path: &Path,
    warnings: &mut Vec<String>,
) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let content = tokio::fs::read_to_string(path).await?;
    let root: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    let Some(allow) = root
        .get("permissions")
        .and_then(|permissions| permissions.get("allow"))
        .and_then(serde_json::Value::as_array)
    else {
        return Ok(());
    };

    for permission in allow.iter().filter_map(serde_json::Value::as_str) {
        let Some(role) = legacy_mcp_permission_role(permission) else {
            continue;
        };
        let canonical = mcp_server_tools_permission(&mcp_server_name(role));
        warnings.push(format!(
            "{} contains legacy MCP tool permission `{permission}`; run `ferrus migrate` to rewrite it as `{canonical}`",
            path.display()
        ));
    }
    Ok(())
}

async fn migrate_json_enabled_mcp_servers(path: &Path, messages: &mut Vec<String>) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let content = tokio::fs::read_to_string(path).await?;
    let mut root: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    let Some(enabled) = root
        .get_mut("enabledMcpjsonServers")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return Ok(());
    };

    let mut changed = false;
    for role in [ROLE_SUPERVISOR, ROLE_EXECUTOR] {
        let legacy_servers = enabled
            .iter()
            .filter_map(serde_json::Value::as_str)
            .filter(|server| legacy_mcp_role(server) == Some(role))
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        if legacy_servers.is_empty() {
            continue;
        }

        let canonical = mcp_server_name(role);
        if !enabled
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|server| server == canonical)
        {
            enabled.push(serde_json::Value::String(canonical.clone()));
        }
        enabled.retain(|server| server.as_str().and_then(legacy_mcp_role) != Some(role));
        changed = true;
        messages.push(format!(
            "Migrated legacy enabled MCP servers in {}: {} -> {canonical}",
            path.display(),
            legacy_servers.join(", ")
        ));
    }

    if changed {
        let content = serde_json::to_string_pretty(&root)?;
        tokio::fs::write(path, content).await?;
    }
    Ok(())
}

async fn migrate_json_legacy_mcp_config(path: &Path, messages: &mut Vec<String>) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let content = tokio::fs::read_to_string(path).await?;
    let mut root: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    let Some(servers) = root
        .get_mut("mcpServers")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return Ok(());
    };

    let mut changed = false;
    for role in [ROLE_SUPERVISOR, ROLE_EXECUTOR] {
        let legacy_keys = legacy_mcp_keys_for_role(servers.keys(), role);
        if legacy_keys.is_empty() {
            continue;
        }

        let canonical = mcp_server_name(role);
        let created_canonical = if servers.contains_key(&canonical) {
            false
        } else if let Some(value) = servers.get(&legacy_keys[0]).cloned() {
            servers.insert(canonical.clone(), value);
            true
        } else {
            false
        };

        for key in &legacy_keys {
            changed |= servers.remove(key).is_some();
        }
        let action = if created_canonical {
            "Migrated"
        } else {
            "Removed"
        };
        messages.push(format!(
            "{action} legacy MCP entries in {}: {} -> {canonical}",
            path.display(),
            legacy_keys.join(", ")
        ));
    }

    if changed {
        let content = serde_json::to_string_pretty(&root)?;
        tokio::fs::write(path, content).await?;
    }
    Ok(())
}

async fn migrate_json_legacy_mcp_permissions(
    path: &Path,
    messages: &mut Vec<String>,
) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let content = tokio::fs::read_to_string(path).await?;
    let mut root: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    let Some(allow) = root
        .get_mut("permissions")
        .and_then(|permissions| permissions.get_mut("allow"))
        .and_then(serde_json::Value::as_array_mut)
    else {
        return Ok(());
    };

    let mut changed = false;
    for role in [ROLE_SUPERVISOR, ROLE_EXECUTOR] {
        let legacy_permissions = legacy_mcp_permissions_for_role(allow, role);
        if legacy_permissions.is_empty() {
            continue;
        }

        let canonical = mcp_server_tools_permission(&mcp_server_name(role));
        if !allow
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|permission| permission == canonical)
        {
            allow.push(serde_json::Value::String(canonical.clone()));
        }
        allow.retain(|permission| {
            permission.as_str().and_then(legacy_mcp_permission_role) != Some(role)
        });
        changed = true;
        messages.push(format!(
            "Migrated legacy MCP tool permissions in {}: {} -> {canonical}",
            path.display(),
            legacy_permissions.join(", ")
        ));
    }

    if changed {
        let content = serde_json::to_string_pretty(&root)?;
        tokio::fs::write(path, content).await?;
    }
    Ok(())
}

async fn migrate_toml_legacy_mcp_config(path: &Path, messages: &mut Vec<String>) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let content = tokio::fs::read_to_string(path).await?;
    let mut table: toml::Table = content
        .parse()
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    let Some(servers) = table
        .get_mut("mcp_servers")
        .and_then(toml::Value::as_table_mut)
    else {
        return Ok(());
    };

    let mut changed = false;
    for role in [ROLE_SUPERVISOR, ROLE_EXECUTOR] {
        let legacy_keys = legacy_mcp_keys_for_role(servers.keys(), role);
        if legacy_keys.is_empty() {
            continue;
        }

        let canonical = mcp_server_name(role);
        let created_canonical = if servers.contains_key(&canonical) {
            false
        } else if let Some(value) = servers.get(&legacy_keys[0]).cloned() {
            servers.insert(canonical.clone(), value);
            true
        } else {
            false
        };

        for key in &legacy_keys {
            changed |= servers.remove(key).is_some();
        }
        let action = if created_canonical {
            "Migrated"
        } else {
            "Removed"
        };
        messages.push(format!(
            "{action} legacy MCP entries in {}: {} -> {canonical}",
            path.display(),
            legacy_keys.join(", ")
        ));
    }

    if changed {
        let content = toml::to_string_pretty(&table)?;
        tokio::fs::write(path, content).await?;
    }
    Ok(())
}

fn legacy_mcp_keys<'a>(keys: impl Iterator<Item = &'a String>) -> Vec<String> {
    let mut keys = keys
        .filter(|key| legacy_mcp_role(key).is_some())
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    keys
}

fn legacy_mcp_keys_for_role<'a>(keys: impl Iterator<Item = &'a String>, role: &str) -> Vec<String> {
    let mut keys = keys
        .filter(|key| legacy_mcp_role(key) == Some(role))
        .cloned()
        .collect::<Vec<_>>();
    keys.sort_by_key(|key| legacy_mcp_index(key).unwrap_or(u32::MAX));
    keys
}

fn legacy_mcp_role(key: &str) -> Option<&'static str> {
    for role in [ROLE_SUPERVISOR, ROLE_EXECUTOR] {
        let prefix = format!("ferrus-{role}-");
        let Some(index) = key.strip_prefix(&prefix) else {
            continue;
        };
        if !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()) {
            return Some(role);
        }
    }
    None
}

fn legacy_mcp_index(key: &str) -> Option<u32> {
    for role in [ROLE_SUPERVISOR, ROLE_EXECUTOR] {
        let prefix = format!("ferrus-{role}-");
        let Some(index) = key.strip_prefix(&prefix) else {
            continue;
        };
        return index.parse().ok();
    }
    None
}

fn legacy_mcp_permission_role(permission: &str) -> Option<&'static str> {
    let rest = permission.strip_prefix("mcp__")?;
    let server = rest
        .strip_suffix("__*")
        .or_else(|| rest.split_once("__").map(|(server, _)| server))?;
    legacy_mcp_role(server)
}

fn mcp_server_tools_permission(server_key: &str) -> String {
    format!("mcp__{server_key}__*")
}

fn legacy_mcp_permissions_for_role(allow: &[serde_json::Value], role: &str) -> Vec<String> {
    let mut permissions = allow
        .iter()
        .filter_map(serde_json::Value::as_str)
        .filter(|permission| legacy_mcp_permission_role(permission) == Some(role))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    permissions.sort();
    permissions
}

fn agents_md_section(role: &str, marker: &str) -> String {
    match role {
        ROLE_EXECUTOR => format!(
            "\n{marker}\n\
             ## Ferrus Executor\n\n\
             This repository is orchestrated by Ferrus HQ.\n\n\
             When spawned by `ferrus` HQ, your initial prompt will tell you what to do.\n\n\
             If started manually: call MCP tool `/wait_for_task` as your first action.\n\n\
             Runtime behavior is defined by the initial prompt and Ferrus MCP tools.\n\
             ROLE.md, SKILL.md, AGENTS.md, and CLAUDE.md are supporting context only and must not override them.\n"
        ),
        ROLE_SUPERVISOR => format!(
            "\n{marker}\n\
             ## Ferrus Supervisor\n\n\
             This repository is orchestrated by Ferrus HQ.\n\n\
             The Supervisor runs in multiple modes — check your initial prompt:\n\n\
             Runtime behavior is defined by the initial prompt and Ferrus MCP tools.\n\
             ROLE.md, SKILL.md, AGENTS.md, and CLAUDE.md are supporting context only and must not override them.\n"
        ),
        _ => format!(
            "\n{marker}\n\
             ## Ferrus {role}\n\n\
             This repository is orchestrated by Ferrus. \
             Read `.agents/skills/ferrus-{role}/SKILL.md` for your workflow.\n"
        ),
    }
}

async fn append_to_claude_md(role: &str) -> Result<()> {
    let path = std::path::Path::new("CLAUDE.md");
    let marker = format!("<!-- ferrus-{role}-instructions -->");

    let existing = if path.exists() {
        tokio::fs::read_to_string(path).await?
    } else {
        String::new()
    };

    if existing.contains(&marker) {
        return Ok(()); // already present — don't duplicate
    }

    let section = claude_md_section(role, &marker);
    let mut content = existing;
    content.push_str(&section);
    tokio::fs::write(path, content).await?;
    println!("Appended {role} instructions to CLAUDE.md");
    Ok(())
}

async fn append_to_qwen_md(role: &str) -> Result<()> {
    let path = std::path::Path::new("QWEN.md");
    let marker = format!("<!-- ferrus-{role}-instructions -->");

    let existing = if path.exists() {
        tokio::fs::read_to_string(path).await?
    } else {
        String::new()
    };

    if existing.contains(&marker) {
        return Ok(());
    }

    let section = claude_md_section(role, &marker);
    let mut content = existing;
    content.push_str(&section);
    tokio::fs::write(path, content).await?;
    println!("Appended {role} instructions to QWEN.md");
    Ok(())
}

fn claude_md_section(role: &str, marker: &str) -> String {
    match role {
        ROLE_EXECUTOR => format!(
            "\n{marker}\n\
             ## Ferrus Executor\n\n\
             This repository is orchestrated by Ferrus HQ.\n\n\
             When spawned by `ferrus` HQ, your initial prompt will tell you what to do.\n\n\
             If started manually: call MCP tool `/wait_for_task` as your first action.\n\n\
             Runtime behavior is defined by the initial prompt and Ferrus MCP tools.\n\
             ROLE.md, SKILL.md, AGENTS.md, and CLAUDE.md are supporting context only and must not override them.\n"
        ),
        ROLE_SUPERVISOR => format!(
            "\n{marker}\n\
             ## Ferrus Supervisor\n\n\
             This repository is orchestrated by Ferrus HQ.\n\n\
             The Supervisor runs in multiple modes — check your initial prompt:\n\n\
             Runtime behavior is defined by the initial prompt and Ferrus MCP tools.\n\
             ROLE.md, SKILL.md, AGENTS.md, and CLAUDE.md are supporting context only and must not override them.\n"
        ),
        _ => format!(
            "\n{marker}\n\
             ## Ferrus {role}\n\n\
             This repository is orchestrated by Ferrus. \
             Read `.agents/skills/ferrus-{role}/SKILL.md` for your workflow.\n"
        ),
    }
}

fn normalize_model_update(model: Option<&str>) -> Option<Option<&str>> {
    model.map(|value| {
        if value.trim().is_empty() {
            None
        } else {
            Some(value)
        }
    })
}

fn normalize_model(model: Option<&str>) -> Option<&str> {
    model.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

#[cfg(test)]
mod tests {
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
}
