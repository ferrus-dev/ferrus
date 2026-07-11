//! Backend-neutral repository graph contracts and local derived storage.
//!
//! This bounded context deliberately does not depend on orchestration task
//! state. `ferrus.db` remains the runtime source of truth; the SQLite module
//! here owns only the deletable `repo-graph.db` sidecar.

pub mod config;
pub mod domain;
pub mod ports;
pub mod query;
pub mod sqlite;
pub mod store;

pub use config::RepositoryGraphConfig;

/// Schema-independent graph model version.
pub const GRAPH_MODEL_VERSION: u32 = 1;
/// Version of request/response JSON contracts introduced by this phase.
pub const QUERY_WIRE_VERSION: u32 = 1;
/// Version of the extractor input/output contract.
pub const EXTRACTOR_CONTRACT_VERSION: u32 = 1;
