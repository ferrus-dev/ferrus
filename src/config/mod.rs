//! Load ferrus.toml defaults and update HQ agent settings while preserving TOML layout.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;
use toml_edit::{DocumentMut, Item, Table, value};

use crate::agents::{ExecutorAgent, SupervisorAgent, parse_executor_agent, parse_supervisor_agent};
mod claude;
pub use claude::{
    ClaudeMcpIsolation, ensure_claude_mcp_isolation_default, load_claude_mcp_isolation,
};

#[derive(Debug)]
#[allow(dead_code)]
pub struct Config {
    pub checks: ChecksConfig,
    pub limits: LimitsConfig,
    pub lease: LeaseConfig,
    pub spec: SpecConfig,
    pub hq: Option<HqConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ChecksConfig {
    #[serde(default)]
    pub commands: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct LimitsConfig {
    #[serde(default = "default_max_check_retries")]
    pub max_check_retries: u32,
    #[serde(default = "default_max_review_cycles")]
    pub max_review_cycles: u32,
    /// Maximum number of trailing lines shown per failing command in /check and /submit output.
    /// The full output is always written to .ferrus/logs/.
    #[serde(default = "default_max_feedback_lines")]
    pub max_feedback_lines: usize,
    /// Maximum duration (in seconds) of a single wait_* MCP tool call before it
    /// returns timeout so the agent can poll again.
    #[serde(default = "default_wait_timeout_secs")]
    pub wait_timeout_secs: u64,
    /// Maximum number of executor sessions HQ may run at the same time.
    #[serde(default = "default_max_parallel_tasks")]
    pub max_parallel_tasks: usize,
    /// Maximum number of executor sessions HQ will dispatch for a single task
    /// within one work phase before giving up and marking it Failed. A session
    /// that hits its turn limit and exits without submitting is respawned; this
    /// bounds that respawn loop so a task that never reaches review cannot churn
    /// forever. The counter resets when a new work phase begins (a fresh
    /// rejection back to Addressing).
    #[serde(default = "default_max_executor_dispatches")]
    pub max_executor_dispatches: u32,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct LeaseConfig {
    /// How long (in seconds) a claimed lease is valid without renewal.
    #[serde(default = "default_ttl_secs")]
    pub ttl_secs: u64,
    /// How often (in seconds) agents should call /heartbeat. Informational -- not enforced server-side.
    #[serde(default = "default_heartbeat_interval_secs")]
    pub heartbeat_interval_secs: u64,
}

#[derive(Debug, Deserialize)]
pub struct SpecConfig {
    /// Directory where `/create_spec` writes approved feature specifications.
    #[serde(default = "default_spec_directory")]
    pub directory: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct HqAgentConfig {
    pub agent: String,
    #[serde(default, deserialize_with = "deserialize_optional_model")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HqConfig {
    pub supervisor: HqAgentConfig,
    pub executor: HqAgentConfig,
}

// Optional graph/memory settings are validated by their own entry points.
// Keeping this shape lenient lets ordinary orchestration run if those settings are invalid.
#[derive(Debug, Deserialize)]
struct RawConfig {
    checks: ChecksConfig,
    limits: LimitsConfig,
    #[serde(default)]
    lease: LeaseConfig,
    #[serde(default)]
    spec: SpecConfig,
    #[serde(default)]
    hq: Option<RawHqConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawHqConfig {
    Nested {
        supervisor: Option<HqAgentConfig>,
        executor: Option<HqAgentConfig>,
    },
    Flat {
        supervisor: String,
        executor: String,
    },
}

impl HqConfig {
    pub fn supervisor_name(&self) -> &str {
        &self.supervisor.agent
    }

    pub fn executor_name(&self) -> &str {
        &self.executor.agent
    }

    pub fn supervisor_agent(&self) -> Result<std::sync::Arc<dyn SupervisorAgent>> {
        parse_supervisor_agent(&self.supervisor.agent, self.supervisor.model.as_deref())
    }

    pub fn executor_agent(&self) -> Result<std::sync::Arc<dyn ExecutorAgent>> {
        parse_executor_agent(&self.executor.agent, self.executor.model.as_deref())
    }
}

impl TryFrom<RawConfig> for Config {
    type Error = anyhow::Error;

    fn try_from(raw: RawConfig) -> Result<Self> {
        let hq = raw.hq.map(hq_config_from_raw).transpose()?.flatten();
        Ok(Self {
            checks: raw.checks,
            limits: raw.limits,
            lease: raw.lease,
            spec: raw.spec,
            hq,
        })
    }
}

fn hq_config_from_raw(raw: RawHqConfig) -> Result<Option<HqConfig>> {
    let Some((supervisor, executor)) = hq_agents_from_raw(raw) else {
        return Ok(None);
    };
    parse_supervisor_agent(&supervisor.agent, supervisor.model.as_deref())?;
    parse_executor_agent(&executor.agent, executor.model.as_deref())?;
    Ok(Some(HqConfig {
        supervisor,
        executor,
    }))
}

fn hq_agents_from_raw(raw: RawHqConfig) -> Option<(HqAgentConfig, HqAgentConfig)> {
    match raw {
        RawHqConfig::Nested {
            supervisor,
            executor,
        } => Some((supervisor?, executor?)),
        RawHqConfig::Flat {
            supervisor,
            executor,
        } => Some((
            HqAgentConfig {
                agent: supervisor,
                model: None,
            },
            HqAgentConfig {
                agent: executor,
                model: None,
            },
        )),
    }
}

impl TryFrom<RawHqConfig> for HqConfig {
    type Error = anyhow::Error;

    fn try_from(raw: RawHqConfig) -> Result<Self> {
        hq_config_from_raw(raw)?.ok_or_else(|| {
            anyhow::anyhow!("ferrus.toml [hq] must configure both supervisor and executor")
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HqRole {
    Supervisor,
    Executor,
}

impl HqRole {
    pub fn table_name(self) -> &'static str {
        match self {
            Self::Supervisor => "supervisor",
            Self::Executor => "executor",
        }
    }
}

impl Default for LeaseConfig {
    fn default() -> Self {
        Self {
            ttl_secs: default_ttl_secs(),
            heartbeat_interval_secs: default_heartbeat_interval_secs(),
        }
    }
}

impl Default for SpecConfig {
    fn default() -> Self {
        Self {
            directory: default_spec_directory(),
        }
    }
}

const fn default_max_check_retries() -> u32 {
    20
}
const fn default_max_review_cycles() -> u32 {
    3
}
const fn default_max_feedback_lines() -> usize {
    30
}
const fn default_wait_timeout_secs() -> u64 {
    60
}
const fn default_max_parallel_tasks() -> usize {
    1
}
const fn default_max_executor_dispatches() -> u32 {
    6
}
const fn default_ttl_secs() -> u64 {
    90
}
const fn default_heartbeat_interval_secs() -> u64 {
    30
}
fn default_spec_directory() -> String {
    "docs/specs".to_string()
}

impl Config {
    pub async fn load() -> Result<Self> {
        Self::load_from(Path::new(".")).await
    }

    pub async fn load_from(project_root: &Path) -> Result<Self> {
        let path = project_root.join("ferrus.toml");
        let contents = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("{} not found -- run `ferrus init` first", path.display()))?;
        Self::from_toml(&contents)
    }

    fn from_toml(contents: &str) -> Result<Self> {
        let raw: RawConfig = toml::from_str(contents).context("Failed to parse ferrus.toml")?;
        raw.try_into()
    }
}

pub async fn update_hq_agent_config(
    role: HqRole,
    agent: Option<&str>,
    model: Option<Option<&str>>,
) -> Result<()> {
    let contents = tokio::fs::read_to_string("ferrus.toml")
        .await
        .context("ferrus.toml not found -- run `ferrus init` first")?;
    let updated = update_hq_agent_config_in_contents(&contents, role, agent, model)?;
    tokio::fs::write("ferrus.toml", updated)
        .await
        .context("Failed to write ferrus.toml")?;
    Ok(())
}

fn update_hq_agent_config_in_contents(
    contents: &str,
    role: HqRole,
    agent: Option<&str>,
    model: Option<Option<&str>>,
) -> Result<String> {
    let mut doc = contents
        .parse::<DocumentMut>()
        .context("Failed to parse ferrus.toml")?;

    if !doc.contains_key("hq") {
        doc["hq"] = Item::Table(Table::new());
    }
    let hq = doc["hq"]
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("ferrus.toml [hq] must be a table"))?;

    let flat_supervisor = hq
        .get("supervisor")
        .and_then(Item::as_str)
        .map(str::to_string);
    let flat_executor = hq
        .get("executor")
        .and_then(Item::as_str)
        .map(str::to_string);
    if flat_supervisor.is_some() {
        hq.remove("supervisor");
    }
    if flat_executor.is_some() {
        hq.remove("executor");
    }

    let table_name = role.table_name();
    if !hq.contains_key(table_name) {
        hq[table_name] = Item::Table(Table::new());
    }
    if flat_supervisor.is_some() && !hq.contains_key(HqRole::Supervisor.table_name()) {
        hq[HqRole::Supervisor.table_name()] = Item::Table(Table::new());
    }
    if flat_executor.is_some() && !hq.contains_key(HqRole::Executor.table_name()) {
        hq[HqRole::Executor.table_name()] = Item::Table(Table::new());
    }
    if let Some(supervisor) = flat_supervisor {
        let supervisor_table = hq[HqRole::Supervisor.table_name()]
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("ferrus.toml [hq.supervisor] must be a table"))?;
        if !supervisor_table.contains_key("agent") {
            supervisor_table["agent"] = value(supervisor);
        }
    }
    if let Some(executor) = flat_executor {
        let executor_table = hq[HqRole::Executor.table_name()]
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("ferrus.toml [hq.executor] must be a table"))?;
        if !executor_table.contains_key("agent") {
            executor_table["agent"] = value(executor);
        }
    }
    let agent_table = hq[table_name]
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("ferrus.toml [hq.{table_name}] must be a table"))?;

    if let Some(agent) = agent {
        agent_table["agent"] = value(agent);
    }
    if let Some(model) = model {
        agent_table["model"] = value(model.unwrap_or(""));
    }

    Ok(doc.to_string())
}

fn deserialize_optional_model<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let model = Option::<String>::deserialize(deserializer)?;
    Ok(model.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }))
}

