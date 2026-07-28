use std::{
    collections::BTreeMap,
    num::{NonZeroU32, NonZeroU64},
    time::Instant,
};

use anyhow::{Context, Result};
use neva::prelude::*;
use serde::Deserialize;

use crate::{
    repository_graph::{
        QUERY_WIRE_VERSION,
        config::QueryLimitsConfig,
        domain::{PageCursor, QueryBudget, RepoPath},
        query::{PageRequest, QueryError, QueryErrorCode, SearchRequest},
    },
    repository_graph_runtime::LocalGraphContext,
};

use super::{repository_query_telemetry, tool_err};

const MAX_QUERY_BYTES: usize = 512;
const MAX_FILTERS: usize = 32;
const MAX_FILTER_BYTES: usize = 512;
const MAX_CURSOR_BYTES: usize = 16 * 1024;

pub const DESCRIPTION: &str = "Search the published repository graph using exact path, semantic-key, \
     normalized-name, and bounded textual matches. Supports node-kind and repository-relative path \
     filters plus snapshot-bound continuation cursors. Results are deterministic, evidence-backed, \
     freshness-labeled, and capped by server limits. This read-only tool requires no task lease and \
     never changes task or run state. An absent relationship means only that it is not known by this \
     index, not that the relationship does not exist.";

pub const INPUT_SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "input": {
            "type": "object",
            "description": "Bounded repository graph search request",
            "properties": {
                "query": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 512,
                    "description": "Name, semantic key, or repository-relative path to find"
                },
                "kinds": {
                    "type": "array",
                    "maxItems": 32,
                    "items": { "type": "string", "minLength": 1, "maxLength": 512 },
                    "description": "Optional exact node-kind filters"
                },
                "paths": {
                    "type": "array",
                    "maxItems": 32,
                    "items": { "type": "string", "minLength": 1, "maxLength": 512 },
                    "description": "Optional repository-relative path-prefix filters"
                },
                "max_results": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Requested result cap; the configured server cap wins"
                },
                "max_bytes": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Requested response byte cap; the configured server cap wins"
                },
                "max_duration_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Requested query duration cap; the configured server cap wins"
                },
                "max_diagnostics": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Requested diagnostic cap; the configured server cap wins"
                },
                "cursor": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 16384,
                    "description": "Opaque continuation cursor from the same search and snapshot"
                }
            },
            "required": ["query"],
            "additionalProperties": false
        }
    },
    "required": ["input"],
    "additionalProperties": false
}"#;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositorySearchInput {
    query: String,
    #[serde(default)]
    kinds: Vec<String>,
    #[serde(default)]
    paths: Vec<String>,
    max_results: Option<u32>,
    max_bytes: Option<u64>,
    max_duration_ms: Option<u64>,
    max_diagnostics: Option<u32>,
    cursor: Option<String>,
}

pub async fn handler(
    ctx: neva::di::Dc<crate::server::ServerContext>,
    input: serde_json::Value,
) -> Result<String, Error> {
    handler_for_agent(ctx.agent_id(), input).await
}

pub async fn handler_for_agent(agent_id: &str, input: serde_json::Value) -> Result<String, Error> {
    let input = match parse_input(input) {
        Ok(input) => input,
        Err(error) => return serialize_invalid_request(&error).map_err(tool_err),
    };
    run(Some(agent_id), input).await.map_err(tool_err)
}

#[cfg(test)]
pub(super) async fn run_without_agent(input: serde_json::Value) -> Result<String> {
    let input = parse_input(input)?;
    run(None, input).await
}

async fn run(agent_id: Option<&str>, input: RepositorySearchInput) -> Result<String> {
    let started = Instant::now();
    let context = match agent_id {
        Some(agent_id) => LocalGraphContext::load_for_agent(false, agent_id).await?,
        None => LocalGraphContext::load(false).await?,
    };
    let request = match search_request(&context, input) {
        Ok(request) => request,
        Err(error) => return serialize_invalid_request(&error),
    };
    let response = context.search(&request).await?;
    let serialized = match &response {
        Ok(response) => serde_json::to_string(response)?,
        Err(error) => serde_json::to_string(error)?,
    };
    repository_query_telemetry::search(&context, started, &response, serialized.len());
    Ok(serialized)
}

