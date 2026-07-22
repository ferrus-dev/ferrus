use anyhow::Result;
use neva::prelude::*;
use std::time::Instant;

use crate::repository_graph_runtime::LocalGraphContext;

use super::{repository_query_telemetry, tool_err};

pub const DESCRIPTION: &str = "Read repository graph status for the caller's routed canonical or \
     task view without claiming a task lease or changing Ferrus runtime state. Reports distinct \
     not-built, building, failed, incompatible, stale, and fresh states with an operator action \
     when one is needed. Call this before repository_search when graph availability is unknown.";

pub async fn handler(ctx: neva::di::Dc<crate::server::ServerContext>) -> Result<String, Error> {
    handler_for_agent(ctx.agent_id()).await
}

pub async fn handler_for_agent(agent_id: &str) -> Result<String, Error> {
    run_for_agent(Some(agent_id)).await.map_err(tool_err)
}

#[cfg(test)]
pub(super) async fn run() -> Result<String> {
    run_for_agent(None).await
}

async fn run_for_agent(agent_id: Option<&str>) -> Result<String> {
    let started = Instant::now();
    let context = match agent_id {
        Some(agent_id) => LocalGraphContext::load_for_agent(false, agent_id).await?,
        None => LocalGraphContext::load(false).await?,
    };
    let response = context.status().await?;
    let serialized = serde_json::to_string(&response)?;
    repository_query_telemetry::status(&context, started, &response, serialized.len());
    Ok(serialized)
}