#[cfg(test)]
mod tests {
    //! Configuration defaults, legacy HQ formats, and lenient optional settings.

    use super::*;

    #[test]
    fn lease_config_defaults_without_block() {
        let toml = r#"
[checks]
commands = ["cargo test"]

[limits]
"#;
        let config = Config::from_toml(toml).unwrap();
        assert_eq!(config.lease.ttl_secs, 90);
        assert_eq!(config.lease.heartbeat_interval_secs, 30);
        assert_eq!(config.spec.directory, "docs/specs");
    }

    #[test]
    fn core_config_ignores_unknown_repository_graph_settings() {
        let toml = r#"
[checks]
commands = ["cargo test"]

[limits]

[repository_graph]
enabled = true
future_backend = "distributed"

[repository_graph.future_projection]
model = "next-generation"
"#;

        let config = Config::from_toml(toml).unwrap();

        assert_eq!(config.checks.commands, vec!["cargo test"]);
    }

    #[test]
    fn limits_config_defaults_without_values() {
        let toml = r#"
[checks]
commands = ["cargo test"]

[limits]
"#;
        let config = Config::from_toml(toml).unwrap();

        assert_eq!(config.limits.max_check_retries, 20);
        assert_eq!(config.limits.max_review_cycles, 3);
        assert_eq!(config.limits.max_feedback_lines, 30);
        assert_eq!(config.limits.wait_timeout_secs, 60);
        assert_eq!(config.limits.max_parallel_tasks, 1);
        assert_eq!(config.limits.max_executor_dispatches, 6);
    }

