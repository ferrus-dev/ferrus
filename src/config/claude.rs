//! Read and initialize Claude's role-scoped MCP isolation setting.

use anyhow::{Context, Result};
use toml_edit::{DocumentMut, Item, Table, value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeMcpIsolation {
    MergeUser,
    FerrusOnly,
}

pub fn load_claude_mcp_isolation() -> ClaudeMcpIsolation {
    let Ok(contents) = std::fs::read_to_string("ferrus.toml") else {
        return ClaudeMcpIsolation::MergeUser;
    };
    let Ok(value) = contents.parse::<toml::Value>() else {
        return ClaudeMcpIsolation::MergeUser;
    };
    match value
        .get("agents")
        .and_then(|v| v.get("claude"))
        .and_then(|v| v.get("mcp_isolation"))
        .and_then(toml::Value::as_str)
    {
        Some("ferrus-only") => ClaudeMcpIsolation::FerrusOnly,
        _ => ClaudeMcpIsolation::MergeUser,
    }
}

pub async fn ensure_claude_mcp_isolation_default() -> Result<()> {
    let contents = tokio::fs::read_to_string("ferrus.toml")
        .await
        .context("ferrus.toml not found -- run `ferrus init` first")?;
    let mut doc = contents
        .parse::<DocumentMut>()
        .context("Failed to parse ferrus.toml")?;

    if !doc.contains_key("agents") {
        doc["agents"] = Item::Table(Table::new());
    }
    let agents = doc["agents"]
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("ferrus.toml [agents] must be a table"))?;
    if !agents.contains_key("claude") {
        agents["claude"] = Item::Table(Table::new());
    }
    let claude = agents["claude"]
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("ferrus.toml [agents.claude] must be a table"))?;
    if claude.contains_key("mcp_isolation") {
        return Ok(());
    }
    claude["mcp_isolation"] = value("merge-user");

    tokio::fs::write("ferrus.toml", doc.to_string())
        .await
        .context("Failed to write ferrus.toml")?;
    Ok(())
}
