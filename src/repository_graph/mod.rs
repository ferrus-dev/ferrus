//! Backend-neutral repository graph contracts and local derived storage.
//!
//! This bounded context deliberately does not depend on orchestration task
//! state. `ferrus.db` remains the runtime source of truth; the SQLite module
//! here owns only the deletable `repo-graph.db` sidecar.

pub mod config;
pub mod diagnostics;
mod diagnostics_sqlite;
pub mod domain;
pub mod extractors;
pub mod health;
pub mod index;
mod index_store;
pub mod ports;
pub mod query;
pub mod resolution;
pub mod source;
pub mod sqlite;
pub mod store;

#[cfg(test)]
mod contracts;

/// Schema-independent graph model version.
pub const GRAPH_MODEL_VERSION: u32 = 1;
/// Version of request/response JSON contracts introduced by this phase.
pub const QUERY_WIRE_VERSION: u32 = 1;
/// Version of the extractor input/output contract.
pub const EXTRACTOR_CONTRACT_VERSION: u32 = 2;