    #[test]
    fn config_round_trips_explicit_values() {
        let toml = r#"
[checks]
commands = ["cargo test", "cargo clippy -- -D warnings"]

[limits]
max_check_retries = 7
max_review_cycles = 4
max_feedback_lines = 12
wait_timeout_secs = 900
max_parallel_tasks = 3

[lease]
ttl_secs = 120
heartbeat_interval_secs = 45

[spec]
directory = "docs/feature-specs"
"#;
        let config = Config::from_toml(toml).unwrap();

        assert_eq!(
            config.checks.commands,
            vec![
                "cargo test".to_string(),
                "cargo clippy -- -D warnings".to_string()
            ]
        );
        assert_eq!(config.limits.max_check_retries, 7);
        assert_eq!(config.limits.max_review_cycles, 4);
        assert_eq!(config.limits.max_feedback_lines, 12);
        assert_eq!(config.limits.wait_timeout_secs, 900);
        assert_eq!(config.limits.max_parallel_tasks, 3);
        assert_eq!(config.lease.ttl_secs, 120);
        assert_eq!(config.lease.heartbeat_interval_secs, 45);
        assert_eq!(config.spec.directory, "docs/feature-specs");
    }

    #[test]
    fn checks_commands_default_to_empty_when_omitted() {
        let toml = r#"
[checks]

[limits]
"#;
        let config = Config::from_toml(toml).unwrap();

        assert!(config.checks.commands.is_empty());
    }

    #[test]
    fn hq_config_absent_gives_none() {
        let toml = "[checks]\ncommands = [\"cargo test\"]\n[limits]\n";
        let config = Config::from_toml(toml).unwrap();
        assert!(config.hq.is_none());
    }

    #[test]
    fn partial_nested_hq_config_gives_none() {
        let toml = "[checks]\ncommands = [\"cargo test\"]\n[limits]\n[hq.supervisor]\nagent = \"claude-code\"\n";
        let config = Config::from_toml(toml).unwrap();
        assert!(config.hq.is_none());
    }

