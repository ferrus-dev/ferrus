//! Vendor-neutral durable index-job coordination port.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{num::NonZeroU64, time::Instant};

use super::{
    DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
    identity::{IndexJobFailureCode, RemoteProjectRef, RequestId, WorkerId},
    protocol::{
        CancelIndexJobRequest, HeartbeatJobRequest, IndexJobKind, IndexJobRecord, IndexJobRef,
        IndexJobState, InspectIndexJobRequest, SubmitIndexJobRequest,
    },
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimIndexJobRequest {
    pub protocol_version: u32,
    pub request_id: RequestId,
    pub project: RemoteProjectRef,
    pub kind: IndexJobKind,
    pub worker_id: WorkerId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdvanceIndexJobRequest {
    pub protocol_version: u32,
    pub request_id: RequestId,
    pub job: IndexJobRef,
    pub worker_id: WorkerId,
    pub lease_generation: NonZeroU64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailIndexJobRequest {
    pub protocol_version: u32,
    pub request_id: RequestId,
    pub job: IndexJobRef,
    pub worker_id: WorkerId,
    pub lease_generation: NonZeroU64,
    pub failure_code: IndexJobFailureCode,
    pub retryable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReclaimIndexJobsRequest {
    pub protocol_version: u32,
    pub request_id: RequestId,
    pub project: RemoteProjectRef,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReclaimIndexJobsResult {
    pub requeued: u64,
    pub failed: u64,
    pub cancelled: u64,
}

pub trait IndexJobCoordinator {
    type Error;

    fn submit(
        &mut self,
        request: &SubmitIndexJobRequest,
        now: DateTime<Utc>,
    ) -> Result<IndexJobRecord, Self::Error>;
    fn inspect(
        &self,
        request: &InspectIndexJobRequest,
    ) -> Result<Option<IndexJobRecord>, Self::Error>;
    fn inspect_bounded(
        &self,
        request: &InspectIndexJobRequest,
        deadline: Instant,
    ) -> Result<Option<IndexJobRecord>, Self::Error>;
    fn claim(
        &mut self,
        request: &ClaimIndexJobRequest,
        now: DateTime<Utc>,
    ) -> Result<Option<IndexJobRecord>, Self::Error>;
    fn start(
        &mut self,
        request: &AdvanceIndexJobRequest,
        now: DateTime<Utc>,
    ) -> Result<IndexJobRecord, Self::Error>;
    fn heartbeat(
        &mut self,
        request: &HeartbeatJobRequest,
        now: DateTime<Utc>,
    ) -> Result<IndexJobRecord, Self::Error>;
    fn begin_publication(
        &mut self,
        request: &AdvanceIndexJobRequest,
        now: DateTime<Utc>,
    ) -> Result<IndexJobRecord, Self::Error>;
    fn complete(
        &mut self,
        request: &AdvanceIndexJobRequest,
        now: DateTime<Utc>,
    ) -> Result<IndexJobRecord, Self::Error>;
    fn fail(
        &mut self,
        request: &FailIndexJobRequest,
        now: DateTime<Utc>,
    ) -> Result<IndexJobRecord, Self::Error>;
    fn cancel(
        &mut self,
        request: &CancelIndexJobRequest,
        now: DateTime<Utc>,
    ) -> Result<IndexJobRecord, Self::Error>;
    fn reclaim(
        &mut self,
        request: &ReclaimIndexJobsRequest,
        now: DateTime<Utc>,
    ) -> Result<ReclaimIndexJobsResult, Self::Error>;
}

pub(crate) fn validate_version(version: u32) -> bool {
    version == DISTRIBUTED_CONTROL_PROTOCOL_VERSION
}

pub(crate) fn state_token(state: IndexJobState) -> &'static str {
    match state {
        IndexJobState::Queued => "queued",
        IndexJobState::Leased => "leased",
        IndexJobState::Running => "running",
        IndexJobState::Publishing => "publishing",
        IndexJobState::Complete => "complete",
        IndexJobState::Failed => "failed",
        IndexJobState::Cancelled => "cancelled",
    }
}
