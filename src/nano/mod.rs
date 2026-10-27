//! Native agent core and managed Ferrus adapter; frontend and provider wiring are separate.

pub(crate) mod agent;
mod binding;
mod checks;
pub(crate) mod cli;
pub(crate) mod coding;
pub(crate) mod commands;
pub(crate) mod compaction;
#[cfg(feature = "nano-openai")]
pub(crate) mod config;
pub(crate) mod context;
mod context_request;
pub(crate) mod conversation;
mod descriptors;
mod effect_recovery;
pub(crate) mod engine;
mod eval;
#[cfg(feature = "nano-openai")]
mod frontend;
pub(crate) mod instructions;
#[cfg(feature = "nano-openai")]
mod interactive;
mod lifecycle;
pub(crate) mod managed;
#[cfg(feature = "nano-mcp")]
pub(crate) mod mcp;
pub(crate) mod native;
mod private;
mod refresh;
mod resume;
pub(crate) mod review;
#[cfg(feature = "nano-openai")]
mod supervisor;
pub(crate) mod wire;
pub(crate) use eval::capture_launch as capture_evaluation_launch;

#[cfg(test)]
mod core_tests;
pub(crate) mod ferrus;
pub(crate) mod journal;
#[cfg(test)]
mod journal_tests;
pub(crate) mod provider;
#[cfg(feature = "nano-openai")]
pub(crate) mod providers;
pub(crate) mod replay;
pub(crate) mod session;
pub(crate) mod tools;
pub(crate) mod working_set;
pub(crate) mod workspace;
