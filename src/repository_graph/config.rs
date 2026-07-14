use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use super::domain::Digest;

const CONFIG_SCHEMA_VERSION: u32 = 2;
const SOURCE_POLICY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RepositoryGraphConfigError {
    #[error("repository graph pattern must not be empty")]
    EmptyPattern,
    #[error("repository graph pattern must be repository-relative: {0}")]
    NonRelativePattern(String),
    #[error("repository graph sensitive pattern must not be negated: {0}")]
    NegatedSensitivePattern(String),
    #[error("repository graph include pattern must not be negated: {0}")]
    NegatedIncludePattern(String),
    #[error("repository graph index limit must be greater than zero: {0}")]
    ZeroIndexLimit(&'static str),
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
    /// Paths that may contain credentials or other secrets and are always excluded.
    pub sensitive: BTreeSet<String>,
    pub include_untracked: bool,
    pub include_generated: bool,
    pub include_vendor: bool,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            include: BTreeSet::from(["**/*".to_string()]),
            rules: vec![".git/**".to_string(), ".ferrus/**".to_string()],
            sensitive: BTreeSet::from([
                "**/.env".to_string(),
                "**/.env.*".to_string(),
                "**/*.key".to_string(),
                "**/*.p12".to_string(),
                "**/*.pem".to_string(),
                "**/*.pfx".to_string(),
                "**/id_ed25519".to_string(),
                "**/id_rsa".to_string(),
            ]),
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
    pub max_directories: u64,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_facts_per_file: u64,
    pub max_parser_duration_ms: u64,
    pub max_diagnostics: u64,
}

impl Default for IndexLimitsConfig {
    fn default() -> Self {
        Self {
            max_files: 100_000,
            max_directories: 100_000,
            max_file_bytes: 2 * 1024 * 1024,
            max_total_bytes: 512 * 1024 * 1024,
            max_facts_per_file: 100_000,
            max_parser_duration_ms: 2_000,
            max_diagnostics: 1_000,
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
    sensitive: BTreeSet<String>,
    include_untracked: bool,
    include_generated: bool,
    include_vendor: bool,
}

#[derive(Serialize)]
struct EffectiveSourcePolicy {
    schema_version: u32,
    source: EffectiveSourceConfig,
    max_files: u64,
    max_directories: u64,
    max_file_bytes: u64,
    max_total_bytes: u64,
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
        Ok(Digest::new("sha256", hex_lower(&value))
            .expect("sha256 and hex_lower always produce a canonical digest"))
    }

    /// Returns the canonical discovery policy included in source-manifest identity.
    /// Analyzer and parser settings remain separate snapshot inputs.
    pub fn source_policy_digest(&self) -> Result<Digest, RepositoryGraphConfigError> {
        self.validate_index_limits()?;
        let effective = EffectiveSourcePolicy {
            schema_version: SOURCE_POLICY_SCHEMA_VERSION,
            source: self.effective_source_config()?,
            max_files: self.index_limits.max_files,
            max_directories: self.index_limits.max_directories,
            max_file_bytes: self.index_limits.max_file_bytes,
            max_total_bytes: self.index_limits.max_total_bytes,
        };
        let bytes = serde_json::to_vec(&effective)
            .map_err(|error| RepositoryGraphConfigError::Serialization(error.to_string()))?;
        let value = Sha256::digest(bytes);
        Ok(Digest::new("sha256", hex_lower(&value))
            .expect("sha256 and hex_lower always produce a canonical digest"))
    }

    fn effective_semantic_config(
        &self,
    ) -> Result<EffectiveSemanticConfig, RepositoryGraphConfigError> {
        self.validate_index_limits()?;
        Ok(EffectiveSemanticConfig {
            schema_version: CONFIG_SCHEMA_VERSION,
            source: self.effective_source_config()?,
            analyzers: self.analyzers.clone(),
            index_limits: self.index_limits.clone(),
        })
    }

    fn effective_source_config(&self) -> Result<EffectiveSourceConfig, RepositoryGraphConfigError> {
        let include = self
            .source
            .include
            .iter()
            .map(|pattern| {
                let normalized = normalize_pattern(pattern)?;
                if normalized.starts_with('!') {
                    return Err(RepositoryGraphConfigError::NegatedIncludePattern(
                        pattern.clone(),
                    ));
                }
                Ok(normalized)
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        let rules = self
            .source
            .rules
            .iter()
            .map(|pattern| normalize_pattern(pattern))
            .collect::<Result<Vec<_>, _>>()?;
        let sensitive = self
            .source
            .sensitive
            .iter()
            .map(|pattern| {
                let normalized = normalize_pattern(pattern)?;
                if normalized.starts_with('!') {
                    return Err(RepositoryGraphConfigError::NegatedSensitivePattern(
                        pattern.clone(),
                    ));
                }
                Ok(normalized.to_ascii_lowercase())
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        Ok(EffectiveSourceConfig {
            include,
            rules,
            sensitive,
            include_untracked: self.source.include_untracked,
            include_generated: self.source.include_generated,
            include_vendor: self.source.include_vendor,
        })
    }

    fn validate_index_limits(&self) -> Result<(), RepositoryGraphConfigError> {
        for (name, value) in [
            ("max_files", self.index_limits.max_files),
            ("max_directories", self.index_limits.max_directories),
            ("max_file_bytes", self.index_limits.max_file_bytes),
            ("max_total_bytes", self.index_limits.max_total_bytes),
            ("max_facts_per_file", self.index_limits.max_facts_per_file),
            (
                "max_parser_duration_ms",
                self.index_limits.max_parser_duration_ms,
            ),
            ("max_diagnostics", self.index_limits.max_diagnostics),
        ] {
            if value == 0 {
                return Err(RepositoryGraphConfigError::ZeroIndexLimit(name));
            }
        }
        Ok(())
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
    let body = canonical_pattern_body(body)?;
    Ok(if negated { format!("!{body}") } else { body })
}

pub(super) fn canonical_pattern_body(pattern: &str) -> Result<String, RepositoryGraphConfigError> {
    let body = pattern.trim().replace('\\', "/");
    let body = body.strip_prefix("./").unwrap_or(&body);
    let body = body.trim_end_matches('/');
    if body.is_empty()
        || body.starts_with('/')
        || body
            .as_bytes()
            .get(1)
            .is_some_and(|separator| *separator == b':')
        || body
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(RepositoryGraphConfigError::NonRelativePattern(
            pattern.to_string(),
        ));
    }
    Ok(body.to_string())
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
rules = [".git/**", ".ferrus/**"]
sensitive = ["**/.env", "**/.env.*", "**/*.key", "**/*.p12", "**/*.pem", "**/*.pfx", "**/id_ed25519", "**/id_rsa"]
include_untracked = true
include_generated = false
include_vendor = false

[analyzers]
enabled = []
settings = {}

[index_limits]
max_files = 100000
max_directories = 100000
max_file_bytes = 2097152
max_total_bytes = 536870912
max_facts_per_file = 100000
max_parser_duration_ms = 2000
max_diagnostics = 1000

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
    fn equivalent_pattern_spelling_has_one_canonical_digest() {
        let mut left = RepositoryGraphConfig::default();
        left.source.include = BTreeSet::from(["./src/**/".to_string()]);
        left.source.rules = vec!["! ./src/keep.rs/".to_string()];
        let mut right = RepositoryGraphConfig::default();
        right.source.include = BTreeSet::from(["src/**".to_string()]);
        right.source.rules = vec!["!src/keep.rs".to_string()];

        assert_eq!(
            left.source_policy_digest().unwrap(),
            right.source_policy_digest().unwrap()
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
    fn source_policy_digest_excludes_analyzer_only_changes() {
        let baseline = RepositoryGraphConfig::default();
        let mut analyzer_changed = baseline.clone();
        analyzer_changed
            .analyzers
            .enabled
            .insert("rust-syntax".to_string());
        assert_eq!(
            baseline.source_policy_digest().unwrap(),
            analyzer_changed.source_policy_digest().unwrap()
        );
        assert_ne!(
            baseline.analysis_config_digest().unwrap(),
            analyzer_changed.analysis_config_digest().unwrap()
        );
    }

    #[test]
    fn sensitive_patterns_cannot_be_negated() {
        let mut config = RepositoryGraphConfig::default();
        config.source.sensitive.insert("!**/.env".to_string());
        assert!(matches!(
            config.analysis_config_digest(),
            Err(RepositoryGraphConfigError::NegatedSensitivePattern(_))
        ));
    }

    #[test]
    fn include_patterns_cannot_be_negated() {
        let mut config = RepositoryGraphConfig::default();
        config.source.include.insert("!target/**".to_string());
        assert!(matches!(
            config.analysis_config_digest(),
            Err(RepositoryGraphConfigError::NegatedIncludePattern(_))
        ));
    }

    #[test]
    fn structural_index_limits_must_be_nonzero() {
        let mut config = RepositoryGraphConfig::default();
        config.index_limits.max_total_bytes = 0;
        assert_eq!(
            config.analysis_config_digest(),
            Err(RepositoryGraphConfigError::ZeroIndexLimit(
                "max_total_bytes"
            ))
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
