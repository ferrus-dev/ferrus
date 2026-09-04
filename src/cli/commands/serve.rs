//! Launch the stdio MCP server with the requested agent identity and role.

use anyhow::Result;

use crate::server::Role;

pub async fn run(
    role: Option<Role>,
    agent_name: String,
    agent_index: u32,
    _debug: bool,
) -> Result<()> {
    crate::server::start(role, agent_name, agent_index).await
}
