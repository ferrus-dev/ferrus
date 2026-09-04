//! Validate repository context requests and retrieve the caller's scoped graph view.

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
        domain::{NodeId, PageCursor, QueryBudget, RepoPath, SemanticKey},
        query::{
            ContextPolicy, ContextRequest, ContextSeed, EdgeDirection, PageRequest, QueryError,
            QueryErrorCode,
        },
    },
    repository_graph_runtime::LocalGraphContext,
};

use super::{repository_query_telemetry, tool_err};

const MAX_SEEDS: usize = 32;
const MAX_SEED_BYTES: usize = 512;
const MAX_EDGE_KINDS: usize = 32;
const MAX_EDGE_KIND_BYTES: usize = 128;
const MAX_CURSOR_BYTES: usize = 16 * 1024;

pub const DESCRIPTION: &str = "Assemble deterministic, bounded repository context from one or more \
     node, semantic-key, or repository-relative path seeds. Ranking favors exact evidence, \
     containment, declarations, and resolved dependencies; optional snippets are read only through \
     the hash-verified snapshot content boundary. Results explain why each fact was selected, label \
     freshness and resolution confidence, and support snapshot-bound continuation. Call \
     repository_graph_status first when availability is unknown. This read-only tool requires no \
     task lease and never changes task or run state. A missing relationship means only that it is \
     not known by this index, not that the relationship does not exist.";

