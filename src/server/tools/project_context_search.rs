use std::time::Instant;

use anyhow::{Context, Result};
use neva::prelude::*;
use serde::Deserialize;

use crate::{
    project_memory::{
        FEDERATION_WIRE_VERSION,
        domain::{
            FederationPageCursor, MemoryEntityKind, MemoryQueryText, MemorySourceCategory,
            MemoryStatusToken,
        },
        federation::{ContextDomain, FederatedSearchRequest},
    },
    project_memory_runtime::LocalProjectContext,
    repository_graph::domain::RepoPath,
};

use super::{project_context_telemetry, tool_err};

pub const DESCRIPTION: &str = "Search repository structure, curated project memory, or both through one bounded read-only surface. The domain is mandatory and is never broadened implicitly. Results preserve independent repository snapshot and memory revision freshness and provenance. This tool never builds indexes or writes curated memory.";

pub const INPUT_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "input": {
      "type": "object",
      "properties": {
        "domain": { "type": "string", "enum": ["repository", "memory", "all"] },
        "query": { "type": "string", "minLength": 1, "maxLength": 4096 },
        "repository_kinds": { "type": "array", "maxItems": 32, "items": { "type": "string", "minLength": 1, "maxLength": 128 } },
        "repository_paths": { "type": "array", "maxItems": 32, "items": { "type": "string", "minLength": 1, "maxLength": 512 } },
        "memory_kinds": { "type": "array", "maxItems": 32, "items": { "type": "string", "enum": ["specification", "milestone", "outcome", "decision", "deviation", "validation_evidence", "follow_up_work", "task_reference", "run_reference", "archive_reference"] } },
        "memory_sources": { "type": "array", "maxItems": 13, "items": { "type": "string", "enum": ["specification_structure", "approved_outcome", "archive_manifest", "runtime_provenance"] } },
        "max_results": { "type": "integer", "minimum": 1 },
        "max_bytes": { "type": "integer", "minimum": 1 },
        "max_duration_ms": { "type": "integer", "minimum": 1 },
        "max_diagnostics": { "type": "integer", "minimum": 1 },
        "cursor": { "type": "string", "minLength": 1, "maxLength": 16384 }
      },
      "required": ["domain", "query"],
      "additionalProperties": false
    }
  },
  "required": ["input"],
  "additionalProperties": false
}"#;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    domain: ContextDomain,
    query: String,
    #[serde(default)]
    repository_kinds: Vec<MemoryStatusToken>,
    #[serde(default)]
    repository_paths: Vec<String>,
    #[serde(default)]
    memory_kinds: Vec<MemoryEntityKind>,
    #[serde(default)]
    memory_sources: Vec<MemorySourceCategory>,
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
    run(ctx.agent_id(), input).await.map_err(tool_err)
}

async fn run(agent_id: &str, input: serde_json::Value) -> Result<String> {
    let started = Instant::now();
    let input: Input = serde_json::from_value(input)
        .context("project_context_search expects an input object matching its schema")?;
    let context = LocalProjectContext::load_for_agent(agent_id, false).await?;
    let budget = context.requested_budget(
        input.max_results,
        input.max_bytes,
        None,
        None,
        input.max_duration_ms,
        input.max_diagnostics,
    )?;
    let request = FederatedSearchRequest {
        scope: context.scope(input.domain, budget)?,
        text: MemoryQueryText::new(input.query.trim())?,
        repository_kinds: input.repository_kinds,
        repository_paths: input
            .repository_paths
            .into_iter()
            .map(RepoPath::new)
            .collect::<Result<Vec<_>, _>>()?,
        memory_kinds: input.memory_kinds,
        memory_sources: input.memory_sources,
        cursor: input.cursor.map(FederationPageCursor::new).transpose()?,
    };
    match context.search(request) {
        Ok(response) => {
            let serialized = serde_json::to_string(&response)?;
            project_context_telemetry::search(started, &response, serialized.len(), None);
            Ok(serialized)
        }
        Err(error) => {
            let serialized = serde_json::json!({
                "wire_version": FEDERATION_WIRE_VERSION,
                "error": error.to_string()
            })
            .to_string();
            project_context_telemetry::search_error(started, &error, serialized.len());
            Ok(serialized)
        }
    }
}
