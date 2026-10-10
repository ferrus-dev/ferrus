//! Shared runtime sources with a standalone composition, without HQ or project state.

mod coding;
mod commands;
mod compaction;
mod config;
mod context_request;
mod descriptors;
mod effect_recovery;
mod engine;
mod instructions;
mod journal;
#[cfg(feature = "nano-mcp")]
mod mcp;
mod private;
mod provider;
mod providers;
mod replay;
mod session;
mod tools;
mod wire;
mod working_set;
mod workspace;

pub mod standalone;
