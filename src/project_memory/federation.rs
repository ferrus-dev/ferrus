//! Explicit federation contracts for repository structure and project memory.

use serde::{Deserialize, Serialize};

use crate::repository_graph::{
    domain::{RepoPath, RepositoryRef, SnapshotId},
    query::{
        ContextItem, ContextPolicy, ContextSeed, ContextSnippet, DiagnosticsEnvelope,
        FreshnessEnvelope, PageInfo, SearchHit, SnapshotSelector, TaskViewEnvelope,
    },
};

use super::{
    FEDERATION_WIRE_VERSION,
    diagnostics::MemoryDiagnostic,
    domain::{
        FederationPageCursor, MemoryEntityKind, MemoryQueryText, MemoryRecordId,
        MemoryRelationship, MemoryRevisionId, MemorySourceCategory, MemoryStatusToken, ProjectRef,
    },
    query::{
        MemoryContextItem, MemoryContextPolicy, MemoryFreshnessEnvelope, MemoryQueryBudget,
        MemoryRevisionSelector, MemorySearchHit, MemoryTruncation,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextDomain {
    Repository,
    Memory,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryContextTarget {
    pub repository: RepositoryRef,
    pub snapshot: SnapshotSelector,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "domain", rename_all = "snake_case")]
pub enum FederatedTarget {
    Repository {
        repository: RepositoryContextTarget,
    },
    Memory {
        memory: MemoryRevisionSelector,
    },
    All {
        repository: RepositoryContextTarget,
        memory: MemoryRevisionSelector,
    },
}

impl FederatedTarget {
    pub fn domain(&self) -> ContextDomain {
        match self {
            Self::Repository { .. } => ContextDomain::Repository,
            Self::Memory { .. } => ContextDomain::Memory,
            Self::All { .. } => ContextDomain::All,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FederatedScope {
    pub wire_version: u32,
    pub project: ProjectRef,
    pub target: FederatedTarget,
    pub budget: MemoryQueryBudget,
}

impl FederatedScope {
    pub fn current(
        project: ProjectRef,
        target: FederatedTarget,
        budget: MemoryQueryBudget,
    ) -> Self {
        Self {
            wire_version: FEDERATION_WIRE_VERSION,
            project,
            target,
            budget,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FederatedSearchRequest {
    pub scope: FederatedScope,
    pub text: MemoryQueryText,
    #[serde(default)]
    pub repository_kinds: Vec<MemoryStatusToken>,
    #[serde(default)]
    pub repository_paths: Vec<RepoPath>,
    #[serde(default)]
    pub memory_kinds: Vec<MemoryEntityKind>,
    #[serde(default)]
    pub memory_sources: Vec<MemorySourceCategory>,
    pub cursor: Option<FederationPageCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "domain", content = "seed", rename_all = "snake_case")]
pub enum FederatedContextSeed {
    Repository(ContextSeed),
    MemoryEntity(super::domain::MemoryEntityId),
    Milestone(MemoryRecordId),
    Task(MemoryRecordId),
    Run(MemoryRecordId),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FederatedContextRequest {
    pub scope: FederatedScope,
    pub seeds: Vec<FederatedContextSeed>,
    pub repository_policy: ContextPolicy,
    pub memory_policy: MemoryContextPolicy,
    pub cursor: Option<FederationPageCursor>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FederatedPageInfo {
    pub next_cursor: Option<FederationPageCursor>,
    pub truncation: Option<MemoryTruncation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryDomainState {
    pub repository: RepositoryRef,
    pub snapshot_id: Option<SnapshotId>,
    pub task_view: Option<TaskViewEnvelope>,
    pub freshness: FreshnessEnvelope,
    pub diagnostics: DiagnosticsEnvelope,
    pub page: PageInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryDomainState {
    pub revision_id: Option<MemoryRevisionId>,
    pub freshness: MemoryFreshnessEnvelope,
    pub authorized_sources: Vec<MemorySourceCategory>,
    pub diagnostics: Vec<MemoryDiagnostic>,
    pub page: FederatedPageInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "domain", content = "result", rename_all = "snake_case")]
pub enum FederatedSearchResult {
    Repository(SearchHit),
    Memory(MemorySearchHit),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FederatedSearchResponse {
    pub wire_version: u32,
    pub project: ProjectRef,
    pub requested_domain: ContextDomain,
    pub repository: Option<RepositoryDomainState>,
    pub memory: Option<MemoryDomainState>,
    pub federation_diagnostics: Vec<MemoryDiagnostic>,
    pub page: FederatedPageInfo,
    pub results: Vec<FederatedSearchResult>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "domain", content = "item", rename_all = "snake_case")]
pub enum FederatedContextItem {
    Repository(Box<ContextItem>),
    Memory(Box<MemoryContextItem>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FederatedContextResponse {
    pub wire_version: u32,
    pub project: ProjectRef,
    pub requested_domain: ContextDomain,
    pub repository: Option<RepositoryDomainState>,
    pub memory: Option<MemoryDomainState>,
    pub federation_diagnostics: Vec<MemoryDiagnostic>,
    pub page: FederatedPageInfo,
    pub items: Vec<FederatedContextItem>,
    #[serde(default)]
    pub memory_relationships: Vec<MemoryRelationship>,
    #[serde(default)]
    pub cross_domain_links: Vec<MemoryRelationship>,
    #[serde(default)]
    pub repository_snippets: Vec<ContextSnippet>,
}

#[cfg(test)]
mod tests {
    //! Federated target selectors cannot contradict their domain.

    use super::*;

    #[test]
    fn target_shape_cannot_mix_domain_and_selectors() {
        let invalid = serde_json::json!({
            "domain": "repository",
            "memory": {"type": "published", "value": "project"}
        });
        assert!(serde_json::from_value::<FederatedTarget>(invalid).is_err());
    }
}
