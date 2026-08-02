use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest as ShaDigest, Sha256};
use thiserror::Error;

use crate::repository_graph::domain::{
    Digest, NodeId, RepoPath, RepositoryRef, SemanticKey, SnapshotId, SourceSpan,
};

use super::{MEMORY_EXTRACTOR_CONTRACT_VERSION, MEMORY_MODEL_VERSION};

const MAX_ID_BYTES: usize = 512;
const MAX_TITLE_BYTES: usize = 1_024;
const MAX_QUERY_TEXT_BYTES: usize = 4_096;
const MAX_MEMORY_TEXT_BYTES: usize = 64 * 1_024;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MemoryValueError {
    #[error("{kind} must contain 1..={max_bytes} bytes without control characters")]
    Invalid {
        kind: &'static str,
        max_bytes: usize,
    },
}

fn validate_bounded_text(
    value: String,
    kind: &'static str,
    max_bytes: usize,
    allow_newlines: bool,
) -> Result<String, MemoryValueError> {
    let valid_character = |character: char| {
        !character.is_control() || (allow_newlines && matches!(character, '\n' | '\r' | '\t'))
    };
    if value.trim().is_empty() || value.len() > max_bytes || !value.chars().all(valid_character) {
        return Err(MemoryValueError::Invalid { kind, max_bytes });
    }
    Ok(value)
}

macro_rules! bounded_string {
    ($name:ident, $kind:literal, $max:expr, $allow_newlines:expr) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, MemoryValueError> {
                validate_bounded_text(value.into(), $kind, $max, $allow_newlines).map(Self)
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
                Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    };
}

