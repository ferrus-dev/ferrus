use std::time::Instant;

use anyhow::Result;
use neva::prelude::*;

use crate::project_memory_runtime::LocalProjectContext;

use super::{project_context_telemetry, tool_err};

pub const DESCRIPTION: &str = "Read the independent local project-memory status without building the index or changing task, run, review, or archive state. Reports availability, revision identity, conservative freshness, authorized source categories, sensitivity, diagnostics, and bounded fact counts. Use project_context_search only after confirming memory is available.";

pub async fn handler(ctx: neva::di::Dc<crate::server::ServerContext>) -> Result<String, Error> {
    run(ctx.agent_id()).await.map_err(tool_err)
}

async fn run(agent_id: &str) -> Result<String> {
    let started = Instant::now();
    let context = LocalProjectContext::load_for_agent(agent_id, false, false).await?;
    let response = context.memory_status(context.default_budget()?)?;
    let serialized = serde_json::to_string(&response)?;
    project_context_telemetry::memory_status(started, &response, serialized.len());
    Ok(serialized)
}

#[cfg(test)]
pub(super) async fn run_without_agent() -> Result<String> {
    let context = LocalProjectContext::load_unscoped_read_only().await?;
    Ok(serde_json::to_string(
        &context.memory_status(context.default_budget()?)?,
    )?)
}
