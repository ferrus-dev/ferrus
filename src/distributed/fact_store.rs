//! Vendor-neutral persistence boundary for unpublished worker fact batches.

use serde::{Deserialize, Serialize};

use super::{
    identity::{FactBatchId, FactShardId},
    protocol::{FactBatch, FactBatchHeader, IndexJobRef},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FactStoreProtection {
    pub authenticated_transport: bool,
    pub encrypted_at_rest: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutFactBatchOutcome {
    Stored,
    Reused,
}

/// Privacy-safe durable retry progress. Payload facts are deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredFactBatchRef {
    pub batch_id: FactBatchId,
    pub shard_id: FactShardId,
    pub sequence: u32,
    pub final_batch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactBatchProgress {
    pub job: IndexJobRef,
    pub batches: Vec<StoredFactBatchRef>,
    pub final_batch_seen: bool,
}

/// Unpublished batch persistence used by workers and the later ingestion phase.
/// This is not a query interface and must never be wired to ordinary retrieval.
pub trait FactBatchStore {
    type Error;

    fn protection(&self) -> FactStoreProtection;
    fn put(&mut self, batch: &FactBatch) -> Result<PutFactBatchOutcome, Self::Error>;
    fn progress(&self, job: &IndexJobRef) -> Result<FactBatchProgress, Self::Error>;
    fn load_for_ingestion(&self, job: &IndexJobRef) -> Result<Vec<FactBatch>, Self::Error>;
}

pub(crate) fn progress_from_headers(
    job: IndexJobRef,
    headers: impl IntoIterator<Item = FactBatchHeader>,
) -> FactBatchProgress {
    let batches = headers
        .into_iter()
        .map(|header| StoredFactBatchRef {
            batch_id: header.batch_id,
            shard_id: header.shard_id,
            sequence: header.sequence,
            final_batch: header.final_batch,
        })
        .collect::<Vec<_>>();
    let final_batch_seen = batches.iter().any(|batch| batch.final_batch);
    FactBatchProgress {
        job,
        batches,
        final_batch_seen,
    }
}