fn serialize_invalid_request(error: &anyhow::Error) -> Result<String> {
    Ok(serde_json::to_string(&QueryError {
        wire_version: QUERY_WIRE_VERSION,
        code: QueryErrorCode::InvalidRequest,
        message: error.to_string(),
        retryable: false,
        recommended_action: None,
        details: BTreeMap::new(),
    })?)
}

fn parse_input(input: serde_json::Value) -> Result<RepositorySearchInput> {
    let input: RepositorySearchInput = serde_json::from_value(input)
        .context("repository_search expects an input object matching its schema")?;
    if input.query.trim().is_empty() {
        anyhow::bail!("repository_search query must not be empty");
    }
    if input.query.len() > MAX_QUERY_BYTES {
        anyhow::bail!("repository_search query exceeds {MAX_QUERY_BYTES} bytes");
    }
    validate_filters("kinds", &input.kinds)?;
    validate_filters("paths", &input.paths)?;
    if input
        .cursor
        .as_ref()
        .is_some_and(|cursor| cursor.len() > MAX_CURSOR_BYTES)
    {
        anyhow::bail!("repository_search cursor exceeds {MAX_CURSOR_BYTES} bytes");
    }
    Ok(input)
}

fn validate_filters(name: &str, values: &[String]) -> Result<()> {
    if values.len() > MAX_FILTERS {
        anyhow::bail!("repository_search {name} exceeds {MAX_FILTERS} entries");
    }
    if values.iter().any(|value| value.trim().is_empty()) {
        anyhow::bail!("repository_search {name} must not contain empty entries");
    }
    if values.iter().any(|value| value.len() > MAX_FILTER_BYTES) {
        anyhow::bail!("repository_search {name} entries exceed {MAX_FILTER_BYTES} bytes");
    }
    Ok(())
}

fn search_request(
    context: &LocalGraphContext,
    input: RepositorySearchInput,
) -> Result<SearchRequest> {
    let budget = requested_budget(&context.config.query_limits, &input)?;
    Ok(SearchRequest {
        scope: context.scope(budget)?,
        text: input.query.trim().to_string(),
        node_kinds: input
            .kinds
            .into_iter()
            .map(|kind| kind.trim().to_string())
            .collect(),
        paths: input
            .paths
            .into_iter()
            .map(|path| RepoPath::new(path.trim()))
            .collect::<Result<Vec<_>, _>>()
            .context("repository_search paths must be confined repository-relative paths")?,
        page: PageRequest {
            cursor: input
                .cursor
                .map(|cursor| PageCursor::new(cursor.trim().to_string()))
                .transpose()?,
        },
    })
}

