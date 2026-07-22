use anyhow::Result;
use neva::prelude::*;
use std::time::Instant;

use crate::repository_graph_runtime::LocalGraphContext;

use super::{repository_query_telemetry, tool_err};

pub const DESCRIPTION: &str = "Read the canonical repository graph status without claiming a task \
     lease or changing Ferrus runtime state. Reports distinct not-built, building, failed, \
     incompatible, stale, and fresh states with an operator action when one is needed. Call this \
     before repository_search when graph availability is unknown.";

pub async fn handler() -> Result<String, Error> {
    run().await.map_err(tool_err)
}

pub(super) async fn run() -> Result<String> {
    let started = Instant::now();
    let context = LocalGraphContext::load(false).await?;
    let response = context.status().await?;
    let serialized = serde_json::to_string(&response)?;
    repository_query_telemetry::status(&context, started, &response, serialized.len());
    Ok(serialized)
}
