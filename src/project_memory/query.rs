//! Versioned and bounded project-memory query contracts.

use std::num::{NonZeroU32, NonZeroU64};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::repository_graph::domain::{Digest, RepoPath, SemanticKey};

use super::{
    MEMORY_QUERY_WIRE_VERSION,
    diagnostics::{MemoryDiagnostic, MemoryDiagnosticCode},
    domain::{
        MemoryBuildId, MemoryBuildState, MemoryEntity, MemoryEntityId, MemoryEntityKind,
        MemoryEvidenceLocator, MemoryPageCursor, MemoryQueryText, MemoryRecordId,
        MemoryRelationship, MemoryRelationshipKind, MemoryRevisionId, MemorySourceCategory,
        MemorySourceLocator, MemoryViewName, ProjectRef,
    },
    policy::MemorySourcePolicy,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryQueryBudget {
    pub max_results: NonZeroU32,
    pub max_bytes: NonZeroU64,
    pub max_snippet_bytes: NonZeroU64,
    pub max_depth: NonZeroU32,
    pub max_duration_ms: NonZeroU64,
    pub max_diagnostics: NonZeroU32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum MemoryRevisionSelector {
    Published(MemoryViewName),
    Revision(MemoryRevisionId),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryQueryScope {
    pub wire_version: u32,
    pub project: ProjectRef,
    pub revision: MemoryRevisionSelector,
    pub budget: MemoryQueryBudget,
    pub freshness_comparison: Option<MemoryFreshnessComparison>,
}

impl MemoryQueryScope {
    pub fn current(
        project: ProjectRef,
        revision: MemoryRevisionSelector,
        budget: MemoryQueryBudget,
    ) -> Self {
        Self {
            wire_version: MEMORY_QUERY_WIRE_VERSION,
            project,
            revision,
            budget,
            freshness_comparison: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryFreshnessComparison {
    pub source_set_digest: Digest,
    pub policy_digest: Digest,
    pub extractor_set_digest: Digest,
}

impl MemoryFreshnessComparison {
    pub fn from_manifest(manifest: &super::domain::AuthorizedSourceManifest) -> Self {
        Self {
            source_set_digest: manifest.source_set_digest.clone(),
            policy_digest: manifest.policy_digest.clone(),
            extractor_set_digest: manifest.extractor_set_digest.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryPageRequest {
    pub cursor: Option<MemoryPageCursor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryTruncationReason {
    Results,
    Bytes,
    Depth,
    Duration,
    Capability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryTruncation {
    pub reason: MemoryTruncationReason,
    pub returned_results: u32,
    pub returned_bytes: u64,
    pub explored_depth: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryPageInfo {
    pub next_cursor: Option<MemoryPageCursor>,
    pub truncation: Option<MemoryTruncation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryFreshness {
    Fresh,
    Stale,
    Unknown,
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryFreshnessEnvelope {
    pub freshness: MemoryFreshness,
    pub compared_source_set_digest: Option<Digest>,
    #[serde(default)]
    pub reason_codes: Vec<MemoryDiagnosticCode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryAvailability {
    NotBuilt,
    Available,
    Incompatible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryRetrievalAction {
    Build,
    WaitForBuild,
    RetryBuild,
    Refresh,
    Rebuild,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySourcePolicyStatus {
    pub category: MemorySourceCategory,
    pub policy: MemorySourcePolicy,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryStatistics {
    pub sources: u64,
    pub entities: u64,
    pub relationships: u64,
    pub stale_links: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryStatusRequest {
    pub scope: MemoryQueryScope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryStatusData {
    pub availability: MemoryAvailability,
    pub build_state: Option<MemoryBuildState>,
    pub build_id: Option<MemoryBuildId>,
    pub memory_model_version: Option<u32>,
    pub statistics: Option<MemoryStatistics>,
    pub recommended_action: Option<MemoryRetrievalAction>,
    pub source_policy: Vec<MemorySourcePolicyStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryStatusResponse {
    pub wire_version: u32,
    pub project: ProjectRef,
    pub revision_id: Option<MemoryRevisionId>,
    pub freshness: MemoryFreshnessEnvelope,
    pub diagnostics: Vec<MemoryDiagnostic>,
    pub data: MemoryStatusData,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemorySearchRequest {
    pub scope: MemoryQueryScope,
    pub text: MemoryQueryText,
    #[serde(default)]
    pub entity_kinds: Vec<MemoryEntityKind>,
    #[serde(default)]
    pub source_categories: Vec<MemorySourceCategory>,
    pub page: MemoryPageRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySearchMatchKind {
    ExactId,
    ExactTitle,
    TitlePrefix,
    TitleContains,
    CuratedTextContains,
    ProvenanceReference,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemorySearchHit {
    pub entity: MemoryEntity,
    pub match_kind: MemorySearchMatchKind,
    pub score: f64,
    #[serde(default)]
    pub selection_reasons: Vec<MemoryDiagnosticCode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemorySearchResponse {
    pub wire_version: u32,
    pub project: ProjectRef,
    pub revision_id: MemoryRevisionId,
    pub freshness: MemoryFreshnessEnvelope,
    pub diagnostics: Vec<MemoryDiagnostic>,
    pub page: MemoryPageInfo,
    pub hits: Vec<MemorySearchHit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum MemoryContextSeed {
    Entity(MemoryEntityId),
    Milestone(MemoryRecordId),
    Task(MemoryRecordId),
    Run(MemoryRecordId),
    RepositoryPath(RepoPath),
    RepositorySymbol(SemanticKey),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryContextPolicy {
    #[serde(default)]
    pub relationship_kinds: Vec<MemoryRelationshipKind>,
    #[serde(default)]
    pub include_unresolved: bool,
    #[serde(default)]
    pub include_stale: bool,
    #[serde(default)]
    pub include_snippets: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryContextRequest {
    pub scope: MemoryQueryScope,
    pub seeds: Vec<MemoryContextSeed>,
    pub policy: MemoryContextPolicy,
    pub page: MemoryPageRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySnippet {
    pub source_locator: MemorySourceLocator,
    pub evidence: Option<MemoryEvidenceLocator>,
    pub verified_fingerprint: Digest,
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryContextItem {
    pub entity: MemoryEntity,
    pub snippet: Option<MemorySnippet>,
    #[serde(default)]
    pub selection_reasons: Vec<MemoryDiagnosticCode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryContextResponse {
    pub wire_version: u32,
    pub project: ProjectRef,
    pub revision_id: MemoryRevisionId,
    pub freshness: MemoryFreshnessEnvelope,
    pub diagnostics: Vec<MemoryDiagnostic>,
    pub page: MemoryPageInfo,
    pub items: Vec<MemoryContextItem>,
    pub relationships: Vec<MemoryRelationship>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryContentRequest {
    pub project: ProjectRef,
    pub revision_id: MemoryRevisionId,
    pub source_category: MemorySourceCategory,
    pub locator: MemorySourceLocator,
    pub expected_fingerprint: Digest,
    pub evidence: Option<MemoryEvidenceLocator>,
    pub max_bytes: NonZeroU64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryContentResponse {
    pub verified_fingerprint: Digest,
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MemoryQueryError {
    #[error("project memory is unavailable")]
    Unavailable,
    #[error("project memory revision was not found")]
    RevisionNotFound,
    #[error("project memory cursor is stale or belongs to another request")]
    StaleCursor,
    #[error("project memory request exceeded the {0:?} budget")]
    BudgetExceeded(MemoryTruncationReason),
    #[error("project memory source content changed")]
    ContentChanged,
    #[error("project memory source category is not authorized")]
    SourceNotAuthorized,
    #[error("project memory backend failed with {0}")]
    Backend(MemoryDiagnosticCode),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_budgets_reject_zero_values_at_the_wire_boundary() {
        let value = serde_json::json!({
            "max_results": 0,
            "max_bytes": 1,
            "max_snippet_bytes": 1,
            "max_depth": 1,
            "max_duration_ms": 1,
            "max_diagnostics": 1
        });
        assert!(serde_json::from_value::<MemoryQueryBudget>(value).is_err());
    }

    #[test]
    fn snippets_require_a_verified_source_fingerprint() {
        let value = serde_json::json!({
            "source_locator": {"type": "runtime_records", "record_type": "task"},
            "evidence": null,
            "text": "content",
            "truncated": false
        });
        assert!(serde_json::from_value::<MemorySnippet>(value).is_err());
    }
}
