//! Vendor-neutral contracts for the optional distributed context data plane.
//!
//! These contracts scope every remote operation by tenant and cloud project.
//! They reuse the repository-graph and project-memory semantic DTOs without
//! making local Ferrus depend on a network, queue, object store, or cloud SDK.

pub mod identity;
pub mod protocol;
pub mod security;

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