pub const INPUT_SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "input": {
            "type": "object",
            "description": "Bounded repository context request",
            "properties": {
                "seeds": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 32,
                    "items": {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["node", "symbol", "path"] },
                            "value": { "type": "string", "minLength": 1, "maxLength": 512 }
                        },
                        "required": ["type", "value"],
                        "additionalProperties": false
                    }
                },
                "direction": {
                    "type": "string",
                    "enum": ["outgoing", "incoming", "both"],
                    "description": "Relationship traversal direction"
                },
                "edge_kinds": {
                    "type": "array",
                    "maxItems": 32,
                    "items": { "type": "string", "minLength": 1, "maxLength": 128 },
                    "description": "Optional exact relationship-kind filters"
                },
                "include_unresolved": {
                    "type": "boolean",
                    "description": "Include explicitly unresolved facts"
                },
                "include_external": {
                    "type": "boolean",
                    "description": "Include facts whose target is outside this repository"
                },
                "include_snippets": {
                    "type": "boolean",
                    "description": "Attach deduplicated hash-verified UTF-8 source excerpts"
                },
                "max_snippet_bytes": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Aggregate snippet byte cap; the configured server cap wins"
                },
                "max_results": { "type": "integer", "minimum": 1 },
                "max_bytes": { "type": "integer", "minimum": 1 },
                "max_depth": { "type": "integer", "minimum": 1 },
                "max_duration_ms": { "type": "integer", "minimum": 1 },
                "max_diagnostics": { "type": "integer", "minimum": 1 },
                "cursor": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 16384,
                    "description": "Opaque cursor from the same context policy and snapshot"
                }
            },
            "required": ["seeds"],
            "additionalProperties": false
        }
    },
    "required": ["input"],
    "additionalProperties": false
}"#;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryContextInput {
    seeds: Vec<RepositoryContextSeedInput>,
    #[serde(default)]
    direction: EdgeDirection,
    #[serde(default)]
    edge_kinds: Vec<String>,
    #[serde(default)]
    include_unresolved: bool,
    #[serde(default)]
    include_external: bool,
    #[serde(default)]
    include_snippets: bool,
    max_snippet_bytes: Option<u64>,
    max_results: Option<u32>,
    max_bytes: Option<u64>,
    max_depth: Option<u32>,
    max_duration_ms: Option<u64>,
    max_diagnostics: Option<u32>,
    cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum RepositoryContextSeedInput {
    Node(String),
    Symbol(String),
    Path(String),
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

async fn run(agent_id: Option<&str>, input: RepositoryContextInput) -> Result<String> {
    let started = Instant::now();
    let context = match agent_id {
        Some(agent_id) => LocalGraphContext::load_for_agent(false, agent_id).await?,
        None => LocalGraphContext::load(false).await?,
    };
    let include_snippets = input.include_snippets;
    let requested_snippet_bytes = input.max_snippet_bytes;
    let request = match context_request(&context, input) {
        Ok(request) => request,
        Err(error) => return serialize_invalid_request(&error),
    };
    let response = if include_snippets {
        let max_snippet_bytes = NonZeroU64::new(
            requested_snippet_bytes.unwrap_or(context.config.query_limits.max_snippet_bytes),
        )
        .context("repository_context max_snippet_bytes must be greater than zero")?;
        context
            .context_with_snippets(&request, max_snippet_bytes)
            .await?
    } else {
        context.context(&request).await?
    };
    let serialized = match &response {
        Ok(response) => serde_json::to_string(response)?,
        Err(error) => serde_json::to_string(error)?,
    };
    repository_query_telemetry::context(&context, started, &response, serialized.len());
    Ok(serialized)
}

fn parse_input(input: serde_json::Value) -> Result<RepositoryContextInput> {
    let input: RepositoryContextInput = serde_json::from_value(input)
        .context("repository_context expects an input object matching its schema")?;
    if input.seeds.is_empty() || input.seeds.len() > MAX_SEEDS {
        anyhow::bail!("repository_context requires 1..={MAX_SEEDS} seeds");
    }
    if input
        .seeds
        .iter()
        .any(|seed| seed_value(seed).trim().is_empty())
    {
        anyhow::bail!("repository_context seeds must not be empty");
    }
    if input
        .seeds
        .iter()
        .any(|seed| seed_value(seed).len() > MAX_SEED_BYTES)
    {
        anyhow::bail!("repository_context seeds exceed {MAX_SEED_BYTES} bytes");
    }
    if input.edge_kinds.len() > MAX_EDGE_KINDS {
        anyhow::bail!("repository_context edge_kinds exceeds {MAX_EDGE_KINDS} entries");
    }
    if input.edge_kinds.iter().any(|kind| kind.trim().is_empty()) {
        anyhow::bail!("repository_context edge_kinds must not contain empty entries");
    }
    if input
        .edge_kinds
        .iter()
        .any(|kind| kind.len() > MAX_EDGE_KIND_BYTES)
    {
        anyhow::bail!("repository_context edge_kinds exceed {MAX_EDGE_KIND_BYTES} bytes");
    }
    if input
        .cursor
        .as_ref()
        .is_some_and(|cursor| cursor.len() > MAX_CURSOR_BYTES)
    {
        anyhow::bail!("repository_context cursor exceeds {MAX_CURSOR_BYTES} bytes");
    }
    if input.max_snippet_bytes == Some(0) {
        anyhow::bail!("repository_context max_snippet_bytes must be greater than zero");
    }
    Ok(input)
}

fn seed_value(seed: &RepositoryContextSeedInput) -> &str {
    match seed {
        RepositoryContextSeedInput::Node(value)
        | RepositoryContextSeedInput::Symbol(value)
        | RepositoryContextSeedInput::Path(value) => value,
    }
}

fn context_request(
    context: &LocalGraphContext,
    input: RepositoryContextInput,
) -> Result<ContextRequest> {
    let budget = requested_budget(&context.config.query_limits, &input)?;
    let seeds = input
        .seeds
        .into_iter()
        .map(|seed| match seed {
            RepositoryContextSeedInput::Node(value) => NodeId::new(value.trim())
                .map(ContextSeed::Node)
                .map_err(Into::into),
            RepositoryContextSeedInput::Symbol(value) => SemanticKey::new(value.trim())
                .map(ContextSeed::Symbol)
                .map_err(Into::into),
            RepositoryContextSeedInput::Path(value) => RepoPath::new(value.trim())
                .map(ContextSeed::Path)
                .map_err(Into::into),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ContextRequest {
        scope: context.scope(budget)?,
        seeds,
        policy: ContextPolicy {
            direction: input.direction,
            edge_kinds: input
                .edge_kinds
                .into_iter()
                .map(|kind| kind.trim().to_string())
                .collect(),
            include_unresolved: input.include_unresolved,
            include_external: input.include_external,
        },
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
    input: &RepositoryContextInput,
) -> Result<QueryBudget> {
    Ok(QueryBudget::new(
        NonZeroU32::new(input.max_results.unwrap_or(limits.max_results))
            .context("repository_context max_results must be greater than zero")?,
        NonZeroU64::new(input.max_bytes.unwrap_or(limits.max_bytes))
            .context("repository_context max_bytes must be greater than zero")?,
        NonZeroU32::new(input.max_depth.unwrap_or(limits.max_depth))
            .context("repository_context max_depth must be greater than zero")?,
        NonZeroU64::new(input.max_duration_ms.unwrap_or(limits.max_duration_ms))
            .context("repository_context max_duration_ms must be greater than zero")?,
        NonZeroU32::new(input.max_diagnostics.unwrap_or(limits.max_diagnostics))
            .context("repository_context max_diagnostics must be greater than zero")?,
    ))
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

#[cfg(test)]
mod tests {
    //! Context schema validation, wrapped input, seeds, and budget preservation.

    use super::*;
    use neva::types::{ArgNames, CallToolRequestParams, FromHandlerArgs};
    use std::collections::HashMap;

    fn local_context() -> LocalGraphContext {
        let directory = tempfile::tempdir().unwrap().keep();
        LocalGraphContext {
            project_root: directory.clone(),
            root: directory,
            repository: crate::repository_graph::domain::RepositoryRef {
                namespace: crate::repository_graph::domain::RepositoryNamespace::new("local:test")
                    .unwrap(),
                repository_id: crate::repository_graph::domain::RepositoryId::new("root").unwrap(),
            },
            config: crate::repository_graph::config::RepositoryGraphConfig::default(),
            repository_view: None,
            task_view_id: None,
            run_id: None,
        }
    }

    #[test]
    fn schema_is_bounded_and_rejects_unknown_fields() {
        let schema: serde_json::Value = serde_json::from_str(INPUT_SCHEMA).unwrap();
        let input = &schema["properties"]["input"];
        assert_eq!(input["additionalProperties"], false);
        assert_eq!(input["properties"]["seeds"]["maxItems"], 32);
        assert_eq!(input["properties"]["edge_kinds"]["maxItems"], 32);
        assert!(
            parse_input(serde_json::json!({
                "seeds": [{"type": "path", "value": "src/lib.rs"}],
                "unknown": true
            }))
            .is_err()
        );
    }

    #[tokio::test]
    async fn boundary_validation_returns_a_versioned_query_error() {
        let response = handler_for_agent("executor:codex:1", serde_json::json!({"seeds": []}))
            .await
            .unwrap();
        let response: QueryError = serde_json::from_str(&response).unwrap();
        assert_eq!(response.wire_version, QUERY_WIRE_VERSION);
        assert_eq!(response.code, QueryErrorCode::InvalidRequest);
    }

    #[test]
    fn neva_extracts_wrapped_context_input_as_one_argument() {
        let params = CallToolRequestParams {
            name: "repository_context".to_string(),
            args: Some(HashMap::from([(
                "input".to_string(),
                serde_json::json!({
                    "seeds": [{"type": "symbol", "value": "crate::RuntimeTaskContext"}],
                    "include_snippets": true
                }),
            )])),
            meta: None,
        };
        let (input,): (serde_json::Value,) =
            FromHandlerArgs::from_args(params, &ArgNames::new(["input"])).unwrap();
        let input = parse_input(input).unwrap();
        assert!(input.include_snippets);
        assert_eq!(input.seeds.len(), 1);
    }

    #[test]
    fn request_preserves_typed_seeds_policy_cursor_and_budgets() {
        let context = local_context();
        let input = parse_input(serde_json::json!({
            "seeds": [
                {"type": "path", "value": "src/lib.rs"},
                {"type": "node", "value": "node:main"}
            ],
            "direction": "incoming",
            "edge_kinds": [" imports "],
            "include_external": true,
            "max_results": 7,
            "max_bytes": 4096,
            "max_depth": 2,
            "max_duration_ms": 50,
            "max_diagnostics": 3,
            "cursor": "opaque"
        }))
        .unwrap();
        let request = context_request(&context, input).unwrap();
        assert_eq!(request.seeds.len(), 2);
        assert_eq!(request.policy.direction, EdgeDirection::Incoming);
        assert_eq!(request.policy.edge_kinds, ["imports"]);
        assert!(request.policy.include_external);
        assert_eq!(request.scope.budget.max_results.get(), 7);
        assert_eq!(request.scope.budget.max_depth.get(), 2);
        assert_eq!(request.page.cursor.unwrap().as_str(), "opaque");
    }

    #[test]
    fn invalid_paths_oversized_collections_and_zero_budgets_are_rejected() {
        let context = local_context();
        let invalid_path = parse_input(serde_json::json!({
            "seeds": [{"type": "path", "value": "../secret"}]
        }))
        .unwrap();
        assert!(context_request(&context, invalid_path).is_err());

        let empty_seed = parse_input(serde_json::json!({
            "seeds": [{"type": "symbol", "value": " "}]
        }));
        assert!(empty_seed.is_err());

        let zero_budget = parse_input(serde_json::json!({
            "seeds": [{"type": "node", "value": "node:main"}],
            "max_depth": 0
        }))
        .unwrap();
        assert!(context_request(&context, zero_budget).is_err());
    }
}
