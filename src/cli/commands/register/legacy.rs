//! Migration of legacy MCP registrations and permissions.

use super::*;

pub(super) async fn collect_json_legacy_mcp_warnings(
    path: &Path,
    warnings: &mut Vec<String>,
) -> Result<()> {
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
        let Some(role) = legacy_mcp_role(&key) else {
            continue;
        };
        let canonical = mcp_server_name(role);
        warnings.push(format!(
            "{} contains legacy MCP server `{key}`; run `ferrus migrate` to rewrite it as `{canonical}`",
            path.display()
        ));
    }

    Ok(())
}

pub(super) async fn collect_toml_legacy_mcp_warnings(
    path: &Path,
    warnings: &mut Vec<String>,
) -> Result<()> {
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
        let Some(role) = legacy_mcp_role(&key) else {
            continue;
        };
        let canonical = mcp_server_name(role);
        warnings.push(format!(
            "{} contains legacy MCP server `{key}`; run `ferrus migrate` to rewrite it as `{canonical}`",
            path.display()
        ));
    }

    Ok(())
}

pub(super) async fn collect_json_legacy_mcp_permission_warnings(
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

pub(super) async fn migrate_json_enabled_mcp_servers(
    path: &Path,
    messages: &mut Vec<String>,
) -> Result<()> {
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

pub(super) async fn migrate_json_legacy_mcp_config(
    path: &Path,
    messages: &mut Vec<String>,
) -> Result<()> {
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

pub(super) async fn migrate_json_legacy_mcp_permissions(
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

pub(super) async fn migrate_toml_legacy_mcp_config(
    path: &Path,
    messages: &mut Vec<String>,
) -> Result<()> {
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

pub(super) fn legacy_mcp_keys<'a>(keys: impl Iterator<Item = &'a String>) -> Vec<String> {
    let mut keys = keys
        .filter(|key| legacy_mcp_role(key).is_some())
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    keys
}

pub(super) fn legacy_mcp_keys_for_role<'a>(
    keys: impl Iterator<Item = &'a String>,
    role: &str,
) -> Vec<String> {
    let mut keys = keys
        .filter(|key| legacy_mcp_role(key) == Some(role))
        .cloned()
        .collect::<Vec<_>>();
    keys.sort_by_key(|key| legacy_mcp_index(key).unwrap_or(u32::MAX));
    keys
}

pub(super) fn legacy_mcp_role(key: &str) -> Option<&'static str> {
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

pub(super) fn legacy_mcp_index(key: &str) -> Option<u32> {
    for role in [ROLE_SUPERVISOR, ROLE_EXECUTOR] {
        let prefix = format!("ferrus-{role}-");
        let Some(index) = key.strip_prefix(&prefix) else {
            continue;
        };
        return index.parse().ok();
    }

    None
}

pub(super) fn legacy_mcp_permission_role(permission: &str) -> Option<&'static str> {
    let rest = permission.strip_prefix("mcp__")?;
    let server = rest
        .strip_suffix("__*")
        .or_else(|| rest.split_once("__").map(|(server, _)| server))?;
    legacy_mcp_role(server)
}

pub(super) fn mcp_server_tools_permission(server_key: &str) -> String {
    format!("mcp__{server_key}__*")
}

pub(super) fn legacy_mcp_permissions_for_role(
    allow: &[serde_json::Value],
    role: &str,
) -> Vec<String> {
    let mut permissions = allow
        .iter()
        .filter_map(serde_json::Value::as_str)
        .filter(|permission| legacy_mcp_permission_role(permission) == Some(role))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    permissions.sort();
    permissions
}
