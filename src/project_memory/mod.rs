//! Backend-neutral project memory and repository federation contracts.
//!
//! Project memory is a rebuildable, project-scoped materialized view of
//! authorized historical sources. It is independent from repository graph
//! snapshots and from orchestration state in `ferrus.db`. Implementations may
//! share a physical sidecar, but that choice must not leak into these APIs.

pub mod diagnostics;
mod documents;
pub mod domain;
pub mod extractors;
pub mod federation;
pub mod federation_service;
pub mod index;
pub mod links;
pub mod policy;
pub mod ports;
pub mod query;
pub mod query_sqlite;
pub mod source;
pub mod sqlite;

#[cfg(test)]
mod contracts;

/// Schema-independent project-memory model version.
pub const MEMORY_MODEL_VERSION: u32 = 1;
/// Version of deterministic memory extractor inputs and outputs.
pub const MEMORY_EXTRACTOR_CONTRACT_VERSION: u32 = 1;
/// Version of project-memory query request and response DTOs.
pub const MEMORY_QUERY_WIRE_VERSION: u32 = 1;
/// Version of federated repository and memory context DTOs.
pub const FEDERATION_WIRE_VERSION: u32 = 1;
