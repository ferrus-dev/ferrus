//! Reusable Ferrus components.

pub mod distributed;
mod json_size;
pub mod project_memory;
pub mod repository_graph;

#[cfg(all(feature = "nano-openai", feature = "nano-mcp"))]
#[allow(dead_code)]
mod agent_id;
/// Standalone Nano composition. Managed orchestration stays in the Ferrus binary.
#[cfg(feature = "nano-openai")]
#[path = "nano/library.rs"]
#[allow(dead_code)]
pub mod nano;
#[cfg(feature = "nano-openai")]
#[allow(dead_code)]
mod platform;

#[cfg(feature = "nano-openai")]
mod user_paths;
