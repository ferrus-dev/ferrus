use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use super::domain::Digest;

const CONFIG_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RepositoryGraphConfigError {
    #[error("repository graph pattern must not be empty")]
    EmptyPattern,
    #[error("repository graph pattern must be repository-relative: {0}")]
    NonRelativePattern(String),
    #[error("failed to serialize effective repository graph configuration: {0}")]
    Serialization(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphBackend {
    #[default]
    Local,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepositoryGraphConfig {
    /// Operational capability switch; excluded from structural snapshot identity.
    pub enabled: bool,
    /// Operational storage selection; excluded from structural snapshot identity.
    pub backend: GraphBackend,
    pub source: SourceConfig,
    pub analyzers: AnalyzersConfig,
    pub index_limits: IndexLimitsConfig,
    pub query_limits: QueryLimitsConfig,
    pub retention: RetentionConfig,
    pub memory: MemoryConfig,
    pub semantic: SemanticConfig,
    pub remote: RemoteConfig,
    pub telemetry: TelemetryConfig,
}

#[derive(Debug, Deserialize)]
struct FerrusConfigDocument {
    #[serde(default)]
    repository_graph: RepositoryGraphConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SourceConfig {
    /// Set-like include patterns. Order does not affect identity.
    pub include: BTreeSet<String>,
    /// Ordered ignore/negation rules. Order is semantically significant.
    pub rules: Vec<String>,
    pub include_untracked: bool,
    pub include_generated: bool,
    pub include_vendor: bool,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            include: BTreeSet::from(["**/*".to_string()]),
            rules: vec![
                ".git/**".to_string(),
                ".ferrus/**".to_string(),
                "target/**".to_string(),
            ],
            include_untracked: true,
            include_generated: false,
            include_vendor: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnalyzersConfig {
    /// Empty means the built-in default extractor set for this Ferrus version.
    pub enabled: BTreeSet<String>,
    pub settings: BTreeMap<String, AnalyzerSettings>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnalyzerSettings {
    pub options: BTreeMap<String, ConfigScalar>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConfigScalar {
    Boolean(bool),
    Integer(i64),
    String(String),
    StringList(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IndexLimitsConfig {
    pub max_files: u64,
    pub max_file_bytes: u64,
    pub max_facts_per_file: u64,
}

impl Default for IndexLimitsConfig {
    fn default() -> Self {
        Self {
            max_files: 100_000,
            max_file_bytes: 2 * 1024 * 1024,
            max_facts_per_file: 100_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueryLimitsConfig {
    pub max_results: u32,
    pub max_bytes: u64,
    pub max_depth: u32,
    pub max_duration_ms: u64,
}

impl Default for QueryLimitsConfig {
    fn default() -> Self {
        Self {
            max_results: 100,
            max_bytes: 256 * 1024,
            max_depth: 3,
            max_duration_ms: 2_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    pub max_snapshots: u32,
    pub max_failed_builds: u32,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            max_snapshots: 5,
            max_failed_builds: 10,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryConfig {
    pub enabled: bool,
    pub sources: BTreeSet<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SemanticConfig {
    pub enabled: bool,
    pub model: Option<String>,
    pub chunker_version: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteConfig {
    /// Endpoint is operational and never included in a structural digest.
    pub endpoint: Option<String>,
    /// Name of an external credential source, never the credential itself.
    pub credential_ref: Option<String>,
    pub upload_enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelemetryConfig {
    pub enabled: bool,
}

#[derive(Serialize)]
struct EffectiveSemanticConfig {
    schema_version: u32,
    source: EffectiveSourceConfig,
    analyzers: AnalyzersConfig,
    index_limits: IndexLimitsConfig,
}

#[derive(Serialize)]
struct EffectiveSourceConfig {
    include: BTreeSet<String>,
    rules: Vec<String>,
    include_untracked: bool,
    include_generated: bool,
    include_vendor: bool,
}

impl RepositoryGraphConfig {
    /// Parses the optional graph namespace from a complete `ferrus.toml` document.
    ///
    /// Core orchestration deliberately ignores this namespace so a newer graph
    /// schema cannot disable task execution. Graph operations use this strict
    /// boundary and report unsupported settings when the capability is invoked.
    pub fn from_ferrus_toml(contents: &str) -> Result<Self, toml::de::Error> {
        let document: FerrusConfigDocument = toml::from_str(contents)?;
        Ok(document.repository_graph)
    }

    /// Returns the canonical structural projection used in snapshot identity.
    /// Operational, memory, and future semantic-projection settings are omitted.
    pub fn analysis_config_digest(&self) -> Result<Digest, RepositoryGraphConfigError> {
        let effective = self.effective_semantic_config()?;
        let bytes = serde_json::to_vec(&effective)
            .map_err(|error| RepositoryGraphConfigError::Serialization(error.to_string()))?;
        let value = Sha256::digest(bytes);
        Ok(Digest {
            algorithm: "sha256".to_string(),
            value: hex_lower(&value),
        })
    }

    fn effective_semantic_config(
        &self,
    ) -> Result<EffectiveSemanticConfig, RepositoryGraphConfigError> {
        let include = self
            .source
            .include
            .iter()
            .map(|pattern| normalize_pattern(pattern))
            .collect::<Result<BTreeSet<_>, _>>()?;
        let rules = self
            .source
            .rules
            .iter()
            .map(|pattern| normalize_pattern(pattern))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(EffectiveSemanticConfig {
            schema_version: CONFIG_SCHEMA_VERSION,
            source: EffectiveSourceConfig {
                include,
                rules,
                include_untracked: self.source.include_untracked,
                include_generated: self.source.include_generated,
                include_vendor: self.source.include_vendor,
            },
            analyzers: self.analyzers.clone(),
            index_limits: self.index_limits.clone(),
        })
    }
}

fn normalize_pattern(pattern: &str) -> Result<String, RepositoryGraphConfigError> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return Err(RepositoryGraphConfigError::EmptyPattern);
    }
    let (negated, body) = pattern
        .strip_prefix('!')
        .map_or((false, pattern), |body| (true, body));
    let body = body.replace('\\', "/");
    let body = body.strip_prefix("./").unwrap_or(&body);
    if body.is_empty()
        || body.starts_with('/')
        || body
            .as_bytes()
            .get(1)
            .is_some_and(|separator| *separator == b':')
        || body.split('/').any(|component| component == "..")
    {
        return Err(RepositoryGraphConfigError::NonRelativePattern(
            pattern.to_string(),
        ));
    }
    Ok(if negated {
        format!("!{body}")
    } else {
        body.to_string()
    })
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_and_explicit_defaults_have_the_same_digest() {
        let missing: RepositoryGraphConfig = toml::from_str("").unwrap();
        let explicit: RepositoryGraphConfig = toml::from_str(
            r#"
enabled = false
backend = "local"

[source]
include = ["**/*"]
rules = [".git/**", ".ferrus/**", "target/**"]
include_untracked = true
include_generated = false
include_vendor = false

[analyzers]
enabled = []
settings = {}

[index_limits]
max_files = 100000
max_file_bytes = 2097152
max_facts_per_file = 100000

[query_limits]
max_results = 100
max_bytes = 262144
max_depth = 3
max_duration_ms = 2000

[retention]
max_snapshots = 5
max_failed_builds = 10

[memory]
enabled = false
sources = []

[semantic]
enabled = false

[remote]
upload_enabled = false

[telemetry]
enabled = false
"#,
        )
        .unwrap();
        assert_eq!(
            missing.analysis_config_digest().unwrap(),
            explicit.analysis_config_digest().unwrap()
        );
    }

    #[test]
    fn set_order_and_platform_separators_do_not_change_digest() {
        let left: RepositoryGraphConfig = toml::from_str(
            r#"[source]
include = ["src\\**", "Cargo.toml"]
"#,
        )
        .unwrap();
        let right: RepositoryGraphConfig = toml::from_str(
            r#"[source]
include = ["Cargo.toml", "src/**"]
"#,
        )
        .unwrap();
        assert_eq!(
            left.analysis_config_digest().unwrap(),
            right.analysis_config_digest().unwrap()
        );
    }

    #[test]
    fn operational_and_secret_references_do_not_change_digest() {
        let baseline = RepositoryGraphConfig::default();
        let mut operational = baseline.clone();
        operational.enabled = true;
        operational.query_limits.max_results = 1;
        operational.retention.max_snapshots = 99;
        operational.remote.endpoint = Some("https://example.invalid".to_string());
        operational.remote.credential_ref = Some("FERRUS_GRAPH_TOKEN".to_string());
        operational.telemetry.enabled = true;
        assert_eq!(
            baseline.analysis_config_digest().unwrap(),
            operational.analysis_config_digest().unwrap()
        );
    }

    #[test]
    fn ordered_source_rules_change_digest() {
        let mut left = RepositoryGraphConfig::default();
        left.source.rules = vec!["vendor/**".to_string(), "!vendor/keep/**".to_string()];
        let mut right = left.clone();
        right.source.rules.reverse();
        assert_ne!(
            left.analysis_config_digest().unwrap(),
            right.analysis_config_digest().unwrap()
        );
    }

    #[test]
    fn unknown_setting_is_rejected() {
        let error = toml::from_str::<RepositoryGraphConfig>("mystery = true").unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn graph_config_parser_defaults_when_namespace_is_absent() {
        let config = RepositoryGraphConfig::from_ferrus_toml(
            r#"
[checks]
commands = ["cargo test"]

[limits]
"#,
        )
        .unwrap();

        assert_eq!(config, RepositoryGraphConfig::default());
    }

    #[test]
    fn graph_config_parser_rejects_unknown_graph_settings() {
        let error = RepositoryGraphConfig::from_ferrus_toml(
            r#"
[checks]
commands = ["cargo test"]

[repository_graph]
enabled = true
future_backend = "distributed"
"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("unknown field"));
        assert!(error.to_string().contains("future_backend"));
    }
}
