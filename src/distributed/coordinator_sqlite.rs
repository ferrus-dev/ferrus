//! Durable SQLite prototype for at-least-once distributed index jobs.

use std::{
    num::{NonZeroU32, NonZeroU64},
    path::Path,
};

use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};
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

const COORDINATOR_SCHEMA_VERSION: u32 = 1;

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
    #[error("distributed coordinator schema is incompatible")]
    IncompatibleSchema,
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
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
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
        "CREATE TABLE IF NOT EXISTS distributed_index_jobs (
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
mod tests {
    use super::*;
    use crate::{
        distributed::{
            DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            coordinator::{AdvanceIndexJobRequest, ClaimIndexJobRequest, ReclaimIndexJobsRequest},
            identity::{
                ObjectId, RemoteProjectId, RemoteRepositoryId, RepositoryManifestId, RequestId,
                TenantId, TenantObjectRef,
            },
            protocol::{IndexInputRef, IndexSemantics},
        },
        repository_graph::domain::Digest,
    };

    fn digest(value: &str) -> Digest {
        Digest::new("sha256", value).unwrap()
    }

    fn project(tenant: &str) -> super::super::identity::RemoteProjectRef {
        super::super::identity::RemoteProjectRef {
            tenant_id: TenantId::new(tenant).unwrap(),
            project_id: RemoteProjectId::new("project").unwrap(),
        }
    }

    fn submit_request(tenant: &str) -> SubmitIndexJobRequest {
        let project = project(tenant);
        let input = IndexInputRef::Repository(super::super::identity::RepositoryManifestRef {
            repository: super::super::identity::RemoteRepositoryRef {
                project: project.clone(),
                repository_id: RemoteRepositoryId::new("repository").unwrap(),
            },
            manifest_id: RepositoryManifestId::new("manifest").unwrap(),
            manifest_digest: digest("11"),
            source_policy_digest: digest("22"),
            manifest_object: TenantObjectRef {
                project: project.clone(),
                object_id: ObjectId::new("11").unwrap(),
                content_identity: digest("11"),
            },
        });
        let job = IndexJobSpec::new(
            IndexJobKind::RepositoryGraph,
            input,
            IndexSemantics {
                semantic_config_digest: digest("33"),
                model_version: NonZeroU32::new(1).unwrap(),
                extractor_set_digest: digest("44"),
            },
        )
        .unwrap();
        SubmitIndexJobRequest {
            protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            request_id: RequestId::new("submit").unwrap(),
            project,
            job,
        }
    }