bounded_string!(ProjectNamespace, "project namespace", MAX_ID_BYTES, false);
bounded_string!(ProjectId, "project id", MAX_ID_BYTES, false);
bounded_string!(MemoryRevisionId, "memory revision id", MAX_ID_BYTES, false);
bounded_string!(MemoryBuildId, "memory build id", MAX_ID_BYTES, false);
bounded_string!(MemoryEntityId, "memory entity id", MAX_ID_BYTES, false);
bounded_string!(
    MemoryRelationshipId,
    "memory relationship id",
    MAX_ID_BYTES,
    false
);
bounded_string!(
    MemoryExtractorId,
    "memory extractor id",
    MAX_ID_BYTES,
    false
);
bounded_string!(MemoryViewName, "memory view name", MAX_ID_BYTES, false);
bounded_string!(MemoryRecordId, "memory record id", MAX_ID_BYTES, false);
bounded_string!(MemoryStatusToken, "memory status", MAX_ID_BYTES, false);
bounded_string!(MemoryPageCursor, "memory page cursor", MAX_ID_BYTES, false);
bounded_string!(
    FederationPageCursor,
    "federation page cursor",
    MAX_ID_BYTES,
    false
);
bounded_string!(MemoryTitle, "memory title", MAX_TITLE_BYTES, false);
bounded_string!(
    MemoryQueryText,
    "memory query text",
    MAX_QUERY_TEXT_BYTES,
    false
);
bounded_string!(MemoryText, "memory text", MAX_MEMORY_TEXT_BYTES, true);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProjectRef {
    pub namespace: ProjectNamespace,
    pub project_id: ProjectId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySourceCategory {
    SpecificationStructure,
    ApprovedOutcome,
    ArchiveManifest,
    RuntimeProvenance,
    TaskBody,
    SubmissionBody,
    ReviewBody,
    PatchBody,
    CheckLogBody,
    QuestionBody,
    AnswerBody,
    ConsultationBody,
    IntegrationErrorBody,
}

impl MemorySourceCategory {
    pub const ALL: [Self; 13] = [
        Self::SpecificationStructure,
        Self::ApprovedOutcome,
        Self::ArchiveManifest,
        Self::RuntimeProvenance,
        Self::TaskBody,
        Self::SubmissionBody,
        Self::ReviewBody,
        Self::PatchBody,
        Self::CheckLogBody,
        Self::QuestionBody,
        Self::AnswerBody,
        Self::ConsultationBody,
        Self::IntegrationErrorBody,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MemorySourceLocator {
    TrackedFile { path: RepoPath },
    ArchiveManifest { archive_id: MemoryRecordId },
    RuntimeRecords { record_type: MemoryStatusToken },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum MemoryEvidenceLocator {
    Span(SourceSpan),
    Record(MemoryRecordId),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryExtractorIdentity {
    pub id: MemoryExtractorId,
    pub version: MemoryStatusToken,
    pub contract_version: u32,
}

impl MemoryExtractorIdentity {
    pub fn current(id: MemoryExtractorId, version: MemoryStatusToken) -> Self {
        Self {
            id,
            version,
            contract_version: MEMORY_EXTRACTOR_CONTRACT_VERSION,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryResolutionState {
    Resolved,
    Unresolved,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryConfidence {
    Exact,
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryIndexTimestamps {
    pub source_observed_at: DateTime<Utc>,
    pub indexed_at: DateTime<Utc>,
}

/// Provenance common to every entity and relationship.
///
/// Locators are project-scoped and portable. Absolute paths and free-form
/// diagnostic messages are deliberately not representable here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryProvenance {
    pub source_category: MemorySourceCategory,
    pub source_locator: MemorySourceLocator,
    pub source_fingerprint: Digest,
    pub extractor: MemoryExtractorIdentity,
    pub evidence: MemoryEvidenceLocator,
    pub resolution: MemoryResolutionState,
    pub confidence: MemoryConfidence,
    pub timestamps: MemoryIndexTimestamps,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MilestoneCompletion {
    Pending,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MemoryEntityData {
    Specification {
        title: MemoryTitle,
    },
    Milestone {
        milestone_id: MemoryRecordId,
        title: MemoryTitle,
        completion: MilestoneCompletion,
    },
    Outcome {
        text: MemoryText,
    },
    Decision {
        text: MemoryText,
    },
    Deviation {
        text: MemoryText,
    },
    ValidationEvidence {
        text: MemoryText,
        check_id: Option<MemoryRecordId>,
    },
    FollowUpWork {
        text: MemoryText,
        milestone_id: Option<MemoryRecordId>,
    },
    TaskReference {
        task_id: MemoryRecordId,
        milestone_id: Option<MemoryRecordId>,
        status: MemoryStatusToken,
    },
    RunReference {
        run_id: MemoryRecordId,
        task_id: MemoryRecordId,
        status: MemoryStatusToken,
        #[serde(default)]
        check_ids: Vec<MemoryRecordId>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryEntityKind {
    Specification,
    Milestone,
    Outcome,
    Decision,
    Deviation,
    ValidationEvidence,
    FollowUpWork,
    TaskReference,
    RunReference,
}

impl MemoryEntityData {
    pub fn kind(&self) -> MemoryEntityKind {
        match self {
            Self::Specification { .. } => MemoryEntityKind::Specification,
            Self::Milestone { .. } => MemoryEntityKind::Milestone,
            Self::Outcome { .. } => MemoryEntityKind::Outcome,
            Self::Decision { .. } => MemoryEntityKind::Decision,
            Self::Deviation { .. } => MemoryEntityKind::Deviation,
            Self::ValidationEvidence { .. } => MemoryEntityKind::ValidationEvidence,
            Self::FollowUpWork { .. } => MemoryEntityKind::FollowUpWork,
            Self::TaskReference { .. } => MemoryEntityKind::TaskReference,
            Self::RunReference { .. } => MemoryEntityKind::RunReference,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEntity {
    pub project: ProjectRef,
    pub memory_revision_id: MemoryRevisionId,
    pub id: MemoryEntityId,
    pub data: MemoryEntityData,
    pub provenance: MemoryProvenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryRelationshipKind {
    Contains,
    Implements,
    Validates,
    Supersedes,
    Concerns,
    Touches,
    FollowsUp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MemoryRelationshipTarget {
    MemoryEntity {
        entity_id: MemoryEntityId,
    },
    RepositoryNode {
        repository: RepositoryRef,
        snapshot_id: SnapshotId,
        node_id: NodeId,
    },
    RepositoryPath {
        repository: RepositoryRef,
        path: RepoPath,
        snapshot_id: Option<SnapshotId>,
    },
    RepositorySymbol {
        repository: RepositoryRef,
        semantic_key: SemanticKey,
        snapshot_id: Option<SnapshotId>,
    },
    Task {
        task_id: MemoryRecordId,
    },
    Run {
        run_id: MemoryRecordId,
    },
    Milestone {
        milestone_id: MemoryRecordId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRelationship {
    pub project: ProjectRef,
    pub memory_revision_id: MemoryRevisionId,
    pub id: MemoryRelationshipId,
    pub kind: MemoryRelationshipKind,
    pub source: MemoryEntityId,
    pub target: MemoryRelationshipTarget,
    pub provenance: MemoryProvenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryBuildState {
    Building,
    Complete,
    Published,
    Failed,
    Superseded,
}

/// Immutable identity inputs for one semantic project-memory revision.
///
/// `source_set_digest` covers only authorized source fingerprints. Wall-clock
/// timestamps and operational storage settings are excluded from the revision
/// identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRevision {
    pub id: MemoryRevisionId,
    pub project: ProjectRef,
    pub source_set_digest: Digest,
    pub policy_digest: Digest,
    pub memory_model_version: u32,
    pub extractor_set_digest: Digest,
    pub completed_by: MemoryBuildId,
}

impl MemoryRevision {
    pub fn from_manifest(
        manifest: &AuthorizedSourceManifest,
        completed_by: MemoryBuildId,
    ) -> Result<Self, AuthorizedSourceManifestError> {
        Ok(Self {
            id: manifest.revision_id()?,
            project: manifest.project.clone(),
            source_set_digest: manifest.source_set_digest.clone(),
            policy_digest: manifest.policy_digest.clone(),
            memory_model_version: MEMORY_MODEL_VERSION,
            extractor_set_digest: manifest.extractor_set_digest.clone(),
            completed_by,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBuild {
    pub id: MemoryBuildId,
    pub project: ProjectRef,
    pub prospective_revision_id: MemoryRevisionId,
    pub state: MemoryBuildState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryPublicationVersion {
    pub revision_id: MemoryRevisionId,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryPublishRequest {
    pub project: ProjectRef,
    pub view_name: MemoryViewName,
    pub build_id: MemoryBuildId,
    pub expected: Option<MemoryPublicationVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedMemoryRevision {
    pub project: ProjectRef,
    pub view_name: MemoryViewName,
    pub revision_id: MemoryRevisionId,
    pub build_id: MemoryBuildId,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MemoryPublicationOutcome {
    Published { view: PublishedMemoryRevision },
    Superseded { current: PublishedMemoryRevision },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizedSourceDescriptor {
    pub project: ProjectRef,
    pub category: MemorySourceCategory,
    pub locator: MemorySourceLocator,
    pub fingerprint: Digest,
    pub byte_len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizedSourceManifest {
    pub project: ProjectRef,
    pub policy_digest: Digest,
    pub source_set_digest: Digest,
    pub extractor_set_digest: Digest,
    pub sources: Vec<AuthorizedSourceDescriptor>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AuthorizedSourceManifestError {
    #[error("authorized source belongs to another project")]
    ProjectMismatch,
    #[error("authorized source-set digest does not match the manifest")]
    SourceSetDigestMismatch,
}

impl AuthorizedSourceManifest {
    /// Recomputes the source-set digest independently from discovery order.
    pub fn computed_source_set_digest(&self) -> Result<Digest, AuthorizedSourceManifestError> {
        #[derive(Serialize)]
        struct CanonicalSource<'a> {
            category: MemorySourceCategory,
            locator: &'a MemorySourceLocator,
            fingerprint: &'a Digest,
        }

        if self
            .sources
            .iter()
            .any(|source| source.project != self.project)
        {
            return Err(AuthorizedSourceManifestError::ProjectMismatch);
        }
        let mut sources = self
            .sources
            .iter()
            .map(|source| CanonicalSource {
                category: source.category,
                locator: &source.locator,
                fingerprint: &source.fingerprint,
            })
            .collect::<Vec<_>>();
        sources.sort_by(|left, right| {
            serde_json::to_vec(left)
                .expect("authorized source descriptors are serializable")
                .cmp(
                    &serde_json::to_vec(right)
                        .expect("authorized source descriptors are serializable"),
                )
        });
        Ok(canonical_digest(&(self.project.clone(), sources)))
    }

    pub fn validate(&self) -> Result<(), AuthorizedSourceManifestError> {
        if self.computed_source_set_digest()? != self.source_set_digest {
            return Err(AuthorizedSourceManifestError::SourceSetDigestMismatch);
        }
        Ok(())
    }

    /// Derives the semantic memory revision identity from all contract inputs.
    pub fn revision_id(&self) -> Result<MemoryRevisionId, AuthorizedSourceManifestError> {
        self.validate()?;
        let digest = canonical_digest(&(
            &self.project,
            &self.source_set_digest,
            &self.policy_digest,
            MEMORY_MODEL_VERSION,
            &self.extractor_set_digest,
        ));
        Ok(MemoryRevisionId::new(format!("memory:{}", digest.value()))
            .expect("sha256 memory revision identity is bounded and non-empty"))
    }
}

fn canonical_digest(value: &impl Serialize) -> Digest {
    let encoded = serde_json::to_vec(value).expect("memory identity inputs are serializable");
    let value = Sha256::digest(encoded)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Digest::new("sha256", value).expect("sha256 output is lowercase hexadecimal")
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryFragment {
    pub entities: Vec<MemoryEntity>,
    pub relationships: Vec<MemoryRelationship>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryFragmentCacheKey {
    pub project: ProjectRef,
    pub category: MemorySourceCategory,
    pub locator: MemorySourceLocator,
    pub source_fingerprint: Digest,
    pub policy_digest: Digest,
    pub extractor: MemoryExtractorIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedMemoryFragment {
    pub key: MemoryFragmentCacheKey,
    pub fragment: MemoryFragment,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryCommit {
    pub revision: MemoryRevision,
    pub entities: Vec<MemoryEntity>,
    pub relationships: Vec<MemoryRelationship>,
    pub cache_writes: Vec<CachedMemoryFragment>,
    pub metrics: MemoryBuildMetrics,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBuildMetrics {
    pub discovered_sources: u64,
    pub reused_sources: u64,
    pub extracted_sources: u64,
    pub skipped_sources: u64,
    pub failed_sources: u64,
    pub processed_bytes: u64,
    pub entities: u64,
    pub relationships: u64,
    pub stale_links: u64,
    pub diagnostics: u64,
    pub duration_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_memory_values_reject_empty_control_and_oversized_content() {
        assert!(MemoryText::new("approved outcome").is_ok());
        assert!(MemoryText::new("line one\nline two").is_ok());
        assert!(MemoryText::new("").is_err());
        assert!(MemoryText::new("secret\0suffix").is_err());
        assert!(MemoryText::new("x".repeat(MAX_MEMORY_TEXT_BYTES + 1)).is_err());
        assert!(MemoryEntityId::new("entity\nnewline").is_err());
    }

    #[test]
    fn source_locators_cannot_represent_absolute_local_paths() {
        let json = serde_json::json!({
            "type": "tracked_file",
            "path": "/Users/example/private/spec.md"
        });
        assert!(serde_json::from_value::<MemorySourceLocator>(json).is_err());
    }

    #[test]
    fn revision_identity_inputs_exclude_timestamps_and_storage_details() {
        let project = ProjectRef {
            namespace: ProjectNamespace::new("local:test").unwrap(),
            project_id: ProjectId::new("project-1").unwrap(),
        };
        let mut manifest = AuthorizedSourceManifest {
            project,
            policy_digest: Digest::new("sha256", "11").unwrap(),
            source_set_digest: Digest::new("sha256", "00").unwrap(),
            extractor_set_digest: Digest::new("sha256", "22").unwrap(),
            sources: Vec::new(),
        };
        manifest.source_set_digest = manifest.computed_source_set_digest().unwrap();
        let revision =
            MemoryRevision::from_manifest(&manifest, MemoryBuildId::new("build-1").unwrap())
                .unwrap();
        let json = serde_json::to_value(revision).unwrap();
        assert_eq!(json["memory_model_version"], MEMORY_MODEL_VERSION);
        for forbidden in ["created_at", "indexed_at", "database", "endpoint", "path"] {
            assert!(json.get(forbidden).is_none());
        }
    }

    #[test]
    fn source_and_revision_identities_are_deterministic_across_discovery_order() {
        let project = ProjectRef {
            namespace: ProjectNamespace::new("local:test").unwrap(),
            project_id: ProjectId::new("project-1").unwrap(),
        };
        let source = |record: &str, fingerprint: &str| AuthorizedSourceDescriptor {
            project: project.clone(),
            category: MemorySourceCategory::RuntimeProvenance,
            locator: MemorySourceLocator::RuntimeRecords {
                record_type: MemoryStatusToken::new(record).unwrap(),
            },
            fingerprint: Digest::new("sha256", fingerprint).unwrap(),
            byte_len: 10,
        };
        let mut first = AuthorizedSourceManifest {
            project: project.clone(),
            policy_digest: Digest::new("sha256", "aa").unwrap(),
            source_set_digest: Digest::new("sha256", "00").unwrap(),
            extractor_set_digest: Digest::new("sha256", "bb").unwrap(),
            sources: vec![source("task", "11"), source("run", "22")],
        };
        first.source_set_digest = first.computed_source_set_digest().unwrap();
        let mut second = first.clone();
        second.sources.reverse();
        assert_eq!(
            first.computed_source_set_digest().unwrap(),
            second.computed_source_set_digest().unwrap()
        );
        assert_eq!(first.revision_id().unwrap(), second.revision_id().unwrap());
    }
}
