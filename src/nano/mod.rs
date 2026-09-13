//! Native agent core and managed Ferrus adapter; frontend and provider wiring are separate.

mod private;

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
pub(crate) mod workspace;

#[cfg(test)]
mod core_tests;
#[cfg(test)]
mod journal_tests;