fn requested_budget(
    limits: &QueryLimitsConfig,
    input: &RepositorySearchInput,
) -> Result<QueryBudget> {
    Ok(QueryBudget::new(
        NonZeroU32::new(input.max_results.unwrap_or(limits.max_results))
            .context("repository_search max_results must be greater than zero")?,
        NonZeroU64::new(input.max_bytes.unwrap_or(limits.max_bytes))
            .context("repository_search max_bytes must be greater than zero")?,
        NonZeroU32::new(limits.max_depth)
            .context("repository_graph.query_limits.max_depth must be greater than zero")?,
        NonZeroU64::new(input.max_duration_ms.unwrap_or(limits.max_duration_ms))
            .context("repository_search max_duration_ms must be greater than zero")?,
        NonZeroU32::new(input.max_diagnostics.unwrap_or(limits.max_diagnostics))
            .context("repository_search max_diagnostics must be greater than zero")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use neva::types::CallToolRequestParams;
    use std::collections::HashMap;

    #[test]
    fn schema_is_bounded_and_rejects_unknown_fields() {
        let schema: serde_json::Value = serde_json::from_str(INPUT_SCHEMA).unwrap();
        let input = &schema["properties"]["input"];
        assert_eq!(input["additionalProperties"], false);
        assert_eq!(input["properties"]["query"]["maxLength"], 512);
        assert_eq!(input["properties"]["kinds"]["maxItems"], 32);
        assert_eq!(input["properties"]["paths"]["maxItems"], 32);

        assert!(parse_input(serde_json::json!({"query": "main", "unknown": true})).is_err());
    }

    #[tokio::test]
    async fn boundary_validation_returns_a_versioned_query_error() {
        let response = handler_for_agent("executor:codex:1", serde_json::json!({"query": "   "}))
            .await
            .unwrap();
        let response: QueryError = serde_json::from_str(&response).unwrap();

        assert_eq!(response.wire_version, QUERY_WIRE_VERSION);
        assert_eq!(response.code, QueryErrorCode::InvalidRequest);
        assert!(!response.retryable);
    }

    #[test]
    fn neva_extracts_wrapped_search_input_as_one_argument() {
        let params = CallToolRequestParams {
            name: "repository_search".to_string(),
            args: Some(HashMap::from([(
                "input".to_string(),
                serde_json::json!({
                    "query": "RuntimeTaskContext",
                    "kinds": ["rust.struct"],
                    "paths": ["src"]
                }),
            )])),
            meta: None,
        };

        let (input,): (serde_json::Value,) = params.try_into().unwrap();
        let input = parse_input(input).unwrap();

        assert_eq!(input.query, "RuntimeTaskContext");
        assert_eq!(input.kinds, ["rust.struct"]);
        assert_eq!(input.paths, ["src"]);
    }

    #[test]
    fn request_preserves_filters_cursor_and_client_budgets() {
        let directory = tempfile::tempdir().unwrap();
        let context = LocalGraphContext {
            project_root: directory.path().to_path_buf(),
            root: directory.path().to_path_buf(),
            repository: crate::repository_graph::domain::RepositoryRef {
                namespace: crate::repository_graph::domain::RepositoryNamespace::new("local:test")
                    .unwrap(),
                repository_id: crate::repository_graph::domain::RepositoryId::new("root").unwrap(),
            },
            config: crate::repository_graph::config::RepositoryGraphConfig::default(),
            repository_view: None,
            task_view_id: None,
            run_id: None,
        };
        let input = parse_input(serde_json::json!({
            "query": "  crate::api  ",
            "kinds": [" rust.struct "],
            "paths": ["src/api"],
            "max_results": 7,
            "max_bytes": 4096,
            "max_duration_ms": 50,
            "max_diagnostics": 3,
            "cursor": "opaque"
        }))
        .unwrap();

        let request = search_request(&context, input).unwrap();

        assert_eq!(request.text, "crate::api");
        assert_eq!(request.node_kinds, ["rust.struct"]);
        assert_eq!(request.paths[0].as_str(), "src/api");
        assert_eq!(request.scope.budget.max_results.get(), 7);
        assert_eq!(request.scope.budget.max_bytes.get(), 4096);
        assert_eq!(request.scope.budget.max_duration_ms.get(), 50);
        assert_eq!(request.scope.budget.max_diagnostics.get(), 3);
        assert_eq!(request.page.cursor.unwrap().as_str(), "opaque");
    }

    #[test]
    fn invalid_paths_and_zero_budgets_are_rejected_before_querying() {
        let directory = tempfile::tempdir().unwrap();
        let context = LocalGraphContext {
            project_root: directory.path().to_path_buf(),
            root: directory.path().to_path_buf(),
            repository: crate::repository_graph::domain::RepositoryRef {
                namespace: crate::repository_graph::domain::RepositoryNamespace::new("local:test")
                    .unwrap(),
                repository_id: crate::repository_graph::domain::RepositoryId::new("root").unwrap(),
            },
            config: crate::repository_graph::config::RepositoryGraphConfig::default(),
            repository_view: None,
            task_view_id: None,
            run_id: None,
        };

        let invalid_path = parse_input(serde_json::json!({
            "query": "main",
            "paths": ["../secret"]
        }))
        .unwrap();
        assert!(search_request(&context, invalid_path).is_err());

        let zero_budget = parse_input(serde_json::json!({
            "query": "main",
            "max_results": 0
        }))
        .unwrap();
        assert!(search_request(&context, zero_budget).is_err());
    }
}