    #[test]
    fn hq_config_parses_when_present() {
        let toml = "[checks]\ncommands = [\"cargo test\"]\n[limits]\n[hq.supervisor]\nagent = \"claude-code\"\nmodel = \"\"\n[hq.executor]\nagent = \"codex\"\nmodel = \"gpt-5.4\"\n";
        let config = Config::from_toml(toml).unwrap();
        let hq = config.hq.unwrap();
        assert_eq!(hq.supervisor_name(), "claude-code");
        assert_eq!(hq.executor_name(), "codex");
        assert_eq!(hq.supervisor.model, None);
        assert_eq!(hq.executor.model.as_deref(), Some("gpt-5.4"));
    }

    #[test]
    fn invalid_hq_agent_is_rejected_at_load_time() {
        let toml = "[checks]\ncommands = [\"cargo test\"]\n[limits]\n[hq.supervisor]\nagent = \"unknown\"\n[hq.executor]\nagent = \"codex\"\n";
        let err = Config::from_toml(toml).unwrap_err().to_string();
        assert!(err.contains("Unknown supervisor agent 'unknown'"));
    }

    #[test]
    fn flat_hq_config_still_parses_for_backward_compatibility() {
        let toml = "[checks]\ncommands = [\"cargo test\"]\n[limits]\n[hq]\nsupervisor = \"claude-code\"\nexecutor = \"codex\"\n";
        let config = Config::from_toml(toml).unwrap();
        let hq = config.hq.unwrap();
        assert_eq!(hq.supervisor.agent, "claude-code");
        assert_eq!(hq.executor.agent, "codex");
        assert_eq!(hq.supervisor.model, None);
        assert_eq!(hq.executor.model, None);
    }

    #[test]
    fn missing_model_deserializes_to_none() {
        let toml = "[checks]\ncommands = [\"cargo test\"]\n[limits]\n[hq.supervisor]\nagent = \"claude-code\"\n[hq.executor]\nagent = \"codex\"\n";
        let config = Config::from_toml(toml).unwrap();
        let hq = config.hq.unwrap();
        assert_eq!(hq.supervisor.model, None);
        assert_eq!(hq.executor.model, None);
    }

    #[test]
    fn update_hq_agent_config_only_changes_requested_fields() {
        let toml = "[checks]\ncommands = [\"cargo test\"]\n[limits]\n[hq.supervisor]\nagent = \"claude-code\"\nmodel = \"\"\n[hq.executor]\nagent = \"codex\"\nmodel = \"gpt-5.4\"\n";
        let updated = update_hq_agent_config_in_contents(
            toml,
            HqRole::Supervisor,
            None,
            Some(Some("claude-opus-4-6")),
        )
        .unwrap();

        let config = Config::from_toml(&updated).unwrap();
        let hq = config.hq.unwrap();
        assert_eq!(hq.supervisor.model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(hq.executor.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(hq.supervisor.agent, "claude-code");
    }

    #[test]
    fn update_hq_agent_config_one_sided_does_not_create_empty_opposite_table() {
        let toml = "[checks]\ncommands = [\"cargo test\"]\n[limits]\n";
        let updated =
            update_hq_agent_config_in_contents(toml, HqRole::Supervisor, Some("claude-code"), None)
                .unwrap();

        assert!(updated.contains("[hq.supervisor]"));
        assert!(!updated.contains("[hq.executor]"));
        let config = Config::from_toml(&updated).unwrap();
        assert!(config.hq.is_none());

        let completed =
            update_hq_agent_config_in_contents(&updated, HqRole::Executor, Some("codex"), None)
                .unwrap();
        let config = Config::from_toml(&completed).unwrap();
        let hq = config.hq.unwrap();
        assert_eq!(hq.supervisor.agent, "claude-code");
        assert_eq!(hq.executor.agent, "codex");
    }

    #[test]
    fn update_hq_agent_config_migrates_flat_hq_tables() {
        let toml = "[checks]\ncommands = [\"cargo test\"]\n[limits]\n[hq]\nsupervisor = \"claude-code\"\nexecutor = \"codex\"\n";
        let updated =
            update_hq_agent_config_in_contents(toml, HqRole::Executor, None, Some(Some("gpt-5.4")))
                .unwrap();

        assert!(updated.contains("[hq.supervisor]"));
        assert!(updated.contains("[hq.executor]"));
        assert!(!updated.contains("\nsupervisor = \"claude-code\"\nexecutor = \"codex\""));

        let config = Config::from_toml(&updated).unwrap();
        let hq = config.hq.unwrap();
        assert_eq!(hq.supervisor.agent, "claude-code");
        assert_eq!(hq.executor.agent, "codex");
        assert_eq!(hq.executor.model.as_deref(), Some("gpt-5.4"));
    }
}
