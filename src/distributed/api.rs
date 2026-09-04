//! Authenticated, transport-neutral distributed control and query API contracts.

use std::num::{NonZeroU32, NonZeroU64};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    project_memory::domain::{
        MemoryEntity, MemoryEntityId, MemoryEntityKind, MemoryRelationship, MemoryRelationshipKind,
        MemorySourceLocator,
    },
    repository_graph::{
        domain::{
            Digest, GraphDiagnostic, GraphEdge, GraphNode, NodeId, RepoPath, SemanticKey,
            SourceSpan,
        },
        query::EdgeDirection,
    },
};

use super::{
    identity::{RemotePageCursor, RemoteProjectRef, RequestId},
    protocol::{
        CancelIndexJobRequest, IndexJobRecord, InspectIndexJobRequest, RemoteError,
        RemoteQueryRequest, RemoteQueryResponse, SubmitIndexJobRequest,
    },
    publication::{RemoteGraphSnapshotRecord, RemoteMemoryRevisionRecord},
    security::AuthorizationContext,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteControlResponse<T> {
    pub protocol_version: u32,
    pub request_id: RequestId,
    pub project: RemoteProjectRef,
    pub body: T,
}

pub trait RemoteControlApi {
    fn submit_build(
        &mut self,
        authorization: &AuthorizationContext,
        request: &SubmitIndexJobRequest,
        now: DateTime<Utc>,
    ) -> Result<RemoteControlResponse<IndexJobRecord>, RemoteError>;

    fn inspect_build(
        &self,
        authorization: &AuthorizationContext,
        request: &InspectIndexJobRequest,
    ) -> Result<RemoteControlResponse<IndexJobRecord>, RemoteError>;

    fn cancel_build(
        &mut self,
        authorization: &AuthorizationContext,
        request: &CancelIndexJobRequest,
        now: DateTime<Utc>,
    ) -> Result<RemoteControlResponse<IndexJobRecord>, RemoteError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteQueryBudget {
    pub max_results: NonZeroU32,
    pub max_bytes: NonZeroU64,
    pub max_depth: NonZeroU32,
    pub max_duration_ms: NonZeroU64,
    pub max_diagnostics: NonZeroU32,
    pub max_snippet_bytes: NonZeroU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteQueryLimits {
    pub max_results: NonZeroU32,
    pub max_bytes: NonZeroU64,
    pub max_depth: NonZeroU32,
    pub max_duration_ms: NonZeroU64,
    pub max_diagnostics: NonZeroU32,
    pub max_snippet_bytes: NonZeroU64,
}

impl RemoteQueryLimits {
    pub fn clamp(self, requested: RemoteQueryBudget) -> RemoteQueryBudget {
        RemoteQueryBudget {
            max_results: self.max_results.min(requested.max_results),
            max_bytes: self.max_bytes.min(requested.max_bytes),
            max_depth: self.max_depth.min(requested.max_depth),
            max_duration_ms: self.max_duration_ms.min(requested.max_duration_ms),
            max_diagnostics: self.max_diagnostics.min(requested.max_diagnostics),
            max_snippet_bytes: self.max_snippet_bytes.min(requested.max_snippet_bytes),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemotePageRequest {
    pub cursor: Option<RemotePageCursor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteTruncationReason {
    Results,
    Bytes,
    Depth,
    Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemotePageInfo {
    pub next_cursor: Option<RemotePageCursor>,
    pub truncation: Option<RemoteTruncationReason>,
    pub returned_results: u32,
    pub returned_bytes: u64,
    pub explored_depth: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RemoteContextSeed {
    GraphNode(NodeId),
    GraphSymbol(SemanticKey),
    GraphPath(RepoPath),
    MemoryEntity(MemoryEntityId),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteQueryOperation {
    Status,
    Search {
        text: String,
        #[serde(default)]
        graph_kinds: Vec<String>,
        #[serde(default)]
        graph_paths: Vec<RepoPath>,
        #[serde(default)]
        memory_kinds: Vec<MemoryEntityKind>,
    },
    Neighborhood {
        roots: Vec<NodeId>,
        #[serde(default)]
        direction: EdgeDirection,
        #[serde(default)]
        edge_kinds: Vec<String>,
    },
    Context {
        seeds: Vec<RemoteContextSeed>,
        #[serde(default)]
        direction: EdgeDirection,
        #[serde(default)]
        graph_edge_kinds: Vec<String>,
        #[serde(default)]
        memory_relationship_kinds: Vec<MemoryRelationshipKind>,
        #[serde(default)]
        include_unresolved: bool,
        #[serde(default)]
        include_external: bool,
        #[serde(default)]
        include_snippets: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteQueryBody {
    pub budget: RemoteQueryBudget,
    pub page: RemotePageRequest,
    pub operation: RemoteQueryOperation,
}

impl RemoteQueryBody {
    pub fn validate(&self) -> bool {
        const MAX_QUERY_BYTES: usize = 4096;
        const MAX_FILTERS: usize = 128;
        match &self.operation {
            RemoteQueryOperation::Status => self.page.cursor.is_none(),
            RemoteQueryOperation::Search {
                text,
                graph_kinds,
                graph_paths,
                memory_kinds,
            } => {
                !text.trim().is_empty()
                    && text.len() <= MAX_QUERY_BYTES
                    && !text.chars().any(char::is_control)
                    && graph_kinds.len() <= MAX_FILTERS
                    && graph_paths.len() <= MAX_FILTERS
                    && memory_kinds.len() <= MAX_FILTERS
                    && graph_kinds.iter().all(|kind| valid_filter(kind))
            }
            RemoteQueryOperation::Neighborhood {
                roots, edge_kinds, ..
            } => {
                !roots.is_empty()
                    && roots.len() <= MAX_FILTERS
                    && edge_kinds.len() <= MAX_FILTERS
                    && edge_kinds.iter().all(|kind| valid_filter(kind))
            }
            RemoteQueryOperation::Context {
                seeds,
                graph_edge_kinds,
                memory_relationship_kinds,
                ..
            } => {
                !seeds.is_empty()
                    && seeds.len() <= MAX_FILTERS
                    && graph_edge_kinds.len() <= MAX_FILTERS
                    && memory_relationship_kinds.len() <= MAX_FILTERS
                    && graph_edge_kinds.iter().all(|kind| valid_filter(kind))
            }
        }
    }

    pub fn includes_snippets(&self) -> bool {
        matches!(
            self.operation,
            RemoteQueryOperation::Context {
                include_snippets: true,
                ..
            }
        )
    }
}

fn valid_filter(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteStatusData {
    pub graph: Option<RemoteGraphSnapshotRecord>,
    pub memory: Option<RemoteMemoryRevisionRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSearchMatchKind {
    ExactId,
    ExactSemanticKey,
    ExactPath,
    ExactTitle,
    Prefix,
    Contains,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "domain", content = "item", rename_all = "snake_case")]
pub enum RemoteSearchItem {
    Repository {
        node: GraphNode,
        match_kind: RemoteSearchMatchKind,
        score: f64,
    },
    Memory {
        entity: MemoryEntity,
        match_kind: RemoteSearchMatchKind,
        score: f64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteSearchData {
    pub items: Vec<RemoteSearchItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteNeighborhoodData {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "domain", rename_all = "snake_case")]
pub enum RemoteVerifiedSnippet {
    Repository {
        path: RepoPath,
        span: Option<SourceSpan>,
        verified_content_identity: Digest,
        text: String,
        truncated: bool,
    },
    Memory {
        entity_id: MemoryEntityId,
        source_locator: MemorySourceLocator,
        verified_fingerprint: Digest,
        text: String,
        truncated: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteContextData {
    pub graph_nodes: Vec<GraphNode>,
    pub graph_edges: Vec<GraphEdge>,
    pub memory_entities: Vec<MemoryEntity>,
    pub memory_relationships: Vec<MemoryRelationship>,
    pub snippets: Vec<RemoteVerifiedSnippet>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "domain", content = "diagnostic", rename_all = "snake_case")]
pub enum RemoteQueryDiagnostic {
    Repository(GraphDiagnostic),
    Memory(crate::project_memory::diagnostics::MemoryDiagnostic),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum RemoteQueryData {
    Status(Box<RemoteStatusData>),
    Search(RemoteSearchData),
    Neighborhood(RemoteNeighborhoodData),
    Context(Box<RemoteContextData>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteQueryResult {
    pub page: RemotePageInfo,
    pub diagnostics: Vec<RemoteQueryDiagnostic>,
    pub data: RemoteQueryData,
}

pub trait RemoteSnapshotQueryApi {
    fn query(
        &self,
        authorization: &AuthorizationContext,
        request: &RemoteQueryRequest<RemoteQueryBody>,
    ) -> Result<RemoteQueryResponse<RemoteQueryResult>, RemoteError>;
}

#[cfg(test)]
mod tests {
    //! Remote request validation and independent server budget clamps.

    use super::*;

    fn nonzero(value: u64) -> NonZeroU64 {
        NonZeroU64::new(value).unwrap()
    }

    #[test]
    fn server_limits_clamp_every_client_budget() {
        let limits = RemoteQueryLimits {
            max_results: NonZeroU32::new(10).unwrap(),
            max_bytes: nonzero(1_000),
            max_depth: NonZeroU32::new(2).unwrap(),
            max_duration_ms: nonzero(50),
            max_diagnostics: NonZeroU32::new(3).unwrap(),
            max_snippet_bytes: nonzero(100),
        };
        let effective = limits.clamp(RemoteQueryBudget {
            max_results: NonZeroU32::new(100).unwrap(),
            max_bytes: nonzero(10_000),
            max_depth: NonZeroU32::new(20).unwrap(),
            max_duration_ms: nonzero(5_000),
            max_diagnostics: NonZeroU32::new(30).unwrap(),
            max_snippet_bytes: nonzero(1_000),
        });
        assert_eq!(effective.max_results.get(), 10);
        assert_eq!(effective.max_bytes.get(), 1_000);
        assert_eq!(effective.max_depth.get(), 2);
        assert_eq!(effective.max_duration_ms.get(), 50);
        assert_eq!(effective.max_diagnostics.get(), 3);
        assert_eq!(effective.max_snippet_bytes.get(), 100);
    }

    #[test]
    fn query_body_rejects_unbounded_or_malformed_shapes() {
        let budget = RemoteQueryBudget {
            max_results: NonZeroU32::new(10).unwrap(),
            max_bytes: nonzero(10_000),
            max_depth: NonZeroU32::new(2).unwrap(),
            max_duration_ms: nonzero(100),
            max_diagnostics: NonZeroU32::new(3).unwrap(),
            max_snippet_bytes: nonzero(100),
        };
        let body = |operation| RemoteQueryBody {
            budget,
            page: RemotePageRequest { cursor: None },
            operation,
        };
        assert!(
            !body(RemoteQueryOperation::Search {
                text: " ".to_string(),
                graph_kinds: Vec::new(),
                graph_paths: Vec::new(),
                memory_kinds: Vec::new(),
            })
            .validate()
        );
        assert!(
            !body(RemoteQueryOperation::Neighborhood {
                roots: Vec::new(),
                direction: EdgeDirection::Both,
                edge_kinds: Vec::new(),
            })
            .validate()
        );
        assert!(
            !body(RemoteQueryOperation::Context {
                seeds: Vec::new(),
                direction: EdgeDirection::Both,
                graph_edge_kinds: Vec::new(),
                memory_relationship_kinds: Vec::new(),
                include_unresolved: false,
                include_external: false,
                include_snippets: false,
            })
            .validate()
        );
    }
}
