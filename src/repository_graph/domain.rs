use std::{
    collections::BTreeMap,
    fmt,
    num::{NonZeroU32, NonZeroU64},
};

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum IdentityError {
    #[error("{kind} must not be empty")]
    Empty { kind: &'static str },
}

macro_rules! opaque_id {
    ($name:ident, $kind:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
                let value = value.into();
                if value.is_empty() {
                    return Err(IdentityError::Empty { kind: $kind });
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

opaque_id!(RepositoryNamespace, "repository namespace");
opaque_id!(RepositoryId, "repository id");
opaque_id!(SourceRevisionId, "source revision id");
opaque_id!(SnapshotId, "snapshot id");
opaque_id!(BuildId, "build id");
opaque_id!(NodeId, "node id");
opaque_id!(EdgeId, "edge id");
opaque_id!(SemanticKey, "semantic key");
opaque_id!(TaskViewId, "task view id");
opaque_id!(OverlayRevisionId, "overlay revision id");
opaque_id!(ExtractorId, "extractor id");
opaque_id!(PublishedViewName, "published view name");
opaque_id!(PageCursor, "page cursor");

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RepositoryRef {
    pub namespace: RepositoryNamespace,
    pub repository_id: RepositoryId,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RepoPathError {
    #[error("repository path must not be empty")]
    Empty,
    #[error("repository path must be valid UTF-8")]
    NonUtf8,
    #[error("repository path must be relative and must not contain a platform prefix")]
    AbsoluteOrPrefixed,
    #[error("repository path contains a forbidden component: {0:?}")]
    ForbiddenComponent(String),
    #[error("repository path must not contain NUL")]
    Nul,
}

/// A portable, case-preserving path relative to a repository root.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct RepoPath(String);

impl RepoPath {
    pub fn new(value: impl AsRef<str>) -> Result<Self, RepoPathError> {
        let value = value.as_ref();
        if value.is_empty() {
            return Err(RepoPathError::Empty);
        }
        if value.contains('\0') {
            return Err(RepoPathError::Nul);
        }
        let normalized = value.replace('\\', "/");
        if normalized.starts_with('/')
            || normalized.starts_with("//")
            || normalized
                .as_bytes()
                .get(1)
                .is_some_and(|separator| *separator == b':')
        {
            return Err(RepoPathError::AbsoluteOrPrefixed);
        }
        for component in normalized.split('/') {
            if component.is_empty() || component == "." || component == ".." {
                return Err(RepoPathError::ForbiddenComponent(component.to_string()));
            }
        }
        Ok(Self(normalized))
    }

    pub fn from_path(path: &std::path::Path) -> Result<Self, RepoPathError> {
        let value = path.to_str().ok_or(RepoPathError::NonUtf8)?;
        Self::new(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RepoPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Algorithm-tagged digest. The value is lowercase hexadecimal.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DigestError {
    #[error("digest algorithm must be a non-empty lowercase ASCII token")]
    InvalidAlgorithm,
    #[error("digest value must be non-empty lowercase hexadecimal")]
    InvalidValue,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct Digest {
    algorithm: String,
    value: String,
}

impl Digest {
    pub fn new(
        algorithm: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<Self, DigestError> {
        let algorithm = algorithm.into();
        if algorithm.is_empty()
            || !algorithm.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
            })
        {
            return Err(DigestError::InvalidAlgorithm);
        }

        let value = value.into();
        if value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(DigestError::InvalidValue);
        }

        Ok(Self { algorithm, value })
    }

    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }

    pub fn value(&self) -> &str {
        &self.value
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireDigest {
            algorithm: String,
            value: String,
        }

        let digest = WireDigest::deserialize(deserializer)?;
        Self::new(digest.algorithm, digest.value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    CommittedTree,
    WorkspaceOverlay,
    TaskBaseline,
    NonGitManifest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRevision {
    pub id: SourceRevisionId,
    pub repository: RepositoryRef,
    pub source_kind: SourceKind,
    pub base_revision: Option<Digest>,
    pub manifest_digest: Digest,
    pub analysis_config_digest: Digest,
    pub dirty: bool,
    pub includes_untracked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildState {
    Building,
    Complete,
    Published,
    Failed,
    Superseded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    NotBuilt,
    Available,
    Incompatible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    Fresh,
    Stale,
    Unknown,
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphSnapshot {
    pub id: SnapshotId,
    pub repository: RepositoryRef,
    pub source_revision_id: SourceRevisionId,
    pub source_manifest_digest: Digest,
    pub graph_model_version: u32,
    pub analysis_config_digest: Digest,
    pub extractor_set_digest: Digest,
    pub completed_by: BuildId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphBuild {
    pub id: BuildId,
    pub repository: RepositoryRef,
    pub source_revision_id: SourceRevisionId,
    pub prospective_snapshot_id: SnapshotId,
    pub state: BuildState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionState {
    Resolved,
    Unresolved,
    External,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Exact,
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePosition {
    /// Zero-based byte offset in the exact content identified by the evidence digest.
    pub byte_offset: u64,
    /// One-based human-readable source line when the extractor can provide it.
    pub line: Option<u32>,
    /// One-based UTF-8 byte column when the extractor can provide it.
    pub column: Option<u32>,
}

/// A half-open source range: `start` is inclusive and `end` is exclusive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSpan {
    pub start: SourcePosition,
    pub end: SourcePosition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEvidence {
    pub path: RepoPath,
    pub content_identity: Digest,
    pub span: Option<SourceSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractorIdentity {
    pub id: ExtractorId,
    pub version: String,
    pub contract_version: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum GraphValue {
    Boolean(bool),
    Integer(i64),
    Float(f64),
    String(String),
    StringList(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FactProvenance {
    pub extractor: ExtractorIdentity,
    pub evidence: Option<SourceEvidence>,
    pub resolution: ResolutionState,
    pub confidence: Confidence,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphNode {
    pub snapshot_id: SnapshotId,
    pub id: NodeId,
    pub kind: String,
    pub semantic_key: Option<SemanticKey>,
    pub provenance: FactProvenance,
    #[serde(default)]
    pub properties: BTreeMap<String, GraphValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphEdge {
    pub snapshot_id: SnapshotId,
    pub id: EdgeId,
    pub kind: String,
    pub source: NodeId,
    pub target: EdgeTarget,
    pub provenance: FactProvenance,
    #[serde(default)]
    pub properties: BTreeMap<String, GraphValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum EdgeTarget {
    Node(NodeId),
    External(String),
    Unresolved(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DiagnosticCodeError {
    #[error("diagnostic code must be 1..=64 lowercase ASCII token characters")]
    Invalid,
}

/// A bounded machine-readable code. Free-form source text and secret values
/// are deliberately not representable as diagnostic messages.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct DiagnosticCode(String);

impl DiagnosticCode {
    pub fn new(value: impl Into<String>) -> Result<Self, DiagnosticCodeError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 64
            || !value.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
            })
        {
            return Err(DiagnosticCodeError::Invalid);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for DiagnosticCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticLocation {
    pub path: RepoPath,
    pub span: Option<SourceSpan>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphDiagnostic {
    pub build_id: BuildId,
    pub snapshot_id: Option<SnapshotId>,
    pub severity: DiagnosticSeverity,
    pub code: DiagnosticCode,
    pub location: Option<DiagnosticLocation>,
    #[serde(default)]
    pub metrics: BTreeMap<DiagnosticCode, i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskViewLifecycle {
    Mutable,
    FrozenSubmitted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRepositoryView {
    pub id: TaskViewId,
    pub task_id: String,
    pub run_id: Option<String>,
    pub baseline_snapshot_id: SnapshotId,
    pub overlay_revision_id: Option<OverlayRevisionId>,
    pub overlay_manifest_digest: Option<Digest>,
    pub lifecycle: TaskViewLifecycle,
    pub baseline_freshness: Freshness,
    pub overlay_freshness: Freshness,
}

/// Mandatory budgets shared by every graph query request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryBudget {
    pub max_results: NonZeroU32,
    pub max_bytes: NonZeroU64,
    pub max_depth: NonZeroU32,
    pub max_duration_ms: NonZeroU64,
    pub max_diagnostics: NonZeroU32,
}

impl QueryBudget {
    pub fn new(
        max_results: NonZeroU32,
        max_bytes: NonZeroU64,
        max_depth: NonZeroU32,
        max_duration_ms: NonZeroU64,
        max_diagnostics: NonZeroU32,
    ) -> Self {
        Self {
            max_results,
            max_bytes,
            max_depth,
            max_duration_ms,
            max_diagnostics,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_paths_are_portable_and_confined() {
        assert_eq!(
            RepoPath::new(r"src\main.rs").unwrap().as_str(),
            "src/main.rs"
        );
        for invalid in [
            "",
            "/etc/passwd",
            "C:/secret",
            "src//main.rs",
            "./src",
            "src/../x",
        ] {
            assert!(
                RepoPath::new(invalid).is_err(),
                "{invalid} must be rejected"
            );
        }
    }

    #[test]
    fn repository_path_deserialization_enforces_invariants() {
        let error = serde_json::from_str::<RepoPath>(r#""../secret""#).unwrap_err();
        assert!(error.to_string().contains("forbidden component"));
    }

    #[test]
    fn digests_accept_only_canonical_algorithm_tags_and_values() {
        let digest = Digest::new("sha256", "00abcdef").unwrap();
        assert_eq!(digest.algorithm(), "sha256");
        assert_eq!(digest.value(), "00abcdef");

        for algorithm in ["", "SHA256", "sha 256", "sha/256"] {
            assert!(Digest::new(algorithm, "00").is_err());
        }
        for value in ["", "AB", "0g", "00-ff"] {
            assert!(Digest::new("sha256", value).is_err());
        }
    }

    #[test]
    fn digest_deserialization_enforces_canonical_values() {
        for value in ["", "AB", "0g", "00-ff"] {
            let json = serde_json::json!({"algorithm": "sha256", "value": value});
            assert!(serde_json::from_value::<Digest>(json).is_err());
        }
    }

    #[test]
    fn query_budgets_cannot_deserialize_zero_limits() {
        let json = r#"{
            "max_results": 10,
            "max_bytes": 4096,
            "max_depth": 0,
            "max_duration_ms": 100
        }"#;
        assert!(serde_json::from_str::<QueryBudget>(json).is_err());
    }

    #[test]
    fn diagnostic_codes_reject_free_form_text() {
        assert!(DiagnosticCode::new("extractor_failed").is_ok());
        for unsafe_value in [
            "Contains source text",
            "token=secret",
            "/absolute/path",
            "line\nbody",
        ] {
            assert!(DiagnosticCode::new(unsafe_value).is_err());
        }
    }

    #[test]
    fn diagnostics_reject_free_form_payload_fields() {
        let json = serde_json::json!({
            "build_id": "build-1",
            "snapshot_id": null,
            "severity": "error",
            "code": "extractor_failed",
            "location": null,
            "metrics": {},
            "message": "arbitrary source body"
        });
        assert!(serde_json::from_value::<GraphDiagnostic>(json).is_err());
    }
}
