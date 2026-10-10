//! Shared bounded repository and memory request contracts.

use crate::{
    project_memory::{
        domain::{MemoryEntityKind, MemoryStatusToken},
        federation::{self, ContextDomain, FederatedContextSeed},
    },
    repository_graph::{
        config::QueryLimitsConfig,
        domain::{QueryBudget, RepoPath},
        query::{self, ContextSeed},
    },
};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(feature = "nano-mcp")]
use serde_json::json;
use std::num::{NonZeroU32, NonZeroU64};

pub(crate) const NAMES: &[&str] = &[
    "repository_graph_status",
    "repository_search",
    "repository_context",
    "project_memory_status",
    "project_context_search",
    "project_context",
    "repository_fallback",
];

pub(super) fn fields(name: &str) -> &'static [&'static str] {
    match name {
        "repository_graph_status" => &[],
        "project_memory_status" => &[
            "max_results",
            "max_bytes",
            "max_duration_ms",
            "max_diagnostics",
        ],
        "repository_search" => &[
            "query",
            "paths",
            "kinds",
            "cursor",
            "max_results",
            "max_bytes",
            "max_duration_ms",
            "max_diagnostics",
        ],
        "project_context_search" => &[
            "domain",
            "query",
            "paths",
            "kinds",
            "cursor",
            "max_results",
            "max_bytes",
            "max_duration_ms",
            "max_diagnostics",
        ],
        "repository_context" => &[
            "seeds",
            "cursor",
            "max_results",
            "max_bytes",
            "max_depth",
            "max_duration_ms",
            "max_diagnostics",
            "max_snippet_bytes",
            "include_snippets",
            "include_unresolved",
            "direction",
        ],
        "project_context" => &[
            "domain",
            "seeds",
            "cursor",
            "max_results",
            "max_bytes",
            "max_depth",
            "max_duration_ms",
            "max_diagnostics",
            "max_snippet_bytes",
            "include_snippets",
            "include_unresolved",
            "include_stale",
            "direction",
        ],
        _ => &[],
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub(crate) domain: Option<ContextDomain>,
    pub(crate) query: Option<String>,
    #[serde(default)]
    pub(crate) paths: Vec<String>,
    #[serde(default)]
    pub(crate) kinds: Vec<String>,
    #[serde(default)]
    pub(crate) seeds: Vec<Seed>,
    pub(crate) cursor: Option<String>,
    pub(crate) max_results: Option<u32>,
    pub(crate) max_bytes: Option<u64>,
    pub(crate) max_depth: Option<u32>,
    pub(crate) max_duration_ms: Option<u64>,
    pub(crate) max_diagnostics: Option<u32>,
    pub(crate) max_snippet_bytes: Option<u64>,
    #[serde(default)]
    pub(crate) include_snippets: bool,
    #[serde(default)]
    pub(crate) include_unresolved: bool,
    #[serde(default)]
    pub(crate) include_stale: bool,
    #[serde(default)]
    pub(crate) direction: query::EdgeDirection,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum Seed {
    Node(String),
    Symbol(String),
    Path(String),
    MemoryEntity(String),
    Milestone(String),
    Task(String),
    Run(String),
}

impl Seed {
    fn value(&self) -> &str {
        match self {
            Self::Node(s)
            | Self::Symbol(s)
            | Self::Path(s)
            | Self::MemoryEntity(s)
            | Self::Milestone(s)
            | Self::Task(s)
            | Self::Run(s) => s,
        }
    }

    pub(crate) fn native(&self) -> Result<FederatedContextSeed> {
        use crate::{
            project_memory::domain::{MemoryEntityId, MemoryRecordId},
            repository_graph::domain::{NodeId, SemanticKey},
        };

        Ok(match self {
            Self::Node(v) => FederatedContextSeed::Repository(ContextSeed::Node(NodeId::new(v)?)),
            Self::Symbol(v) => {
                FederatedContextSeed::Repository(ContextSeed::Symbol(SemanticKey::new(v)?))
            }
            Self::Path(v) => FederatedContextSeed::Repository(ContextSeed::Path(RepoPath::new(v)?)),
            Self::MemoryEntity(v) => FederatedContextSeed::MemoryEntity(MemoryEntityId::new(v)?),
            Self::Milestone(v) => FederatedContextSeed::Milestone(MemoryRecordId::new(v)?),
            Self::Task(v) => FederatedContextSeed::Task(MemoryRecordId::new(v)?),
            Self::Run(v) => FederatedContextSeed::Run(MemoryRecordId::new(v)?),
        })
    }
}

impl Request {
    pub(crate) fn parse(name: &str, value: Value) -> Result<Self> {
        let object = value
            .as_object()
            .context("Expected context request object")?;

        ensure!(
            object
                .keys()
                .all(|key| fields(name).contains(&key.as_str())),
            "Unsupported context field"
        );

        let r: Self = serde_json::from_value(value)?;
        ensure!(NAMES.contains(&name), "Unknown context tool");

        if name.starts_with("project_context") {
            r.domain.context("Explicit domain is required")?;
        } else {
            ensure!(r.domain.is_none(), "This tool has a fixed domain");
        }

        if name.ends_with("search") {
            let query = r.query.as_deref().context("Query is required")?;
            ensure!(
                !query.trim().is_empty() && query.len() <= 512,
                "Invalid query"
            );
        }

        if name == "repository_context" || name == "project_context" {
            ensure!(
                !r.seeds.is_empty() && r.seeds.len() <= 32,
                "Expected 1..=32 seeds"
            );
        }

        ensure!(
            r.paths.len() <= 32 && r.kinds.len() <= 32 && r.seeds.len() <= 32,
            "Too many filters"
        );

        for value in r
            .paths
            .iter()
            .chain(&r.kinds)
            .map(String::as_str)
            .chain(r.seeds.iter().map(Seed::value))
        {
            ensure!(
                !value.trim().is_empty() && value.len() <= 512,
                "Invalid filter or seed"
            );
        }

        for path in &r.paths {
            RepoPath::new(path)?;
        }

        for seed in &r.seeds {
            let native = seed.native()?;
            match r.domain.unwrap_or(ContextDomain::Repository) {
                ContextDomain::Repository => ensure!(
                    matches!(native, FederatedContextSeed::Repository(_)),
                    "Repository seeds required"
                ),
                ContextDomain::Memory => ensure!(
                    !matches!(
                        native,
                        FederatedContextSeed::Repository(ContextSeed::Node(_))
                    ),
                    "Memory cannot resolve node IDs"
                ),
                ContextDomain::All => (),
            }
        }

        ensure!(
            r.cursor
                .as_ref()
                .is_none_or(|s| !s.is_empty() && s.len() <= 16384),
            "Invalid cursor"
        );

        for n in [
            r.max_results.map(u64::from),
            r.max_bytes,
            r.max_depth.map(u64::from),
            r.max_duration_ms,
            r.max_diagnostics.map(u64::from),
            r.max_snippet_bytes,
        ]
        .into_iter()
        .flatten()
        {
            ensure!(n > 0, "Query limits must be positive");
        }

        if name == "project_context_search" {
            r.search_kind_filters()?;
        }
        Ok(r)
    }

    /// Memory has a closed kind vocabulary; repository kinds are backend tokens.
    pub(crate) fn search_kind_filters(
        &self,
    ) -> Result<(Vec<MemoryStatusToken>, Vec<MemoryEntityKind>)> {
        let domain = self.domain.context("Explicit domain is required")?;
        let mut repository = Vec::new();
        let mut memory = Vec::new();
        for kind in &self.kinds {
            let parsed = serde_json::from_value::<MemoryEntityKind>(Value::String(kind.clone()));
            match (domain, parsed) {
                (ContextDomain::Memory, parsed) => {
                    memory.push(parsed.context("Unknown memory kind")?)
                }
                (ContextDomain::All, Ok(kind)) => memory.push(kind),
                _ => repository.push(MemoryStatusToken::new(kind)?),
            }
        }
        Ok((repository, memory))
    }

    pub(crate) fn graph_budget(&self, limits: &QueryLimitsConfig) -> Result<QueryBudget> {
        let default = crate::repository_graph::query_sqlite::default_budget(limits)?;

        Ok(QueryBudget::new(
            NonZeroU32::new(
                self.max_results
                    .unwrap_or(default.max_results.get())
                    .min(64),
            )
            .unwrap(),
            NonZeroU64::new(
                self.max_bytes
                    .unwrap_or(default.max_bytes.get())
                    .min(24 * 1024),
            )
            .unwrap(),
            NonZeroU32::new(self.max_depth.unwrap_or(default.max_depth.get()).min(8)).unwrap(),
            NonZeroU64::new(
                self.max_duration_ms
                    .unwrap_or(default.max_duration_ms.get())
                    .min(1000),
            )
            .unwrap(),
            NonZeroU32::new(
                self.max_diagnostics
                    .unwrap_or(default.max_diagnostics.get())
                    .min(16),
            )
            .unwrap(),
        ))
    }

    #[cfg(feature = "nano-mcp")]
    pub(crate) fn with_graph_budget(
        &self,
        mut value: Value,
        limits: &QueryLimitsConfig,
    ) -> Result<Value> {
        let budget = self.graph_budget(limits)?;
        let fields = value
            .as_object_mut()
            .context("Expected graph request object")?;
        fields.insert("max_results".into(), json!(budget.max_results.get()));
        fields.insert("max_bytes".into(), json!(budget.max_bytes.get()));
        fields.insert(
            "max_duration_ms".into(),
            json!(budget.max_duration_ms.get()),
        );
        fields.insert(
            "max_diagnostics".into(),
            json!(budget.max_diagnostics.get()),
        );
        if !self.seeds.is_empty() {
            fields.insert("max_depth".into(), json!(budget.max_depth.get()));
            if self.include_snippets {
                fields.insert(
                    "max_snippet_bytes".into(),
                    json!(self.max_snippet_bytes.unwrap_or(4096).min(8192)),
                );
            }
        }
        Ok(value)
    }
}

/// Keep typed domain responses up to the final model-tool encoding boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "result", rename_all = "snake_case")]
pub(crate) enum Response {
    RepositoryStatus(query::StatusResponse),
    RepositorySearch(std::result::Result<query::SearchResponse, query::QueryError>),
    RepositoryContext(std::result::Result<query::ContextResponse, query::QueryError>),
    MemoryStatus(crate::project_memory::query::MemoryStatusResponse),
    ProjectSearch(std::result::Result<federation::FederatedSearchResponse, String>),
    ProjectContext(std::result::Result<federation::FederatedContextResponse, String>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn search_kind_filters_follow_the_selected_domain() {
        let parse = |domain, kinds| {
            Request::parse(
                "project_context_search",
                json!({
                    "domain":domain,"query":"context","kinds":kinds
                }),
            )
        };
        let (repository, memory) = parse("memory", vec!["milestone"])
            .unwrap()
            .search_kind_filters()
            .unwrap();
        assert!(repository.is_empty());
        assert_eq!(memory, vec![MemoryEntityKind::Milestone]);
        assert!(parse("memory", vec!["function"]).is_err());
        let (repository, memory) = parse("all", vec!["function", "milestone"])
            .unwrap()
            .search_kind_filters()
            .unwrap();
        assert_eq!(
            repository,
            vec![MemoryStatusToken::new("function").unwrap()]
        );
        assert_eq!(memory, vec![MemoryEntityKind::Milestone]);
        let (repository, memory) = parse("repository", vec!["milestone"])
            .unwrap()
            .search_kind_filters()
            .unwrap();
        assert_eq!(
            repository,
            vec![MemoryStatusToken::new("milestone").unwrap()]
        );
        assert!(memory.is_empty());
    }
}