    fn limits(max_attempts: u32) -> CoordinatorLimits {
        CoordinatorLimits {
            max_attempts: NonZeroU32::new(max_attempts).unwrap(),
            lease_ttl_ms: NonZeroU64::new(1_000).unwrap(),
            max_job_duration_ms: NonZeroU64::new(60_000).unwrap(),
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    fn claim_request(tenant: &str, worker: &str) -> ClaimIndexJobRequest {
        ClaimIndexJobRequest {
            protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            request_id: RequestId::new(format!("claim-{worker}")).unwrap(),
            project: project(tenant),
            kind: IndexJobKind::RepositoryGraph,
            worker_id: WorkerId::new(worker).unwrap(),
        }
    }

    fn advance(record: &IndexJobRecord) -> AdvanceIndexJobRequest {
        let lease = record.lease.as_ref().unwrap();
        AdvanceIndexJobRequest {
            protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            request_id: RequestId::new("advance").unwrap(),
            job: record.job.clone(),
            worker_id: lease.worker_id.clone(),
            lease_generation: lease.generation,
        }
    }

    #[test]
    fn repeated_submission_converges_and_survives_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("coordinator.db");
        let request = submit_request("tenant-a");
        let first = {
            let mut coordinator = SqliteIndexJobCoordinator::open(&path, limits(3)).unwrap();
            let first = coordinator.submit(&request, now()).unwrap();
            let repeated = coordinator.submit(&request, now()).unwrap();
            assert_eq!(first, repeated);
            first
        };
        let coordinator = SqliteIndexJobCoordinator::open(&path, limits(3)).unwrap();
        let inspected = coordinator
            .inspect(&InspectIndexJobRequest {
                protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                request_id: RequestId::new("inspect").unwrap(),
                job: first.job.clone(),
            })
            .unwrap()
            .unwrap();
        assert_eq!(inspected, first);
        assert_eq!(inspected.state, IndexJobState::Queued);
    }

    #[test]
    fn concurrent_duplicate_submissions_create_one_durable_job() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("coordinator.db");
        SqliteIndexJobCoordinator::open(&path, limits(3)).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles = (0..2)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut coordinator = SqliteIndexJobCoordinator::open(path, limits(3)).unwrap();
                    barrier.wait();
                    coordinator
                        .submit(&submit_request("tenant-a"), now())
                        .unwrap()
                        .job
                        .job_id
                })
            })
            .collect::<Vec<_>>();
        let jobs = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(jobs[0], jobs[1]);
    }

    #[test]
    fn lease_heartbeat_and_state_transitions_are_generation_guarded() {
        let directory = tempfile::tempdir().unwrap();
        let mut coordinator =
            SqliteIndexJobCoordinator::open(directory.path().join("jobs.db"), limits(3)).unwrap();
        coordinator
            .submit(&submit_request("tenant-a"), now())
            .unwrap();
        let leased = coordinator
            .claim(&claim_request("tenant-a", "worker-a"), now())
            .unwrap()
            .unwrap();
        assert_eq!(leased.state, IndexJobState::Leased);
        assert!(
            coordinator
                .claim(&claim_request("tenant-a", "worker-b"), now())
                .unwrap()
                .is_none()
        );
        let running = coordinator.start(&advance(&leased), now()).unwrap();
        let lease = running.lease.as_ref().unwrap();
        let heartbeat_at = now() + Duration::milliseconds(500);
        let heartbeat = coordinator
            .heartbeat(
                &HeartbeatJobRequest {
                    protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                    request_id: RequestId::new("heartbeat").unwrap(),
                    job: running.job.clone(),
                    worker_id: lease.worker_id.clone(),
                    lease_generation: lease.generation,
                },
                heartbeat_at,
            )
            .unwrap();
        assert!(heartbeat.lease.unwrap().expires_at > lease.expires_at);
        let publishing = coordinator
            .begin_publication(&advance(&running), now())
            .unwrap();
        let complete = coordinator.complete(&advance(&publishing), now()).unwrap();
        assert_eq!(complete.state, IndexJobState::Complete);
        assert!(complete.lease.is_none());
        assert!(complete.failure_code.is_none());
    }

    #[test]
    fn expired_leases_requeue_then_stop_at_the_attempt_limit() {
        let directory = tempfile::tempdir().unwrap();
        let mut coordinator =
            SqliteIndexJobCoordinator::open(directory.path().join("jobs.db"), limits(2)).unwrap();
        coordinator
            .submit(&submit_request("tenant-a"), now())
            .unwrap();
        let first = coordinator
            .claim(&claim_request("tenant-a", "worker-a"), now())
            .unwrap()
            .unwrap();
        let after_expiry = now() + Duration::seconds(2);
        let reclaimed = coordinator
            .reclaim(
                &ReclaimIndexJobsRequest {
                    protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                    request_id: RequestId::new("reclaim-one").unwrap(),
                    project: project("tenant-a"),
                },
                after_expiry,
            )
            .unwrap();
        assert_eq!(reclaimed.requeued, 1);
        assert!(matches!(
            coordinator.start(&advance(&first), after_expiry),
            Err(CoordinatorError::LeaseLost)
        ));

        let second = coordinator
            .claim(&claim_request("tenant-a", "worker-b"), after_expiry)
            .unwrap()
            .unwrap();
        assert_eq!(second.attempt.get(), 2);
        assert!(second.lease.as_ref().unwrap().generation.get() > 1);
        let final_reclaim = coordinator
            .reclaim(
                &ReclaimIndexJobsRequest {
                    protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                    request_id: RequestId::new("reclaim-two").unwrap(),
                    project: project("tenant-a"),
                },
                after_expiry + Duration::seconds(2),
            )
            .unwrap();
        assert_eq!(final_reclaim.failed, 1);
        let record = coordinator
            .inspect(&InspectIndexJobRequest {
                protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                request_id: RequestId::new("inspect-failed").unwrap(),
                job: second.job,
            })
            .unwrap()
            .unwrap();
        assert_eq!(record.state, IndexJobState::Failed);
        assert_eq!(record.failure_code.unwrap().as_str(), "job.attempt_limit");
    }

    #[test]
    fn total_job_deadline_is_enforced_before_claim() {
        let directory = tempfile::tempdir().unwrap();
        let mut coordinator = SqliteIndexJobCoordinator::open(
            directory.path().join("jobs.db"),
            CoordinatorLimits {
                max_attempts: NonZeroU32::new(3).unwrap(),
                lease_ttl_ms: NonZeroU64::new(1_000).unwrap(),
                max_job_duration_ms: NonZeroU64::new(500).unwrap(),
            },
        )
        .unwrap();
        let submitted = coordinator
            .submit(&submit_request("tenant-a"), now())
            .unwrap();
        assert!(
            coordinator
                .claim(
                    &claim_request("tenant-a", "worker-a"),
                    now() + Duration::seconds(1),
                )
                .unwrap()
                .is_none()
        );
        let failed = coordinator
            .inspect(&InspectIndexJobRequest {
                protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                request_id: RequestId::new("inspect-timeout").unwrap(),
                job: submitted.job,
            })
            .unwrap()
            .unwrap();
        assert_eq!(failed.state, IndexJobState::Failed);
        assert_eq!(failed.failure_code.unwrap().as_str(), "job.timeout");
    }

    #[test]
    fn cancellation_revokes_the_lease_and_prevents_publication() {
        let directory = tempfile::tempdir().unwrap();
        let mut coordinator =
            SqliteIndexJobCoordinator::open(directory.path().join("jobs.db"), limits(3)).unwrap();
        coordinator
            .submit(&submit_request("tenant-a"), now())
            .unwrap();
        let leased = coordinator
            .claim(&claim_request("tenant-a", "worker-a"), now())
            .unwrap()
            .unwrap();
        let cancelled = coordinator
            .cancel(
                &CancelIndexJobRequest {
                    protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                    request_id: RequestId::new("cancel").unwrap(),
                    job: leased.job.clone(),
                    expected_state: Some(IndexJobState::Leased),
                },
                now(),
            )
            .unwrap();
        assert_eq!(cancelled.state, IndexJobState::Cancelled);
        assert!(cancelled.cancellation_requested);
        assert!(matches!(
            coordinator.start(&advance(&leased), now()),
            Err(CoordinatorError::LeaseLost)
        ));
    }

    #[test]
    fn retryable_worker_failure_requeues_with_a_bounded_code() {
        let directory = tempfile::tempdir().unwrap();
        let mut coordinator =
            SqliteIndexJobCoordinator::open(directory.path().join("jobs.db"), limits(2)).unwrap();
        coordinator
            .submit(&submit_request("tenant-a"), now())
            .unwrap();
        let leased = coordinator
            .claim(&claim_request("tenant-a", "worker-a"), now())
            .unwrap()
            .unwrap();
        let running = coordinator.start(&advance(&leased), now()).unwrap();
        let lease = running.lease.as_ref().unwrap();
        let requeued = coordinator
            .fail(
                &FailIndexJobRequest {
                    protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                    request_id: RequestId::new("retry").unwrap(),
                    job: running.job.clone(),
                    worker_id: lease.worker_id.clone(),
                    lease_generation: lease.generation,
                    failure_code: IndexJobFailureCode::new("object.timeout").unwrap(),
                    retryable: true,
                },
                now(),
            )
            .unwrap();
        assert_eq!(requeued.state, IndexJobState::Queued);
        assert_eq!(
            requeued.failure_code.as_ref().unwrap().as_str(),
            "object.timeout"
        );
        let second = coordinator
            .claim(&claim_request("tenant-a", "worker-b"), now())
            .unwrap()
            .unwrap();
        assert_eq!(second.attempt.get(), 2);
        assert!(second.failure_code.is_none());
    }

    #[test]
    fn job_lookup_and_claim_are_tenant_scoped() {
        let directory = tempfile::tempdir().unwrap();
        let mut coordinator =
            SqliteIndexJobCoordinator::open(directory.path().join("jobs.db"), limits(3)).unwrap();
        let tenant_a = coordinator
            .submit(&submit_request("tenant-a"), now())
            .unwrap();
        coordinator
            .submit(&submit_request("tenant-b"), now())
            .unwrap();
        let mut foreign_job = tenant_a.job.clone();
        foreign_job.project = project("tenant-c");
        assert!(
            coordinator
                .inspect(&InspectIndexJobRequest {
                    protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                    request_id: RequestId::new("foreign-inspect").unwrap(),
                    job: foreign_job,
                })
                .unwrap()
                .is_none()
        );
        let claimed = coordinator
            .claim(&claim_request("tenant-b", "worker-b"), now())
            .unwrap()
            .unwrap();
        assert_eq!(claimed.job.project, project("tenant-b"));
    }

    #[test]
    fn incompatible_metadata_does_not_create_job_tables() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("jobs.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE distributed_coordinator_metadata (
                     singleton INTEGER PRIMARY KEY,
                     schema_version INTEGER NOT NULL
                 );
                 INSERT INTO distributed_coordinator_metadata VALUES (1, 999);",
            )
            .unwrap();
        drop(connection);
        assert!(matches!(
            SqliteIndexJobCoordinator::open(&path, limits(3)),
            Err(CoordinatorError::IncompatibleSchema)
        ));
        let connection = Connection::open(path).unwrap();
        let created = connection
            .query_row(
                "SELECT 1 FROM sqlite_master
                 WHERE type = 'table' AND name = 'distributed_index_jobs'",
                [],
                |_| Ok(()),
            )
            .optional()
            .unwrap();
        assert!(created.is_none());
    }
}
