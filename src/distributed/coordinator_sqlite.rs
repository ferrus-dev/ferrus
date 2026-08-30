//! Durable SQLite prototype for at-least-once distributed index jobs.

use std::{
    num::{NonZeroU32, NonZeroU64},
    path::Path,
    time::Instant,
};

use chrono::{DateTime, Duration, Utc};
use rusqlite::{
    Connection, Error as SqliteError, ErrorCode, OptionalExtension, Row, Transaction,
    TransactionBehavior, params,
};
use thiserror::Error;

use super::{
    coordinator::{
        AdvanceIndexJobRequest, ClaimIndexJobRequest, FailIndexJobRequest, IndexJobCoordinator,
        ReclaimIndexJobsRequest, ReclaimIndexJobsResult, state_token, validate_version,
    },
    identity::{IndexJobFailureCode, IndexJobId, WorkerId},
    protocol::{
        CancelIndexJobRequest, HeartbeatJobRequest, IndexJobKind, IndexJobRecord, IndexJobRef,
        IndexJobSpec, IndexJobState, InspectIndexJobRequest, JobLease, SubmitIndexJobRequest,
    },
};

pub(crate) const COORDINATOR_SCHEMA_VERSION: u32 = 1;
const SQLITE_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const SQLITE_PROGRESS_OPS: i32 = 100;

struct ReadDeadline<'connection> {
    connection: &'connection Connection,
}

impl<'connection> ReadDeadline<'connection> {
    fn install(
        connection: &'connection Connection,
        deadline: Instant,
    ) -> Result<Self, CoordinatorError> {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(CoordinatorError::ReadBudgetExceeded)?;
        connection.busy_timeout(remaining)?;
        connection
            .progress_handler(
                SQLITE_PROGRESS_OPS,
                Some(move || Instant::now() >= deadline),
            )
            .map_err(|error| {
                let _ = connection.busy_timeout(SQLITE_BUSY_TIMEOUT);
                CoordinatorError::Database(error)
            })?;
        Ok(Self { connection })
    }
}

