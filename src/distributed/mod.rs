//! Vendor-neutral contracts for the optional distributed context data plane.
//!
//! These contracts scope every remote operation by tenant and cloud project.
//! They reuse the repository-graph and project-memory semantic DTOs without
//! making local Ferrus depend on a network, queue, object store, or cloud SDK.

pub mod api;
pub mod api_sqlite;
pub mod coordinator;
pub mod coordinator_sqlite;
pub mod fact_store;
pub mod fact_store_sqlite;
pub mod identity;
pub mod object_store;
pub mod protocol;
pub mod publication;
pub mod publication_sqlite;
pub mod security;
pub mod source;
pub mod worker;

#[cfg(test)]
mod contracts;

/// Version of distributed control-plane request and response envelopes.
pub const DISTRIBUTED_CONTROL_PROTOCOL_VERSION: u32 = 1;
/// Version of immutable worker fact-batch envelopes.
pub const DISTRIBUTED_FACT_PROTOCOL_VERSION: u32 = 1;
/// Version of snapshot-pinned remote query envelopes.
pub const DISTRIBUTED_QUERY_PROTOCOL_VERSION: u32 = 1;
/// Version of the distributed authorization and retention policy contracts.
pub const DISTRIBUTED_POLICY_VERSION: u32 = 1;
/// Version of privacy-filtered repository and memory source manifests.
pub const DISTRIBUTED_SOURCE_MANIFEST_VERSION: u32 = 1;
/// Version of stateless worker execution requests and sandbox declarations.
pub const DISTRIBUTED_WORKER_PROTOCOL_VERSION: u32 = 1;
/// Version of immutable remote fact storage and publication pointers.
pub const DISTRIBUTED_STORAGE_PROTOCOL_VERSION: u32 = 1;
