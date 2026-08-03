use std::time::Instant;

use anyhow::{Context, Result};
use neva::prelude::*;
use serde::Deserialize;

use crate::{
    project_memory::{
        FEDERATION_WIRE_VERSION,
        domain::{FederationPageCursor, MemoryEntityId, MemoryRecordId, MemoryRelationshipKind},
        federation::{ContextDomain, FederatedContextRequest, FederatedContextSeed},
        query::MemoryContextPolicy,
    },
    project_memory_runtime::LocalProjectContext,
    repository_graph::{
        domain::{NodeId, RepoPath, SemanticKey},
        query::{ContextPolicy, ContextSeed, EdgeDirection},
    },
};

use super::{project_context_telemetry, tool_err};

pub const DESCRIPTION: &str = "Assemble bounded deterministic context from repository structure, curated project memory, or both. The mandatory domain prevents accidental broadening. Combined expansion crosses domains only through evidence-backed links for the exact repository snapshot and memory revision. Optional memory snippets are fingerprint-verified. This read-only tool never builds indexes or authors memory.";

pub const INPUT_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "input": {
      "type": "object",
      "properties": {
        "domain": { "type": "string", "enum": ["repository", "memory", "all"] },
        "seeds": {
          "type": "array", "minItems": 1, "maxItems": 32,
          "items": {
            "type": "object",
            "properties": {
              "type": { "type": "string", "enum": ["node", "symbol", "path", "memory_entity", "milestone", "task", "run"] },
              "value": { "type": "string", "minLength": 1, "maxLength": 512 }
            },
            "required": ["type", "value"], "additionalProperties": false
          }
        },
        "direction": { "type": "string", "enum": ["outgoing", "incoming", "both"] },
        "repository_edge_kinds": { "type": "array", "maxItems": 32, "items": { "type": "string", "minLength": 1, "maxLength": 128 } },
        "memory_relationship_kinds": { "type": "array", "maxItems": 32, "items": { "type": "string", "enum": ["contains", "implements", "validates", "supersedes", "concerns", "touches", "follows_up"] } },
        "include_unresolved": { "type": "boolean" },
        "include_stale": { "type": "boolean" },
        "include_external": { "type": "boolean" },
        "include_snippets": { "type": "boolean" },
        "max_results": { "type": "integer", "minimum": 1 },
        "max_bytes": { "type": "integer", "minimum": 1 },
        "max_snippet_bytes": { "type": "integer", "minimum": 1 },
        "max_depth": { "type": "integer", "minimum": 1 },
        "max_duration_ms": { "type": "integer", "minimum": 1 },
        "max_diagnostics": { "type": "integer", "minimum": 1 },
        "cursor": { "type": "string", "minLength": 1, "maxLength": 16384 }
      },
      "required": ["domain", "seeds"],
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
    seeds: Vec<SeedInput>,
    #[serde(default)]
    direction: EdgeDirection,
    #[serde(default)]
    repository_edge_kinds: Vec<String>,
    #[serde(default)]
    memory_relationship_kinds: Vec<MemoryRelationshipKind>,
    #[serde(default)]
    include_unresolved: bool,
    #[serde(default)]
    include_stale: bool,
    #[serde(default)]
    include_external: bool,
    #[serde(default)]
    include_snippets: bool,
    max_results: Option<u32>,
    max_bytes: Option<u64>,
    max_snippet_bytes: Option<u64>,
    max_depth: Option<u32>,
    max_duration_ms: Option<u64>,
    max_diagnostics: Option<u32>,
    cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum SeedInput {
    Node(String),
    Symbol(String),
    Path(String),
    MemoryEntity(String),
    Milestone(String),
    Task(String),
    Run(String),
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
        .context("project_context expects an input object matching its schema")?;
    if input.seeds.is_empty() || input.seeds.len() > 32 {
        anyhow::bail!("project_context requires 1..=32 seeds");
    }
    let context = LocalProjectContext::load_for_agent(agent_id, input.include_snippets).await?;
    let budget = context.requested_budget(
        input.max_results,
        input.max_bytes,
        input.max_snippet_bytes,
        input.max_depth,
        input.max_duration_ms,
        input.max_diagnostics,
    )?;
    let seeds = input
        .seeds
        .into_iter()
        .map(seed)
        .collect::<Result<Vec<_>>>()?;
    validate_seed_domains(input.domain, &seeds)?;
    let request = FederatedContextRequest {
        scope: context.scope(input.domain, budget)?,
        seeds,
        repository_policy: ContextPolicy {
            direction: input.direction,
            edge_kinds: input.repository_edge_kinds,
            include_unresolved: input.include_unresolved,
            include_external: input.include_external,
        },
        memory_policy: MemoryContextPolicy {
            relationship_kinds: input.memory_relationship_kinds,
            include_unresolved: input.include_unresolved,
            include_stale: input.include_stale,
            include_snippets: input.include_snippets,
        },
        cursor: input.cursor.map(FederationPageCursor::new).transpose()?,
    };
    match context.context(request) {
        Ok(response) => {
            let serialized = serde_json::to_string(&response)?;
            project_context_telemetry::context(started, &response, serialized.len(), None);
            Ok(serialized)
        }
        Err(error) => {
            let serialized = serde_json::json!({
                "wire_version": FEDERATION_WIRE_VERSION,
                "error": error.to_string()
            })
            .to_string();
            project_context_telemetry::context_error(started, &error, serialized.len());
            Ok(serialized)
        }
    }
}

fn seed(seed: SeedInput) -> Result<FederatedContextSeed> {
    Ok(match seed {
        SeedInput::Node(value) => {
            FederatedContextSeed::Repository(ContextSeed::Node(NodeId::new(value)?))
        }
        SeedInput::Symbol(value) => {
            FederatedContextSeed::Repository(ContextSeed::Symbol(SemanticKey::new(value)?))
        }
        SeedInput::Path(value) => {
            FederatedContextSeed::Repository(ContextSeed::Path(RepoPath::new(value)?))
        }
        SeedInput::MemoryEntity(value) => {
            FederatedContextSeed::MemoryEntity(MemoryEntityId::new(value)?)
        }
        SeedInput::Milestone(value) => FederatedContextSeed::Milestone(MemoryRecordId::new(value)?),
        SeedInput::Task(value) => FederatedContextSeed::Task(MemoryRecordId::new(value)?),
        SeedInput::Run(value) => FederatedContextSeed::Run(MemoryRecordId::new(value)?),
    })
}

fn validate_seed_domains(domain: ContextDomain, seeds: &[FederatedContextSeed]) -> Result<()> {
    let has_repository = seeds
        .iter()
        .any(|seed| matches!(seed, FederatedContextSeed::Repository(_)));
    let has_memory = seeds
        .iter()
        .any(|seed| !matches!(seed, FederatedContextSeed::Repository(_)));
    if matches!(domain, ContextDomain::Repository) && has_memory {
        anyhow::bail!("project_context repository domain accepts only node, symbol, or path seeds");
    }
    if matches!(domain, ContextDomain::Memory) && has_repository {
        anyhow::bail!(
            "project_context memory domain accepts only memory entity, milestone, task, or run seeds"
        );
    }
    Ok(())
}