impl Drop for ReadDeadline<'_> {
    fn drop(&mut self) {
        let _ = self.connection.progress_handler(0, None::<fn() -> bool>);
        let _ = self.connection.busy_timeout(SQLITE_BUSY_TIMEOUT);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoordinatorLimits {
    pub max_attempts: NonZeroU32,
    pub lease_ttl_ms: NonZeroU64,
    pub max_job_duration_ms: NonZeroU64,
}

#[derive(Debug, Error)]
pub enum CoordinatorError {
    #[error("distributed coordinator request is invalid or incompatible")]
    InvalidRequest,
    #[error("distributed index job was not found in the authorized scope")]
    NotFound,
    #[error("distributed index job state changed concurrently")]
    Conflict,
    #[error("distributed index job lease is missing, expired, or owned by another worker")]
    LeaseLost,
    #[error("distributed index job was cancelled")]
    Cancelled,
    #[error("distributed project has a durable deletion tombstone")]
    ProjectDeleted,
    #[error("distributed coordinator schema is incompatible")]
    IncompatibleSchema,
    #[error("distributed coordinator read exceeded its duration budget")]
    ReadBudgetExceeded,
    #[error("distributed coordinator database operation failed")]
    Database(#[source] rusqlite::Error),
    #[error("distributed coordinator record serialization failed")]
    Serialization,
}

impl From<rusqlite::Error> for CoordinatorError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

pub struct SqliteIndexJobCoordinator {
    connection: Connection,
    limits: CoordinatorLimits,
}

impl SqliteIndexJobCoordinator {
    pub fn open(
        path: impl AsRef<Path>,
        limits: CoordinatorLimits,
    ) -> Result<Self, CoordinatorError> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
        initialize_schema(&connection)?;
        Ok(Self { connection, limits })
    }

    fn lease_expiry(&self, now: DateTime<Utc>) -> Result<DateTime<Utc>, CoordinatorError> {
        let millis = i64::try_from(self.limits.lease_ttl_ms.get())
            .map_err(|_| CoordinatorError::InvalidRequest)?;
        now.checked_add_signed(Duration::milliseconds(millis))
            .ok_or(CoordinatorError::InvalidRequest)
    }

    fn job_deadline(&self, now: DateTime<Utc>) -> Result<DateTime<Utc>, CoordinatorError> {
        let millis = i64::try_from(self.limits.max_job_duration_ms.get())
            .map_err(|_| CoordinatorError::InvalidRequest)?;
        now.checked_add_signed(Duration::milliseconds(millis))
            .ok_or(CoordinatorError::InvalidRequest)
    }

    pub fn inspect_bounded(
        &self,
        request: &InspectIndexJobRequest,
        deadline: Instant,
    ) -> Result<Option<IndexJobRecord>, CoordinatorError> {
        let read_deadline = ReadDeadline::install(&self.connection, deadline)?;
        let result = IndexJobCoordinator::inspect(self, request);
        drop(read_deadline);
        if Instant::now() >= deadline
            || matches!(
                &result,
                Err(CoordinatorError::Database(SqliteError::SqliteFailure(
                    failure,
                    _
                ))) if matches!(failure.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
            )
        {
            Err(CoordinatorError::ReadBudgetExceeded)
        } else {
            result
        }
    }

    #[cfg(test)]
    pub(crate) fn use_delete_journal_for_test(&self) -> Result<(), CoordinatorError> {
        self.connection
            .query_row("PRAGMA journal_mode = DELETE", [], |row| {
                row.get::<_, String>(0)
            })?;
        Ok(())
    }

    fn transition_with_lease(
        &mut self,
        request: &AdvanceIndexJobRequest,
        expected: IndexJobState,
        next: IndexJobState,
        now: DateTime<Utc>,
        clear_lease: bool,
    ) -> Result<IndexJobRecord, CoordinatorError> {
        if !validate_version(request.protocol_version) || !expected.can_transition_to(next) {
            return Err(CoordinatorError::InvalidRequest);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record = load_record(&transaction, &request.job)?.ok_or(CoordinatorError::NotFound)?;
        require_live_lease(&record, &request.worker_id, request.lease_generation, now)?;
        if record.cancellation_requested || record.state == IndexJobState::Cancelled {
            return Err(CoordinatorError::Cancelled);
        }
        if record.state != expected {
            return Err(CoordinatorError::Conflict);
        }
        let (worker, generation, expires) = if clear_lease {
            (None, None, None)
        } else {
            let lease = record.lease.as_ref().ok_or(CoordinatorError::LeaseLost)?;
            (
                Some(lease.worker_id.as_str()),
                Some(i64_from_u64(lease.generation.get())?),
                Some(lease.expires_at.timestamp_millis()),
            )
        };
        let changed = transaction.execute(
            "UPDATE distributed_index_jobs
             SET state = ?1, lease_worker_id = ?2, lease_generation = COALESCE(?3, lease_generation),
                 lease_until_ms = ?4, updated_at_ms = ?5
             WHERE tenant_id = ?6 AND project_id = ?7 AND job_id = ?8 AND state = ?9",
            params![
                state_token(next),
                worker,
                generation,
                expires,
                now.timestamp_millis(),
                request.job.project.tenant_id.as_str(),
                request.job.project.project_id.as_str(),
                request.job.job_id.as_str(),
                state_token(expected),
            ],
        )?;
        if changed != 1 {
            return Err(CoordinatorError::Conflict);
        }
        let updated = load_record(&transaction, &request.job)?.ok_or(CoordinatorError::NotFound)?;
        transaction.commit()?;
        Ok(updated)
    }
}

impl IndexJobCoordinator for SqliteIndexJobCoordinator {
    type Error = CoordinatorError;

    fn inspect_bounded(
        &self,
        request: &InspectIndexJobRequest,
        deadline: Instant,
    ) -> Result<super::coordinator::BoundedJobInspection, Self::Error> {
        match SqliteIndexJobCoordinator::inspect_bounded(self, request, deadline) {
            Ok(Some(record)) => Ok(super::coordinator::BoundedJobInspection::Found(Box::new(
                record,
            ))),
            Ok(None) => Ok(super::coordinator::BoundedJobInspection::NotFound),
            Err(CoordinatorError::ReadBudgetExceeded) => {
                Ok(super::coordinator::BoundedJobInspection::DeadlineExceeded)
            }
            Err(error) => Err(error),
        }
    }

    fn submit(
        &mut self,
        request: &SubmitIndexJobRequest,
        now: DateTime<Utc>,
    ) -> Result<IndexJobRecord, Self::Error> {
        request
            .validate()
            .map_err(|_| CoordinatorError::InvalidRequest)?;
        let job_id = IndexJobId::new(request.job.idempotency_key.value())
            .map_err(|_| CoordinatorError::InvalidRequest)?;
        let job = IndexJobRef {
            project: request.project.clone(),
            job_id,
            kind: request.job.kind,
        };
        let deadline_at = self.job_deadline(now)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if project_has_deletion_tombstone(&transaction, &request.project)? {
            return Err(CoordinatorError::ProjectDeleted);
        }
        if let Some(existing) = load_record(&transaction, &job)? {
            if existing.spec != request.job {
                return Err(CoordinatorError::Conflict);
            }
            transaction.commit()?;
            return Ok(existing);
        }
        let spec = serde_json::to_vec(&request.job).map_err(|_| CoordinatorError::Serialization)?;
        transaction.execute(
            "INSERT INTO distributed_index_jobs (
                tenant_id, project_id, job_id, kind, idempotency_algorithm, idempotency_value,
                spec_json, state, attempt, max_attempts, lease_generation,
                cancellation_requested, created_at_ms, updated_at_ms, deadline_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'queued', 1, ?8, 0, 0, ?9, ?9, ?10)",
            params![
                request.project.tenant_id.as_str(),
                request.project.project_id.as_str(),
                job.job_id.as_str(),
                kind_token(job.kind),
                request.job.idempotency_key.algorithm(),
                request.job.idempotency_key.value(),
                spec,
                self.limits.max_attempts.get(),
                now.timestamp_millis(),
                deadline_at.timestamp_millis(),
            ],
        )?;
        let record = load_record(&transaction, &job)?.ok_or(CoordinatorError::NotFound)?;
        transaction.commit()?;
        Ok(record)
    }

    fn inspect(
        &self,
        request: &InspectIndexJobRequest,
    ) -> Result<Option<IndexJobRecord>, Self::Error> {
        request
            .validate()
            .map_err(|_| CoordinatorError::InvalidRequest)?;
        load_record(&self.connection, &request.job)
    }

    fn claim(
        &mut self,
        request: &ClaimIndexJobRequest,
        now: DateTime<Utc>,
    ) -> Result<Option<IndexJobRecord>, Self::Error> {
        if !validate_version(request.protocol_version) {
            return Err(CoordinatorError::InvalidRequest);
        }
        let expires_at = self.lease_expiry(now)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        reclaim_expired(&transaction, &request.project, now)?;
        let selected = transaction
            .query_row(
                "SELECT job_id, attempt, max_attempts, lease_generation
                 FROM distributed_index_jobs
                 WHERE tenant_id = ?1 AND project_id = ?2 AND kind = ?3 AND state = 'queued'
                   AND cancellation_requested = 0
                 ORDER BY created_at_ms, job_id LIMIT 1",
                params![
                    request.project.tenant_id.as_str(),
                    request.project.project_id.as_str(),
                    kind_token(request.kind),
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, u32>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((job_id, attempt, max_attempts, previous_generation)) = selected else {
            transaction.commit()?;
            return Ok(None);
        };
        let previous_generation =
            u64::try_from(previous_generation).map_err(|_| CoordinatorError::Serialization)?;
        let next_attempt = if previous_generation == 0 {
            attempt
        } else {
            attempt.saturating_add(1)
        };
        if next_attempt == 0 || next_attempt > max_attempts {
            return Err(CoordinatorError::Conflict);
        }
        let generation = previous_generation
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or(CoordinatorError::Conflict)?;
        let changed = transaction.execute(
            "UPDATE distributed_index_jobs
             SET state = 'leased', attempt = ?1, lease_worker_id = ?2, lease_generation = ?3,
                 lease_until_ms = MIN(?4, deadline_at_ms), updated_at_ms = ?5,
                 failure_code = NULL
             WHERE tenant_id = ?6 AND project_id = ?7 AND job_id = ?8 AND state = 'queued'",
            params![
                next_attempt,
                request.worker_id.as_str(),
                i64_from_u64(generation.get())?,
                expires_at.timestamp_millis(),
                now.timestamp_millis(),
                request.project.tenant_id.as_str(),
                request.project.project_id.as_str(),
                job_id,
            ],
        )?;
        if changed != 1 {
            return Err(CoordinatorError::Conflict);
        }
        let job = IndexJobRef {
            project: request.project.clone(),
            job_id: IndexJobId::new(job_id).map_err(|_| CoordinatorError::Serialization)?,
            kind: request.kind,
        };
        let record = load_record(&transaction, &job)?.ok_or(CoordinatorError::NotFound)?;
        transaction.commit()?;
        Ok(Some(record))
    }

    fn start(
        &mut self,
        request: &AdvanceIndexJobRequest,
        now: DateTime<Utc>,
    ) -> Result<IndexJobRecord, Self::Error> {
        self.transition_with_lease(
            request,
            IndexJobState::Leased,
            IndexJobState::Running,
            now,
            false,
        )
    }

    fn heartbeat(
        &mut self,
        request: &HeartbeatJobRequest,
        now: DateTime<Utc>,
    ) -> Result<IndexJobRecord, Self::Error> {
        request
            .validate()
            .map_err(|_| CoordinatorError::InvalidRequest)?;
        let expires_at = self.lease_expiry(now)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record = load_record(&transaction, &request.job)?.ok_or(CoordinatorError::NotFound)?;
        require_live_lease(&record, &request.worker_id, request.lease_generation, now)?;
        if record.cancellation_requested || record.state == IndexJobState::Cancelled {
            return Err(CoordinatorError::Cancelled);
        }
        let lease_generation = i64_from_u64(request.lease_generation.get())?;
        let changed = transaction.execute(
            "UPDATE distributed_index_jobs
             SET lease_until_ms = MIN(?1, deadline_at_ms), updated_at_ms = ?2
             WHERE tenant_id = ?3 AND project_id = ?4 AND job_id = ?5
               AND lease_worker_id = ?6 AND lease_generation = ?7
               AND state IN ('leased', 'running', 'publishing')",
            params![
                expires_at.timestamp_millis(),
                now.timestamp_millis(),
                request.job.project.tenant_id.as_str(),
                request.job.project.project_id.as_str(),
                request.job.job_id.as_str(),
                request.worker_id.as_str(),
                lease_generation,
            ],
        )?;
        if changed != 1 {
            return Err(CoordinatorError::LeaseLost);
        }
        let updated = load_record(&transaction, &request.job)?.ok_or(CoordinatorError::NotFound)?;
        transaction.commit()?;
        Ok(updated)
    }

    fn begin_publication(
        &mut self,
        request: &AdvanceIndexJobRequest,
        now: DateTime<Utc>,
    ) -> Result<IndexJobRecord, Self::Error> {
        self.transition_with_lease(
            request,
            IndexJobState::Running,
            IndexJobState::Publishing,
            now,
            false,
        )
    }

    fn complete(
        &mut self,
        request: &AdvanceIndexJobRequest,
        now: DateTime<Utc>,
    ) -> Result<IndexJobRecord, Self::Error> {
        self.transition_with_lease(
            request,
            IndexJobState::Publishing,
            IndexJobState::Complete,
            now,
            true,
        )
    }

    fn fail(
        &mut self,
        request: &FailIndexJobRequest,
        now: DateTime<Utc>,
    ) -> Result<IndexJobRecord, Self::Error> {
        if !validate_version(request.protocol_version) {
            return Err(CoordinatorError::InvalidRequest);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record = load_record(&transaction, &request.job)?.ok_or(CoordinatorError::NotFound)?;
        require_live_lease(&record, &request.worker_id, request.lease_generation, now)?;
        if record.cancellation_requested || record.state == IndexJobState::Cancelled {
            return Err(CoordinatorError::Cancelled);
        }
        if !matches!(
            record.state,
            IndexJobState::Leased | IndexJobState::Running | IndexJobState::Publishing
        ) {
            return Err(CoordinatorError::Conflict);
        }
        let retry = request.retryable
            && record.attempt < record.max_attempts
            && matches!(record.state, IndexJobState::Leased | IndexJobState::Running);
        transaction.execute(
            "UPDATE distributed_index_jobs
             SET state = ?1, lease_worker_id = NULL, lease_until_ms = NULL,
                 failure_code = ?2, updated_at_ms = ?3
             WHERE tenant_id = ?4 AND project_id = ?5 AND job_id = ?6",
            params![
                if retry { "queued" } else { "failed" },
                request.failure_code.as_str(),
                now.timestamp_millis(),
                request.job.project.tenant_id.as_str(),
                request.job.project.project_id.as_str(),
                request.job.job_id.as_str(),
            ],
        )?;
        let updated = load_record(&transaction, &request.job)?.ok_or(CoordinatorError::NotFound)?;
        transaction.commit()?;
        Ok(updated)
    }

    fn cancel(
        &mut self,
        request: &CancelIndexJobRequest,
        now: DateTime<Utc>,
    ) -> Result<IndexJobRecord, Self::Error> {
        request
            .validate()
            .map_err(|_| CoordinatorError::InvalidRequest)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record = load_record(&transaction, &request.job)?.ok_or(CoordinatorError::NotFound)?;
        if record.state == IndexJobState::Cancelled {
            transaction.commit()?;
            return Ok(record);
        }
        if request
            .expected_state
            .is_some_and(|expected| expected != record.state)
        {
            return Err(CoordinatorError::Conflict);
        }
        if record.state.is_terminal() {
            transaction.commit()?;
            return Ok(record);
        }
        transaction.execute(
            "UPDATE distributed_index_jobs
             SET state = 'cancelled', cancellation_requested = 1, lease_worker_id = NULL,
                 lease_until_ms = NULL, failure_code = NULL, updated_at_ms = ?1
             WHERE tenant_id = ?2 AND project_id = ?3 AND job_id = ?4",
            params![
                now.timestamp_millis(),
                request.job.project.tenant_id.as_str(),
                request.job.project.project_id.as_str(),
                request.job.job_id.as_str(),
            ],
        )?;
        let updated = load_record(&transaction, &request.job)?.ok_or(CoordinatorError::NotFound)?;
        transaction.commit()?;
        Ok(updated)
    }

    fn reclaim(
        &mut self,
        request: &ReclaimIndexJobsRequest,
        now: DateTime<Utc>,
    ) -> Result<ReclaimIndexJobsResult, Self::Error> {
        if !validate_version(request.protocol_version) {
            return Err(CoordinatorError::InvalidRequest);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = reclaim_expired(&transaction, &request.project, now)?;
        transaction.commit()?;
        Ok(result)
    }
}

fn reclaim_expired(
    transaction: &Transaction<'_>,
    project: &super::identity::RemoteProjectRef,
    now: DateTime<Utc>,
) -> Result<ReclaimIndexJobsResult, CoordinatorError> {
    let scope = params![
        now.timestamp_millis(),
        project.tenant_id.as_str(),
        project.project_id.as_str()
    ];
    let cancelled = transaction.execute(
        "UPDATE distributed_index_jobs
         SET state = 'cancelled', lease_worker_id = NULL, lease_until_ms = NULL,
             failure_code = NULL, updated_at_ms = ?1
         WHERE tenant_id = ?2 AND project_id = ?3 AND cancellation_requested = 1
           AND lease_until_ms <= ?1 AND state IN ('leased', 'running', 'publishing')",
        scope,
    )? as u64;
    let timed_out = transaction.execute(
        "UPDATE distributed_index_jobs
         SET state = 'failed', lease_worker_id = NULL, lease_until_ms = NULL,
             failure_code = 'job.timeout', updated_at_ms = ?1
         WHERE tenant_id = ?2 AND project_id = ?3 AND cancellation_requested = 0
           AND deadline_at_ms <= ?1
           AND state IN ('queued', 'leased', 'running', 'publishing')",
        params![
            now.timestamp_millis(),
            project.tenant_id.as_str(),
            project.project_id.as_str()
        ],
    )? as u64;
    let attempt_limited = transaction.execute(
        "UPDATE distributed_index_jobs
         SET state = 'failed', lease_worker_id = NULL, lease_until_ms = NULL,
             failure_code = 'job.attempt_limit', updated_at_ms = ?1
         WHERE tenant_id = ?2 AND project_id = ?3 AND cancellation_requested = 0
           AND attempt >= max_attempts AND lease_until_ms <= ?1
           AND state IN ('leased', 'running', 'publishing')",
        params![
            now.timestamp_millis(),
            project.tenant_id.as_str(),
            project.project_id.as_str()
        ],
    )? as u64;
    let requeued = transaction.execute(
        "UPDATE distributed_index_jobs
         SET state = 'queued', lease_worker_id = NULL, lease_until_ms = NULL,
             failure_code = NULL, updated_at_ms = ?1
         WHERE tenant_id = ?2 AND project_id = ?3 AND cancellation_requested = 0
           AND attempt < max_attempts AND lease_until_ms <= ?1
           AND state IN ('leased', 'running', 'publishing')",
        params![
            now.timestamp_millis(),
            project.tenant_id.as_str(),
            project.project_id.as_str()
        ],
    )? as u64;
    Ok(ReclaimIndexJobsResult {
        requeued,
        failed: timed_out.saturating_add(attempt_limited),
        cancelled,
    })
}

fn require_live_lease(
    record: &IndexJobRecord,
    worker_id: &WorkerId,
    generation: NonZeroU64,
    now: DateTime<Utc>,
) -> Result<(), CoordinatorError> {
    let lease = record.lease.as_ref().ok_or(CoordinatorError::LeaseLost)?;
    if &lease.worker_id != worker_id
        || lease.generation != generation
        || lease.expires_at <= now
        || record.deadline_at <= now
    {
        return Err(CoordinatorError::LeaseLost);
    }
    Ok(())
}

fn load_record(
    connection: &Connection,
    job: &IndexJobRef,
) -> Result<Option<IndexJobRecord>, CoordinatorError> {
    connection
        .query_row(
            "SELECT kind, spec_json, state, attempt, max_attempts, lease_worker_id,
                    lease_generation, lease_until_ms, cancellation_requested, failure_code,
                    created_at_ms, updated_at_ms, deadline_at_ms
             FROM distributed_index_jobs
             WHERE tenant_id = ?1 AND project_id = ?2 AND job_id = ?3",
            params![
                job.project.tenant_id.as_str(),
                job.project.project_id.as_str(),
                job.job_id.as_str()
            ],
            |row| row_to_record(row, job),
        )
        .optional()
        .map_err(CoordinatorError::from)?
        .map_or(Ok(None), |record| {
            let record = record?;
            if record.job.kind != job.kind {
                return Err(CoordinatorError::NotFound);
            }
            record
                .validate()
                .map_err(|_| CoordinatorError::Serialization)?;
            Ok(Some(record))
        })
}

fn row_to_record(
    row: &Row<'_>,
    job: &IndexJobRef,
) -> rusqlite::Result<Result<IndexJobRecord, CoordinatorError>> {
    let kind = parse_kind(&row.get::<_, String>(0)?);
    let spec = serde_json::from_slice::<IndexJobSpec>(&row.get::<_, Vec<u8>>(1)?)
        .map_err(|_| CoordinatorError::Serialization);
    let state = parse_state(&row.get::<_, String>(2)?);
    let attempt = NonZeroU32::new(row.get::<_, u32>(3)?).ok_or(CoordinatorError::Serialization);
    let max_attempts =
        NonZeroU32::new(row.get::<_, u32>(4)?).ok_or(CoordinatorError::Serialization);
    let worker = row.get::<_, Option<String>>(5)?;
    let generation = row.get::<_, i64>(6)?;
    let expires = row.get::<_, Option<i64>>(7)?;
    let cancellation_requested = row.get::<_, bool>(8)?;
    let failure = row.get::<_, Option<String>>(9)?;
    let created = datetime(row.get::<_, i64>(10)?);
    let updated = datetime(row.get::<_, i64>(11)?);
    let deadline = datetime(row.get::<_, i64>(12)?);
    Ok((|| {
        let kind = kind?;
        let spec = spec?;
        let state = state?;
        let attempt = attempt?;
        let max_attempts = max_attempts?;
        let generation = u64::try_from(generation).map_err(|_| CoordinatorError::Serialization)?;
        let lease = match (worker, NonZeroU64::new(generation), expires) {
            (Some(worker_id), Some(generation), Some(expires_at)) => Some(JobLease {
                worker_id: WorkerId::new(worker_id).map_err(|_| CoordinatorError::Serialization)?,
                generation,
                expires_at: datetime(expires_at)?,
            }),
            (None, _, None) => None,
            _ => return Err(CoordinatorError::Serialization),
        };
        let failure_code = failure
            .map(IndexJobFailureCode::new)
            .transpose()
            .map_err(|_| CoordinatorError::Serialization)?;
        Ok(IndexJobRecord {
            job: IndexJobRef {
                project: job.project.clone(),
                job_id: job.job_id.clone(),
                kind,
            },
            spec,
            state,
            attempt,
            max_attempts,
            lease,
            cancellation_requested,
            failure_code,
            created_at: created?,
            updated_at: updated?,
            deadline_at: deadline?,
        })
    })())
}

fn initialize_schema(connection: &Connection) -> Result<(), CoordinatorError> {
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE IF NOT EXISTS distributed_coordinator_metadata (
             singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
             schema_version INTEGER NOT NULL
         );",
    )?;
    let version = connection
        .query_row(
            "SELECT schema_version FROM distributed_coordinator_metadata WHERE singleton = 1",
            [],
            |row| row.get::<_, u32>(0),
        )
        .optional()?;
    match version {
        None => {
            connection.execute(
                "INSERT OR IGNORE INTO distributed_coordinator_metadata (singleton, schema_version)
                 VALUES (1, ?1)",
                [COORDINATOR_SCHEMA_VERSION],
            )?;
            let installed = connection.query_row(
                "SELECT schema_version FROM distributed_coordinator_metadata WHERE singleton = 1",
                [],
                |row| row.get::<_, u32>(0),
            )?;
            if installed != COORDINATOR_SCHEMA_VERSION {
                return Err(CoordinatorError::IncompatibleSchema);
            }
        }
        Some(COORDINATOR_SCHEMA_VERSION) => {}
        Some(_) => return Err(CoordinatorError::IncompatibleSchema),
    }
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS project_deletion_tombstones (
             tenant_id TEXT NOT NULL,
             project_id TEXT NOT NULL,
             deletion_id TEXT NOT NULL,
             created_at_ms INTEGER NOT NULL,
             PRIMARY KEY (tenant_id, project_id)
         );
         CREATE TABLE IF NOT EXISTS distributed_index_jobs (
             tenant_id TEXT NOT NULL,
             project_id TEXT NOT NULL,
             job_id TEXT NOT NULL,
             kind TEXT NOT NULL,
             idempotency_algorithm TEXT NOT NULL,
             idempotency_value TEXT NOT NULL,
             spec_json BLOB NOT NULL,
             state TEXT NOT NULL,
             attempt INTEGER NOT NULL CHECK (attempt > 0),
             max_attempts INTEGER NOT NULL CHECK (max_attempts > 0),
             lease_worker_id TEXT,
             lease_generation INTEGER NOT NULL DEFAULT 0 CHECK (lease_generation >= 0),
             lease_until_ms INTEGER,
             cancellation_requested INTEGER NOT NULL DEFAULT 0,
             failure_code TEXT,
             created_at_ms INTEGER NOT NULL,
             updated_at_ms INTEGER NOT NULL,
             deadline_at_ms INTEGER NOT NULL,
             PRIMARY KEY (tenant_id, project_id, job_id),
             UNIQUE (tenant_id, project_id, kind, idempotency_algorithm, idempotency_value)
         );
         CREATE INDEX IF NOT EXISTS distributed_index_jobs_claim
             ON distributed_index_jobs (tenant_id, project_id, kind, state, created_at_ms);",
    )?;
    Ok(())
}

fn project_has_deletion_tombstone(
    connection: &Connection,
    project: &super::identity::RemoteProjectRef,
) -> Result<bool, CoordinatorError> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM project_deletion_tombstones
             WHERE tenant_id = ?1 AND project_id = ?2",
            params![project.tenant_id.as_str(), project.project_id.as_str()],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn kind_token(kind: IndexJobKind) -> &'static str {
    match kind {
        IndexJobKind::RepositoryGraph => "repository_graph",
        IndexJobKind::ProjectMemory => "project_memory",
    }
}

fn parse_kind(value: &str) -> Result<IndexJobKind, CoordinatorError> {
    match value {
        "repository_graph" => Ok(IndexJobKind::RepositoryGraph),
        "project_memory" => Ok(IndexJobKind::ProjectMemory),
        _ => Err(CoordinatorError::Serialization),
    }
}

fn parse_state(value: &str) -> Result<IndexJobState, CoordinatorError> {
    match value {
        "queued" => Ok(IndexJobState::Queued),
        "leased" => Ok(IndexJobState::Leased),
        "running" => Ok(IndexJobState::Running),
        "publishing" => Ok(IndexJobState::Publishing),
        "complete" => Ok(IndexJobState::Complete),
        "failed" => Ok(IndexJobState::Failed),
        "cancelled" => Ok(IndexJobState::Cancelled),
        _ => Err(CoordinatorError::Serialization),
    }
}

fn datetime(timestamp_millis: i64) -> Result<DateTime<Utc>, CoordinatorError> {
    DateTime::from_timestamp_millis(timestamp_millis).ok_or(CoordinatorError::Serialization)
}

fn i64_from_u64(value: u64) -> Result<i64, CoordinatorError> {
    i64::try_from(value).map_err(|_| CoordinatorError::Serialization)
}

#[cfg(test)]
#[path = "coordinator_sqlite_tests.rs"]
mod tests;
