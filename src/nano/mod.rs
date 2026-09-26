//! Native agent core and managed Ferrus adapter; frontend and provider wiring are separate.

pub(crate) mod agent;
mod checks;
pub(crate) mod cli;
pub(crate) mod coding;
pub(crate) mod commands;
pub(crate) mod compaction;
pub(crate) mod context;
pub(crate) mod instructions;
mod lifecycle;
pub(crate) mod managed;
#[cfg(feature = "nano-mcp")]
pub(crate) mod mcp;
pub(crate) mod native;
mod private;
mod refresh;
pub(crate) mod wire;

#[cfg(feature = "nano-openai")]
pub(crate) mod config;
pub(crate) mod engine;
pub(crate) mod ferrus;
pub(crate) mod journal;
pub(crate) mod provider;
#[cfg(feature = "nano-openai")]
pub(crate) mod providers;
pub(crate) mod replay;
pub(crate) mod session;
pub(crate) mod tools;
pub(crate) mod working_set;
pub(crate) mod workspace;

#[cfg(test)]
mod core_tests;
#[cfg(test)]
mod journal_tests;
