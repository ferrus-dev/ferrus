use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::state::store;

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Running,
    Suspended,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEntry {
    pub role: String,
    pub agent_type: String,
    pub name: String,
    pub pid: Option<u32>,
    pub status: AgentStatus,
    /// When the agent was last spawned. Useful for debugging and future GUI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[allow(dead_code)]
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AgentsRegistry {
    #[serde(default)]
    pub agents: Vec<AgentEntry>,
}

impl AgentsRegistry {
    #[allow(dead_code)]
    pub fn by_role(&self, role: &str) -> Option<&AgentEntry> {
        self.agents.iter().find(|a| a.role == role)
    }

    #[allow(dead_code)]
    pub fn by_role_mut(&mut self, role: &str) -> Option<&mut AgentEntry> {
        self.agents.iter_mut().find(|a| a.role == role)
    }

    #[allow(dead_code)]
    pub fn by_name(&self, name: &str) -> Option<&AgentEntry> {
        self.agents.iter().find(|a| a.name == name)
    }

    #[allow(dead_code)]
    pub fn by_name_mut(&mut self, name: &str) -> Option<&mut AgentEntry> {
        self.agents.iter_mut().find(|a| a.name == name)
    }

    #[allow(dead_code)]
    pub fn upsert(&mut self, entry: AgentEntry) {
        if let Some(e) = self.by_name_mut(&entry.name) {
            *e = entry;
        } else {
            self.agents.push(entry);
        }
    }
}

const AGENTS_FILE: &str = ".ferrus/agents.json";

#[allow(dead_code)]
pub async fn read_agents() -> Result<AgentsRegistry> {
    let p = store::resolve_project_path(AGENTS_FILE);
    if !p.exists() {
        return Ok(AgentsRegistry::default());
    }
    let s = tokio::fs::read_to_string(p)
        .await
        .context("Failed to read .ferrus/agents.json")?;
    serde_json::from_str(&s).context("Failed to parse .ferrus/agents.json")
}

#[allow(dead_code)]
pub async fn write_agents(registry: &AgentsRegistry) -> Result<()> {
    let json = serde_json::to_string_pretty(registry)?;
    let ferrus_dir = store::resolve_project_path(".ferrus");
    tokio::fs::create_dir_all(&ferrus_dir)
        .await
        .context("Failed to create .ferrus directory")?;
    let tmp_path = store::resolve_project_path(".ferrus/agents.json.tmp");
    let dst_path = store::resolve_project_path(AGENTS_FILE);
    tokio::fs::write(&tmp_path, &json).await?;
    tokio::fs::rename(tmp_path, dst_path)
        .await
        .context("Failed to rename agents.json.tmp → agents.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let orig = std::env::current_dir()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        (dir, orig)
    }

    fn teardown(orig: String) {
        std::env::set_current_dir(orig).unwrap();
    }

    #[tokio::test]
    async fn round_trips_empty_registry() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, orig) = setup().await;
        write_agents(&AgentsRegistry::default()).await.unwrap();
        let loaded = read_agents().await.unwrap();
        assert!(loaded.agents.is_empty());
        teardown(orig);
    }

    #[tokio::test]
    async fn round_trips_agent_entry() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, orig) = setup().await;
        let entry = AgentEntry {
            role: "executor".into(),
            agent_type: "codex".into(),
            name: "executor:codex:1".into(),
            pid: Some(42),
            status: AgentStatus::Running,
            started_at: None,
        };
        let mut reg = AgentsRegistry::default();
        reg.upsert(entry);
        write_agents(&reg).await.unwrap();
        let loaded = read_agents().await.unwrap();
        assert_eq!(loaded.agents[0].pid, Some(42));
        teardown(orig);
    }

    #[tokio::test]
    async fn read_returns_default_when_absent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, orig) = setup().await;
        let loaded = read_agents().await.unwrap();
        assert!(loaded.agents.is_empty());
        teardown(orig);
    }

    #[test]
    fn upsert_updates_existing_name() {
        let mut reg = AgentsRegistry::default();
        reg.upsert(AgentEntry {
            role: "executor".into(),
            agent_type: "codex".into(),
            name: "executor:codex:1".into(),
            pid: Some(1),
            status: AgentStatus::Running,
            started_at: None,
        });
        reg.upsert(AgentEntry {
            role: "executor".into(),
            agent_type: "codex".into(),
            name: "executor:codex:1".into(),
            pid: Some(2),
            status: AgentStatus::Suspended,
            started_at: None,
        });
        assert_eq!(reg.agents.len(), 1);
        assert_eq!(reg.agents[0].pid, Some(2));
    }
}
