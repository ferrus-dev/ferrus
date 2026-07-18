//! Versioned, bounded request and response contracts.
//!
//! These DTOs are portable across the local SQLite and future remote
//! implementations and never carry absolute workspace paths or backend-specific
//! values.

use std::{collections::BTreeMap, num::NonZeroU64};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    QUERY_WIRE_VERSION,
    domain::{
        Availability, BuildId, BuildState, DiagnosticCode, DiagnosticLocation, DiagnosticSeverity,
        Digest, EdgeId, EdgeTarget, FactProvenance, Freshness, GraphNode, NodeId, PageCursor,
        PublishedViewName, QueryBudget, RepoPath, RepositoryRef, SemanticKey, SnapshotId,
        SourceRevisionId, SourceSpan,
    },
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum SnapshotSelector {
    Published(PublishedViewName),
    Snapshot(SnapshotId),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryScope {
    pub wire_version: u32,
    pub repository: RepositoryRef,
    pub snapshot: SnapshotSelector,
    pub budget: QueryBudget,
}

impl QueryScope {
    pub fn current(
        repository: RepositoryRef,
        snapshot: SnapshotSelector,
        budget: QueryBudget,
    ) -> Self {
        Self {
            wire_version: QUERY_WIRE_VERSION,
            repository,
            snapshot,
            budget,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageRequest {
    pub cursor: Option<PageCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusRequest {
    pub scope: QueryScope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchRequest {
    pub scope: QueryScope,
    pub text: String,
    #[serde(default)]
    pub node_kinds: Vec<String>,
    #[serde(default)]
    pub paths: Vec<RepoPath>,
    pub page: PageRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ShowLookup {
    Node(NodeId),
    Symbol(SemanticKey),
    Path(RepoPath),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShowRequest {
    pub scope: QueryScope,
    pub lookup: ShowLookup,
    pub page: PageRequest,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeDirection {
    Outgoing,
    Incoming,
    #[default]
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeighborhoodRequest {
    pub scope: QueryScope,
    pub roots: Vec<NodeId>,
    #[serde(default)]
    pub direction: EdgeDirection,
    #[serde(default)]
    pub edge_kinds: Vec<String>,
    pub page: PageRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextRequest {
    pub scope: QueryScope,
    pub seeds: Vec<ContextSeed>,
    pub policy: ContextPolicy,
    pub page: PageRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ContextSeed {
    Node(NodeId),
    Symbol(SemanticKey),
    Path(RepoPath),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPolicy {
    #[serde(default)]
    pub direction: EdgeDirection,
    #[serde(default)]
    pub edge_kinds: Vec<String>,
    #[serde(default)]
    pub include_unresolved: bool,
    #[serde(default)]
    pub include_external: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncationReason {
    Results,
    Bytes,
    Depth,
    Duration,
    Capability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Truncation {
    pub reason: TruncationReason,
    pub returned_results: u32,
    pub returned_bytes: u64,
    pub explored_depth: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageInfo {
    pub next_cursor: Option<PageCursor>,
    pub truncation: Option<Truncation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshnessEnvelope {
    pub freshness: Freshness,
    pub compared_manifest: Option<Digest>,
    #[serde(default)]
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRevisionEnvelope {
    pub id: SourceRevisionId,
    pub manifest_digest: Digest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalAction {
    Index,
    WaitForBuild,
    RetryIndex,
    RefreshIndex,
    Rebuild,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryDiagnostic {
    pub severity: DiagnosticSeverity,
    pub code: DiagnosticCode,
    pub location: Option<DiagnosticLocation>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticsEnvelope {
    pub summary: DiagnosticSummary,
    #[serde(default)]
    pub items: Vec<QueryDiagnostic>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryResponse<T> {
    pub wire_version: u32,
    pub repository: RepositoryRef,
    pub snapshot_id: SnapshotId,
    pub source_revision: SourceRevisionEnvelope,
    pub freshness: FreshnessEnvelope,
    pub diagnostics: DiagnosticsEnvelope,
    pub page: PageInfo,
    pub data: T,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusData {
    pub availability: Availability,
    /// State of the newest build attempt, independently from the published snapshot.
    pub build_state: Option<BuildState>,
    pub build_id: Option<BuildId>,
    pub published_view: Option<PublishedViewName>,
    pub graph_model_version: Option<u32>,
    pub statistics: Option<SnapshotStatistics>,
    pub recommended_action: Option<RetrievalAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusResponse {
    pub wire_version: u32,
    pub repository: RepositoryRef,
    pub snapshot_id: Option<SnapshotId>,
    pub source_revision: Option<SourceRevisionEnvelope>,
    pub freshness: FreshnessEnvelope,
    pub diagnostics: DiagnosticsEnvelope,
    pub page: PageInfo,
    pub data: StatusData,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticSummary {
    pub info: u64,
    pub warning: u64,
    pub error: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotStatistics {
    pub files: u64,
    pub nodes: u64,
    pub edges: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub node_id: NodeId,
    pub kind: String,
    pub semantic_key: Option<SemanticKey>,
    pub path: Option<RepoPath>,
    pub span: Option<SourceSpan>,
    pub provenance: FactProvenance,
    pub match_kind: SearchMatchKind,
    pub score: f64,
    #[serde(default)]
    pub matched_fields: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMatchKind {
    ExactSemanticKey,
    ExactPath,
    ExactNormalizedName,
    NormalizedNamePrefix,
    NormalizedNameContains,
    SemanticKeyContains,
    PathContains,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchData {
    pub hits: Vec<SearchHit>,
}

pub type SearchResponse = QueryResponse<SearchData>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShowData {
    pub nodes: Vec<GraphNode>,
}

pub type ShowResponse = QueryResponse<ShowData>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NeighborhoodNode {
    pub id: NodeId,
    pub kind: String,
    pub semantic_key: Option<SemanticKey>,
    pub path: Option<RepoPath>,
    pub span: Option<SourceSpan>,
    pub provenance: FactProvenance,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NeighborhoodEdge {
    pub id: EdgeId,
    pub kind: String,
    pub source: NodeId,
    pub target: EdgeTarget,
    pub provenance: FactProvenance,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NeighborhoodData {
    pub nodes: Vec<NeighborhoodNode>,
    pub edges: Vec<NeighborhoodEdge>,
}

pub type NeighborhoodResponse = QueryResponse<NeighborhoodData>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextItem {
    pub node_id: NodeId,
    pub kind: String,
    pub semantic_key: Option<SemanticKey>,
    pub path: RepoPath,
    pub span: Option<SourceSpan>,
    pub content_identity: Digest,
    pub provenance: FactProvenance,
    pub selection_reasons: Vec<ContextSelectionReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSelectionReason {
    pub kind: ContextSelectionKind,
    pub via_node: Option<NodeId>,
    pub via_edge: Option<EdgeId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSelectionKind {
    ExactSeed,
    Containment,
    Declaration,
    ResolvedDependency,
    Documentation,
    Configuration,
    Relationship,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextData {
    pub items: Vec<ContextItem>,
    /// Deduplicated, hash-verified source excerpts requested by the caller.
    /// Structural query implementations leave this empty; a trusted
    /// `SnapshotContent` boundary may populate it without changing ranking.
    #[serde(default)]
    pub snippets: Vec<ContextSnippet>,
}

pub type ContextResponse = QueryResponse<ContextData>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSnippet {
    pub path: RepoPath,
    pub span: Option<SourceSpan>,
    pub verified_content_identity: Digest,
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryErrorCode {
    UnsupportedWireVersion,
    InvalidRequest,
    NotBuilt,
    IndexBuilding,
    IndexFailed,
    Incompatible,
    SnapshotNotFound,
    StaleCursor,
    BudgetExceeded,
    BackendUnavailable,
    ContentChanged,
    ContentUnavailable,
}

#[derive(Debug, Error, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[error("{message}")]
pub struct QueryError {
    pub wire_version: u32,
    pub code: QueryErrorCode,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_action: Option<RetrievalAction>,
    #[serde(default)]
    pub details: BTreeMap<String, String>,
}

/// A hash-pinned, repository-confined source read request. Implementations
/// must verify `expected_content_identity` before returning bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentRequest {
    pub wire_version: u32,
    pub repository: RepositoryRef,
    pub snapshot_id: SnapshotId,
    pub path: RepoPath,
    pub expected_content_identity: Digest,
    pub span: Option<SourceSpan>,
    pub max_bytes: NonZeroU64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentResponse {
    pub wire_version: u32,
    pub repository: RepositoryRef,
    pub snapshot_id: SnapshotId,
    pub path: RepoPath,
    pub verified_content_identity: Digest,
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};

    use super::*;
    use crate::repository_graph::domain::{
        Confidence, ExtractorId, ExtractorIdentity, RepositoryId, RepositoryNamespace,
        ResolutionState, SourceEvidence,
    };

    fn repository() -> RepositoryRef {
        RepositoryRef {
            namespace: RepositoryNamespace::new("local:test").unwrap(),
            repository_id: RepositoryId::new("root").unwrap(),
        }
    }

    fn budget() -> QueryBudget {
        QueryBudget::new(
            NonZeroU32::new(20).unwrap(),
            NonZeroU64::new(16_384).unwrap(),
            NonZeroU32::new(2).unwrap(),
            NonZeroU64::new(500).unwrap(),
            NonZeroU32::new(10).unwrap(),
        )
    }

    #[test]
    fn every_query_scope_serializes_explicit_wire_version_and_budgets() {
        let request = SearchRequest {
            scope: QueryScope::current(
                repository(),
                SnapshotSelector::Published(PublishedViewName::new("canonical").unwrap()),
                budget(),
            ),
            text: "RuntimeTaskContext".to_string(),
            node_kinds: vec![],
            paths: vec![],
            page: PageRequest { cursor: None },
        };
        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json["scope"]["wire_version"], QUERY_WIRE_VERSION);
        assert_eq!(json["scope"]["budget"]["max_depth"], 2);
        assert_eq!(json["scope"]["budget"]["max_bytes"], 16_384);
    }

    #[test]
    fn content_requests_reject_absolute_paths_and_zero_budgets() {
        let json = serde_json::json!({
            "wire_version": QUERY_WIRE_VERSION,
            "repository": {
                "namespace": "local:test",
                "repository_id": "root"
            },
            "snapshot_id": "snapshot-1",
            "path": "/etc/passwd",
            "expected_content_identity": {"algorithm": "sha256", "value": "00"},
            "span": null,
            "max_bytes": 0
        });
        assert!(serde_json::from_value::<ContentRequest>(json).is_err());
    }

    #[test]
    fn context_items_are_references_and_do_not_embed_source_bodies() {
        let fields = serde_json::to_value(ContextItem {
            node_id: NodeId::new("node:main").unwrap(),
            kind: "file".to_string(),
            semantic_key: None,
            path: RepoPath::new("src/main.rs").unwrap(),
            span: None,
            content_identity: Digest::new("sha256", "00").unwrap(),
            provenance: FactProvenance {
                extractor: ExtractorIdentity {
                    id: ExtractorId::new("generic").unwrap(),
                    version: "1".to_string(),
                    contract_version: 1,
                },
                evidence: Some(SourceEvidence {
                    path: RepoPath::new("src/main.rs").unwrap(),
                    content_identity: Digest::new("sha256", "00").unwrap(),
                    span: None,
                }),
                resolution: ResolutionState::Resolved,
                confidence: Confidence::Exact,
            },
            selection_reasons: vec![ContextSelectionReason {
                kind: ContextSelectionKind::ExactSeed,
                via_node: None,
                via_edge: None,
            }],
        })
        .unwrap();
        assert!(fields.get("bytes").is_none());
        assert!(fields.get("content").is_none());
    }

    #[test]
    fn context_requests_use_typed_seeds_and_explicit_expansion_policy() {
        let request = ContextRequest {
            scope: QueryScope::current(
                repository(),
                SnapshotSelector::Published(PublishedViewName::new("canonical").unwrap()),
                budget(),
            ),
            seeds: vec![
                ContextSeed::Node(NodeId::new("node:main").unwrap()),
                ContextSeed::Symbol(SemanticKey::new("rust:crate::main").unwrap()),
                ContextSeed::Path(RepoPath::new("src/main.rs").unwrap()),
            ],
            policy: ContextPolicy {
                direction: EdgeDirection::Both,
                edge_kinds: vec!["contains".to_string()],
                include_unresolved: false,
                include_external: false,
            },
            page: PageRequest { cursor: None },
        };
        let json = serde_json::to_value(request).unwrap();

        assert_eq!(json["seeds"][0]["type"], "node");
        assert_eq!(json["seeds"][1]["type"], "symbol");
        assert_eq!(json["seeds"][2]["type"], "path");
        assert_eq!(json["policy"]["direction"], "both");
        assert_eq!(json["scope"]["budget"]["max_diagnostics"], 10);
    }

    #[test]
    fn search_request_serialization_matches_v2_fixture() {
        let request = SearchRequest {
            scope: QueryScope::current(
                repository(),
                SnapshotSelector::Published(PublishedViewName::new("canonical").unwrap()),
                budget(),
            ),
            text: "RuntimeTaskContext".to_string(),
            node_kinds: vec!["rust_symbol".to_string()],
            paths: vec![RepoPath::new("src/project.rs").unwrap()],
            page: PageRequest { cursor: None },
        };
        assert_eq!(
            serde_json::to_string_pretty(&request).unwrap(),
            include_str!("fixtures/query_v2_search.json")
                .trim()
                .replace("\r\n", "\n")
        );
    }

    #[test]
    fn error_details_serialize_in_deterministic_key_order() {
        let error = QueryError {
            wire_version: QUERY_WIRE_VERSION,
            code: QueryErrorCode::StaleCursor,
            message: "cursor no longer matches snapshot".to_string(),
            retryable: false,
            recommended_action: None,
            details: BTreeMap::from([
                ("z".to_string(), "last".to_string()),
                ("a".to_string(), "first".to_string()),
            ]),
        };
        assert_eq!(
            serde_json::to_string_pretty(&error).unwrap(),
            include_str!("fixtures/query_v2_error.json")
                .trim()
                .replace("\r\n", "\n")
        );
    }

    #[test]
    fn status_response_serialization_matches_v2_envelope_fixture() {
        let response = StatusResponse {
            wire_version: QUERY_WIRE_VERSION,
            repository: repository(),
            snapshot_id: Some(SnapshotId::new("snapshot-1").unwrap()),
            source_revision: Some(SourceRevisionEnvelope {
                id: SourceRevisionId::new("revision-1").unwrap(),
                manifest_digest: Digest::new("sha256", "00").unwrap(),
            }),
            freshness: FreshnessEnvelope {
                freshness: Freshness::Fresh,
                compared_manifest: Some(Digest::new("sha256", "00").unwrap()),
                reason_codes: vec![],
            },
            diagnostics: DiagnosticsEnvelope::default(),
            page: PageInfo {
                next_cursor: None,
                truncation: None,
            },
            data: StatusData {
                availability: Availability::Available,
                build_state: Some(BuildState::Published),
                build_id: Some(BuildId::new("build-1").unwrap()),
                published_view: Some(PublishedViewName::new("canonical").unwrap()),
                graph_model_version: Some(1),
                statistics: Some(SnapshotStatistics {
                    files: 2,
                    nodes: 8,
                    edges: 7,
                }),
                recommended_action: None,
            },
        };

        assert_eq!(
            serde_json::to_string_pretty(&response).unwrap(),
            include_str!("fixtures/query_v2_status.json")
                .trim()
                .replace("\r\n", "\n")
        );
    }
}
