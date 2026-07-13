//! Versioned, bounded request and response contracts.
//!
//! Query execution is intentionally deferred. These DTOs are portable across
//! the local SQLite and future remote implementations and never carry absolute
//! workspace paths or backend-specific values.

use std::{collections::BTreeMap, num::NonZeroU64};

use serde::{Deserialize, Serialize};

use super::{
    QUERY_WIRE_VERSION,
    domain::{
        Availability, BuildState, Digest, EdgeId, EdgeTarget, Freshness, NodeId, PageCursor,
        PublishedViewName, QueryBudget, RepoPath, RepositoryRef, SemanticKey, SnapshotId,
        SourceSpan,
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
    pub fn v1(repository: RepositoryRef, snapshot: SnapshotSelector, budget: QueryBudget) -> Self {
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
pub struct NeighborhoodRequest {
    pub scope: QueryScope,
    pub roots: Vec<NodeId>,
    #[serde(default)]
    pub edge_kinds: Vec<String>,
    pub page: PageRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextRequest {
    pub scope: QueryScope,
    pub objective: String,
    #[serde(default)]
    pub anchors: Vec<NodeId>,
    #[serde(default)]
    pub paths: Vec<RepoPath>,
    pub page: PageRequest,
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
pub struct QueryResponse<T> {
    pub wire_version: u32,
    pub repository: RepositoryRef,
    pub snapshot_id: SnapshotId,
    pub freshness: FreshnessEnvelope,
    pub page: PageInfo,
    pub data: T,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusData {
    pub availability: Availability,
    pub build_state: Option<BuildState>,
    pub published_view: Option<PublishedViewName>,
    pub graph_model_version: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusResponse {
    pub wire_version: u32,
    pub repository: RepositoryRef,
    pub snapshot_id: Option<SnapshotId>,
    pub freshness: FreshnessEnvelope,
    pub data: StatusData,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub node_id: NodeId,
    pub kind: String,
    pub semantic_key: Option<SemanticKey>,
    pub path: Option<RepoPath>,
    pub span: Option<SourceSpan>,
    pub score: f64,
    #[serde(default)]
    pub matched_fields: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchData {
    pub hits: Vec<SearchHit>,
}

pub type SearchResponse = QueryResponse<SearchData>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeighborhoodNode {
    pub id: NodeId,
    pub kind: String,
    pub semantic_key: Option<SemanticKey>,
    pub path: Option<RepoPath>,
    pub span: Option<SourceSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeighborhoodEdge {
    pub id: EdgeId,
    pub kind: String,
    pub source: NodeId,
    pub target: EdgeTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeighborhoodData {
    pub nodes: Vec<NeighborhoodNode>,
    pub edges: Vec<NeighborhoodEdge>,
}

pub type NeighborhoodResponse = QueryResponse<NeighborhoodData>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextItem {
    pub node_id: Option<NodeId>,
    pub path: RepoPath,
    pub span: Option<SourceSpan>,
    pub content_identity: Digest,
    pub relevance_reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextData {
    pub items: Vec<ContextItem>,
}

pub type ContextResponse = QueryResponse<ContextData>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryErrorCode {
    UnsupportedWireVersion,
    InvalidRequest,
    NotBuilt,
    Incompatible,
    SnapshotNotFound,
    StaleCursor,
    BudgetExceeded,
    BackendUnavailable,
    ContentChanged,
    ContentUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryError {
    pub wire_version: u32,
    pub code: QueryErrorCode,
    pub message: String,
    pub retryable: bool,
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
    use crate::repository_graph::domain::{RepositoryId, RepositoryNamespace};

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
        )
    }

    #[test]
    fn every_query_scope_serializes_explicit_wire_version_and_budgets() {
        let request = SearchRequest {
            scope: QueryScope::v1(
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
            node_id: None,
            path: RepoPath::new("src/main.rs").unwrap(),
            span: None,
            content_identity: Digest::new("sha256", "00").unwrap(),
            relevance_reason: "entry point".to_string(),
        })
        .unwrap();
        assert!(fields.get("bytes").is_none());
        assert!(fields.get("content").is_none());
    }

    #[test]
    fn search_request_serialization_matches_v1_fixture() {
        let request = SearchRequest {
            scope: QueryScope::v1(
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
            include_str!("fixtures/query_v1_search.json")
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
            details: BTreeMap::from([
                ("z".to_string(), "last".to_string()),
                ("a".to_string(), "first".to_string()),
            ]),
        };
        assert_eq!(
            serde_json::to_string_pretty(&error).unwrap(),
            include_str!("fixtures/query_v1_error.json")
                .trim()
                .replace("\r\n", "\n")
        );
    }
}
