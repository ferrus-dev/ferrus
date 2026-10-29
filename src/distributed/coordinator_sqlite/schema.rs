//! Coordinator schema and database value conversion.

use super::*;

pub(super) fn initialize_schema(connection: &Connection) -> Result<(), CoordinatorError> {
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
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
             ON distributed_index_jobs (tenant_id, project_id, kind, state, created_at_ms);
         CREATE TABLE IF NOT EXISTS distributed_index_job_advance_receipts (
             tenant_id TEXT NOT NULL,
             project_id TEXT NOT NULL,
             job_id TEXT NOT NULL,
             expected_state TEXT NOT NULL,
             next_state TEXT NOT NULL,
             worker_id TEXT NOT NULL,
             lease_generation INTEGER NOT NULL CHECK (lease_generation > 0),
             outcome_json BLOB NOT NULL,
             PRIMARY KEY (
                 tenant_id, project_id, job_id, expected_state, next_state,
                 worker_id, lease_generation
             ),
             FOREIGN KEY (tenant_id, project_id, job_id)
                 REFERENCES distributed_index_jobs (tenant_id, project_id, job_id)
                 ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS distributed_index_job_failure_receipts (
             tenant_id TEXT NOT NULL,
             project_id TEXT NOT NULL,
             job_id TEXT NOT NULL,
             worker_id TEXT NOT NULL,
             lease_generation INTEGER NOT NULL CHECK (lease_generation > 0),
             failure_code TEXT NOT NULL,
             retryable INTEGER NOT NULL,
             outcome_json BLOB NOT NULL,
             PRIMARY KEY (
                 tenant_id, project_id, job_id, worker_id, lease_generation,
                 failure_code, retryable
             ),
             FOREIGN KEY (tenant_id, project_id, job_id)
                 REFERENCES distributed_index_jobs (tenant_id, project_id, job_id)
                 ON DELETE CASCADE
         );",
    )?;
    Ok(())
}

pub(super) fn project_has_deletion_tombstone(
    connection: &Connection,
    project: &super::super::identity::RemoteProjectRef,
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

pub(super) fn kind_token(kind: IndexJobKind) -> &'static str {
    match kind {
        IndexJobKind::RepositoryGraph => "repository_graph",
        IndexJobKind::ProjectMemory => "project_memory",
    }
}

pub(super) fn parse_kind(value: &str) -> Result<IndexJobKind, CoordinatorError> {
    match value {
        "repository_graph" => Ok(IndexJobKind::RepositoryGraph),
        "project_memory" => Ok(IndexJobKind::ProjectMemory),
        _ => Err(CoordinatorError::Serialization),
    }
}

pub(super) fn parse_state(value: &str) -> Result<IndexJobState, CoordinatorError> {
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

pub(super) fn datetime(timestamp_millis: i64) -> Result<DateTime<Utc>, CoordinatorError> {
    DateTime::from_timestamp_millis(timestamp_millis).ok_or(CoordinatorError::Serialization)
}

pub(super) fn i64_from_u64(value: u64) -> Result<i64, CoordinatorError> {
    i64::try_from(value).map_err(|_| CoordinatorError::Serialization)
}
