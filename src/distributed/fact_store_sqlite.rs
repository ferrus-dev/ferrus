//! Durable encrypted SQLite adapter for unpublished distributed fact batches.

use std::{
    num::NonZeroU64,
    path::Path,
    time::{Duration, Instant},
};

use chrono::Utc;
use ring::{
    aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey},
    rand::{SecureRandom, SystemRandom},
};
use rusqlite::{
    Connection, Error as SqliteError, ErrorCode, OptionalExtension, TransactionBehavior, params,
};
use thiserror::Error;

use super::{
    coordinator_sqlite::COORDINATOR_SCHEMA_VERSION,
    fact_store::{
        FactBatchProgressOutcome, FactBatchStore, FactBatchWriteAuthority, FactStoreProtection,
        PutFactBatchOutcome, progress_from_headers,
    },
    protocol::{FactBatch, IndexJobKind, IndexJobRef},
};

const FACT_STORE_SCHEMA_VERSION: u32 = 1;
const NONCE_BYTES: usize = 12;
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const SQLITE_PROGRESS_OPS: i32 = 100;
type EncryptedBatchRow = (String, Vec<u8>, Vec<u8>);

fn sqlite_timed_out<T>(result: &Result<T, FactStoreError>) -> bool {
    matches!(
        result,
        Err(FactStoreError::Database(SqliteError::SqliteFailure(failure, _)))
            if matches!(
                failure.code,
                ErrorCode::DatabaseBusy
                    | ErrorCode::DatabaseLocked
                    | ErrorCode::OperationInterrupted
            )
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FactStoreQuota {
    pub max_batches_per_project: NonZeroU64,
    pub max_bytes_per_project: NonZeroU64,
    pub max_batch_bytes: NonZeroU64,
}

#[derive(Debug, Error)]
pub enum FactStoreError {
    #[error("fact store requires authenticated transport and encryption at rest")]
    InsecureProtection,
    #[error("fact batch is invalid or belongs to a different logical sequence")]
    InvalidBatch,
    #[error("fact batch sequence or final marker conflicts with durable progress")]
    SequenceConflict,
    #[error("fact batch exceeds the per-batch byte quota")]
    BatchQuotaExceeded,
    #[error("project fact-batch count quota exceeded")]
    ProjectBatchQuotaExceeded,
    #[error("project fact-batch byte quota exceeded")]
    ProjectByteQuotaExceeded,
    #[error("project has a durable deletion tombstone")]
    ProjectDeleted,
    #[error("fact batches may be written only for a running coordinator job")]
    JobNotWritable,
    #[error("fact batch failed authenticated decryption or integrity verification")]
    IntegrityFailure,
    #[error("fact store schema is incompatible")]
    IncompatibleSchema,
    #[error("fact store database operation failed")]
    Database(#[source] rusqlite::Error),
    #[error("fact batch serialization failed")]
    Serialization,
    #[error("fact batch encryption failed")]
    Encryption,
}

impl From<rusqlite::Error> for FactStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

pub struct SqliteFactBatchStore {
    database: Connection,
    key: LessSafeKey,
    quota: FactStoreQuota,
    protection: FactStoreProtection,
    coordinator_attached: bool,
}

impl SqliteFactBatchStore {
    /// Opens a worker-facing fact store bound to the separate coordinator
    /// database for live-job validation and terminal-batch reclamation.
    pub fn open(
        path: impl AsRef<Path>,
        coordinator_path: impl AsRef<Path>,
        encryption_key: [u8; 32],
        quota: FactStoreQuota,
        authenticated_transport: bool,
    ) -> Result<Self, FactStoreError> {
        Self::open_inner(
            path.as_ref(),
            Some(coordinator_path.as_ref()),
            encryption_key,
            quota,
            authenticated_transport,
        )
    }

    #[cfg(test)]
    pub(crate) fn open_uncoordinated_for_test(
        path: impl AsRef<Path>,
        encryption_key: [u8; 32],
        quota: FactStoreQuota,
        authenticated_transport: bool,
    ) -> Result<Self, FactStoreError> {
        Self::open_inner(
            path.as_ref(),
            None,
            encryption_key,
            quota,
            authenticated_transport,
        )
    }

    fn open_inner(
        path: &Path,
        coordinator_path: Option<&Path>,
        encryption_key: [u8; 32],
        quota: FactStoreQuota,
        authenticated_transport: bool,
    ) -> Result<Self, FactStoreError> {
        if !authenticated_transport {
            return Err(FactStoreError::InsecureProtection);
        }

        let database = Connection::open(path)?;
        database.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
        initialize_schema(&database)?;
        if let Some(coordinator_path) = coordinator_path {
            database.execute(
                "ATTACH DATABASE ?1 AS coordinator",
                [coordinator_path.to_string_lossy().as_ref()],
            )?;
            let version = database
                .query_row(
                    "SELECT schema_version
                     FROM coordinator.distributed_coordinator_metadata
                     WHERE singleton = 1",
                    [],
                    |row| row.get::<_, u32>(0),
                )
                .optional()?;
            if version != Some(COORDINATOR_SCHEMA_VERSION) {
                return Err(FactStoreError::IncompatibleSchema);
            }
        }

        let key = LessSafeKey::new(
            UnboundKey::new(&AES_256_GCM, &encryption_key)
                .map_err(|_| FactStoreError::Encryption)?,
        );
        Ok(Self {
            database,
            key,
            quota,
            protection: FactStoreProtection {
                authenticated_transport,
                encrypted_at_rest: true,
            },
            coordinator_attached: coordinator_path.is_some(),
        })
    }

    fn aad(job: &IndexJobRef, batch_id: &str) -> Vec<u8> {
        format!(
            "{}\0{}\0{}\0{}\0{}",
            job.project.tenant_id,
            job.project.project_id,
            job.job_id,
            job_kind(job.kind),
            batch_id
        )
        .into_bytes()
    }

    fn encrypt(
        key: &LessSafeKey,
        batch: &FactBatch,
        plaintext: &[u8],
    ) -> Result<([u8; 12], Vec<u8>), FactStoreError> {
        let mut nonce_bytes = [0u8; NONCE_BYTES];
        SystemRandom::new()
            .fill(&mut nonce_bytes)
            .map_err(|_| FactStoreError::Encryption)?;
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let mut ciphertext = plaintext.to_vec();
        key.seal_in_place_append_tag(
            nonce,
            Aad::from(Self::aad(&batch.header.job, batch.header.batch_id.as_str())),
            &mut ciphertext,
        )
        .map_err(|_| FactStoreError::Encryption)?;
        Ok((nonce_bytes, ciphertext))
    }

    fn decrypt(
        &self,
        job: &IndexJobRef,
        batch_id: &str,
        nonce: &[u8],
        ciphertext: &[u8],
    ) -> Result<FactBatch, FactStoreError> {
        let nonce = Nonce::try_assume_unique_for_key(nonce)
            .map_err(|_| FactStoreError::IntegrityFailure)?;
        let mut plaintext = ciphertext.to_vec();
        let plaintext = self
            .key
            .open_in_place(nonce, Aad::from(Self::aad(job, batch_id)), &mut plaintext)
            .map_err(|_| FactStoreError::IntegrityFailure)?;
        let batch: FactBatch =
            serde_json::from_slice(plaintext).map_err(|_| FactStoreError::IntegrityFailure)?;
        batch
            .validate()
            .map_err(|_| FactStoreError::IntegrityFailure)?;
        if batch.header.job != *job || batch.header.batch_id.as_str() != batch_id {
            return Err(FactStoreError::IntegrityFailure);
        }

        Ok(batch)
    }

    fn rows_for_job(&self, job: &IndexJobRef) -> Result<Vec<EncryptedBatchRow>, FactStoreError> {
        let mut statement = self.database.prepare(
            "SELECT batch_id, nonce, ciphertext FROM unpublished_fact_batches
             WHERE tenant_id = ?1 AND project_id = ?2 AND job_id = ?3 AND job_kind = ?4
             ORDER BY shard_id, sequence",
        )?;
        let rows = statement.query_map(
            params![
                job.project.tenant_id.as_str(),
                job.project.project_id.as_str(),
                job.job_id.as_str(),
                job_kind(job.kind)
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    fn progress_inner(
        &self,
        job: &IndexJobRef,
        deadline: Instant,
    ) -> Result<FactBatchProgressOutcome, FactStoreError> {
        let mut statement = self.database.prepare(
            "SELECT batch_id, nonce, ciphertext FROM unpublished_fact_batches
             WHERE tenant_id = ?1 AND project_id = ?2 AND job_id = ?3 AND job_kind = ?4
             ORDER BY shard_id, sequence",
        )?;
        let mut rows = statement.query(params![
            job.project.tenant_id.as_str(),
            job.project.project_id.as_str(),
            job.job_id.as_str(),
            job_kind(job.kind)
        ])?;
        let mut headers = Vec::new();
        while let Some(row) = rows.next()? {
            if Instant::now() >= deadline {
                return Ok(FactBatchProgressOutcome::DeadlineExceeded);
            }

            let batch_id = row.get::<_, String>(0)?;
            let nonce = row.get::<_, Vec<u8>>(1)?;
            let ciphertext = row.get::<_, Vec<u8>>(2)?;
            if Instant::now() >= deadline {
                return Ok(FactBatchProgressOutcome::DeadlineExceeded);
            }

            let batch = self.decrypt(job, &batch_id, &nonce, &ciphertext)?;
            if Instant::now() >= deadline {
                return Ok(FactBatchProgressOutcome::DeadlineExceeded);
            }

            headers.push(batch.header);
        }

        Ok(FactBatchProgressOutcome::Available(progress_from_headers(
            job.clone(),
            headers,
        )))
    }
}

impl SqliteFactBatchStore {
    fn put_inner(
        &mut self,
        batch: &FactBatch,
        authority: &FactBatchWriteAuthority,
        deadline: Instant,
    ) -> Result<PutFactBatchOutcome, FactStoreError> {
        if !self.protection.authenticated_transport || !self.protection.encrypted_at_rest {
            return Err(FactStoreError::InsecureProtection);
        }

        if Instant::now() >= deadline {
            return Ok(PutFactBatchOutcome::DeadlineExceeded);
        }

        let validation = batch.validate();
        if Instant::now() >= deadline {
            return Ok(PutFactBatchOutcome::DeadlineExceeded);
        }

        validation.map_err(|_| FactStoreError::InvalidBatch)?;
        let encoded = serde_json::to_vec(batch);
        if Instant::now() >= deadline {
            return Ok(PutFactBatchOutcome::DeadlineExceeded);
        }

        let encoded = encoded.map_err(|_| FactStoreError::Serialization)?;
        let byte_len = u64::try_from(encoded.len()).unwrap_or(u64::MAX);
        if byte_len > self.quota.max_batch_bytes.get() {
            return Err(FactStoreError::BatchQuotaExceeded);
        }

        if Instant::now() >= deadline {
            return Ok(PutFactBatchOutcome::DeadlineExceeded);
        }

        let encrypted = Self::encrypt(&self.key, batch, &encoded);
        if Instant::now() >= deadline {
            return Ok(PutFactBatchOutcome::DeadlineExceeded);
        }

        let (nonce, ciphertext) = encrypted?;
        if Instant::now() >= deadline {
            return Ok(PutFactBatchOutcome::DeadlineExceeded);
        }

        let transaction = self
            .database
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if transaction
            .query_row(
                "SELECT 1 FROM project_deletion_tombstones
                 WHERE tenant_id = ?1 AND project_id = ?2",
                params![
                    batch.header.job.project.tenant_id.as_str(),
                    batch.header.job.project.project_id.as_str()
                ],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            return Err(FactStoreError::ProjectDeleted);
        }

        if self.coordinator_attached {
            transaction.execute(
                "DELETE FROM unpublished_fact_batches
                 WHERE tenant_id = ?1 AND project_id = ?2
                   AND EXISTS (
                       SELECT 1 FROM coordinator.distributed_index_jobs AS jobs
                       WHERE jobs.tenant_id = unpublished_fact_batches.tenant_id
                         AND jobs.project_id = unpublished_fact_batches.project_id
                         AND jobs.job_id = unpublished_fact_batches.job_id
                         AND jobs.kind = unpublished_fact_batches.job_kind
                         AND jobs.state IN ('complete', 'failed', 'cancelled')
                   )",
                params![
                    batch.header.job.project.tenant_id.as_str(),
                    batch.header.job.project.project_id.as_str()
                ],
            )?;
            let lease = transaction
                .query_row(
                    "SELECT state, lease_worker_id, lease_generation, lease_until_ms,
                            cancellation_requested
                     FROM coordinator.distributed_index_jobs
                     WHERE tenant_id = ?1 AND project_id = ?2 AND job_id = ?3 AND kind = ?4",
                    params![
                        batch.header.job.project.tenant_id.as_str(),
                        batch.header.job.project.project_id.as_str(),
                        batch.header.job.job_id.as_str(),
                        job_kind(batch.header.job.kind)
                    ],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, Option<i64>>(3)?,
                            row.get::<_, i64>(4)?,
                        ))
                    },
                )
                .optional()?;
            let expected_generation =
                i64::try_from(authority.lease_generation.get()).unwrap_or(i64::MAX);
            if !matches!(
                lease,
                Some((state, Some(worker_id), generation, Some(lease_until_ms), 0))
                    if state == "running"
                        && worker_id == authority.worker_id.as_str()
                        && generation == expected_generation
                        && lease_until_ms > Utc::now().timestamp_millis()
            ) {
                return Err(FactStoreError::JobNotWritable);
            }
        }

        let logical_existing = transaction
            .query_row(
                "SELECT batch_id, nonce, ciphertext FROM unpublished_fact_batches
                 WHERE tenant_id = ?1 AND project_id = ?2 AND job_id = ?3 AND job_kind = ?4
                   AND shard_id = ?5 AND sequence = ?6",
                params![
                    batch.header.job.project.tenant_id.as_str(),
                    batch.header.job.project.project_id.as_str(),
                    batch.header.job.job_id.as_str(),
                    job_kind(batch.header.job.kind),
                    batch.header.shard_id.as_str(),
                    i64::from(batch.header.sequence)
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()?;
        if let Some((batch_id, nonce, ciphertext)) = logical_existing {
            drop(transaction);
            let existing = self.decrypt(&batch.header.job, &batch_id, &nonce, &ciphertext)?;
            return if existing == *batch {
                Ok(PutFactBatchOutcome::Reused)
            } else {
                Err(FactStoreError::SequenceConflict)
            };
        }

        let final_exists = transaction
            .query_row(
                "SELECT 1 FROM unpublished_fact_batches
                 WHERE tenant_id = ?1 AND project_id = ?2 AND job_id = ?3 AND job_kind = ?4
                   AND final_batch = 1",
                params![
                    batch.header.job.project.tenant_id.as_str(),
                    batch.header.job.project.project_id.as_str(),
                    batch.header.job.job_id.as_str(),
                    job_kind(batch.header.job.kind)
                ],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if final_exists {
            return Err(FactStoreError::SequenceConflict);
        }

        let (count, bytes): (i64, i64) = transaction.query_row(
            "SELECT COUNT(*), COALESCE(SUM(byte_len), 0) FROM unpublished_fact_batches
             WHERE tenant_id = ?1 AND project_id = ?2",
            params![
                batch.header.job.project.tenant_id.as_str(),
                batch.header.job.project.project_id.as_str()
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let count = u64::try_from(count).map_err(|_| FactStoreError::IntegrityFailure)?;
        let bytes = u64::try_from(bytes).map_err(|_| FactStoreError::IntegrityFailure)?;
        if count >= self.quota.max_batches_per_project.get() {
            return Err(FactStoreError::ProjectBatchQuotaExceeded);
        }

        if bytes.saturating_add(byte_len) > self.quota.max_bytes_per_project.get() {
            return Err(FactStoreError::ProjectByteQuotaExceeded);
        }

        transaction.execute(
            "INSERT INTO unpublished_fact_batches (
                tenant_id, project_id, job_id, job_kind, batch_id, shard_id, sequence,
                final_batch, byte_len, nonce, ciphertext
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                batch.header.job.project.tenant_id.as_str(),
                batch.header.job.project.project_id.as_str(),
                batch.header.job.job_id.as_str(),
                job_kind(batch.header.job.kind),
                batch.header.batch_id.as_str(),
                batch.header.shard_id.as_str(),
                i64::from(batch.header.sequence),
                i64::from(batch.header.final_batch),
                i64::try_from(byte_len).map_err(|_| FactStoreError::BatchQuotaExceeded)?,
                nonce.as_slice(),
                ciphertext
            ],
        )?;
        transaction.commit()?;
        Ok(PutFactBatchOutcome::Stored)
    }
}

impl FactBatchStore for SqliteFactBatchStore {
    type Error = FactStoreError;

    fn protection(&self) -> FactStoreProtection {
        self.protection
    }

    fn put(
        &mut self,
        batch: &FactBatch,
        authority: &FactBatchWriteAuthority,
        deadline: Instant,
    ) -> Result<PutFactBatchOutcome, Self::Error> {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(PutFactBatchOutcome::DeadlineExceeded);
        };
        self.database.busy_timeout(remaining)?;
        if let Err(error) = self.database.progress_handler(
            SQLITE_PROGRESS_OPS,
            Some(move || Instant::now() >= deadline),
        ) {
            let _ = self.database.busy_timeout(SQLITE_BUSY_TIMEOUT);
            return Err(error.into());
        }

        let result = self.put_inner(batch, authority, deadline);
        let timed_out = Instant::now() >= deadline || sqlite_timed_out(&result);
        let _ = self.database.progress_handler(0, None::<fn() -> bool>);
        let _ = self.database.busy_timeout(SQLITE_BUSY_TIMEOUT);
        if timed_out {
            Ok(PutFactBatchOutcome::DeadlineExceeded)
        } else {
            result
        }
    }

    fn progress(
        &self,
        job: &IndexJobRef,
        deadline: Instant,
    ) -> Result<FactBatchProgressOutcome, Self::Error> {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(FactBatchProgressOutcome::DeadlineExceeded);
        };
        self.database.busy_timeout(remaining)?;
        if let Err(error) = self.database.progress_handler(
            SQLITE_PROGRESS_OPS,
            Some(move || Instant::now() >= deadline),
        ) {
            let _ = self.database.busy_timeout(SQLITE_BUSY_TIMEOUT);
            return Err(error.into());
        }

        let result = self.progress_inner(job, deadline);
        let timed_out = Instant::now() >= deadline || sqlite_timed_out(&result);
        let _ = self.database.progress_handler(0, None::<fn() -> bool>);
        let _ = self.database.busy_timeout(SQLITE_BUSY_TIMEOUT);
        if timed_out {
            Ok(FactBatchProgressOutcome::DeadlineExceeded)
        } else {
            result
        }
    }

    fn load_for_ingestion(&self, job: &IndexJobRef) -> Result<Vec<FactBatch>, Self::Error> {
        self.rows_for_job(job)?
            .into_iter()
            .map(|(batch_id, nonce, ciphertext)| self.decrypt(job, &batch_id, &nonce, &ciphertext))
            .collect()
    }
}

fn initialize_schema(connection: &Connection) -> Result<(), FactStoreError> {
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS fact_store_metadata (
             singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
             schema_version INTEGER NOT NULL
         );",
    )?;
    let version = connection
        .query_row(
            "SELECT schema_version FROM fact_store_metadata WHERE singleton = 1",
            [],
            |row| row.get::<_, u32>(0),
        )
        .optional()?;
    match version {
        None => {
            connection.execute(
                "INSERT OR IGNORE INTO fact_store_metadata (singleton, schema_version) VALUES (1, ?1)",
                [FACT_STORE_SCHEMA_VERSION],
            )?;
        }
        Some(version) if version == FACT_STORE_SCHEMA_VERSION => {}
        Some(_) => return Err(FactStoreError::IncompatibleSchema),
    }

    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS project_deletion_tombstones (
             tenant_id TEXT NOT NULL,
             project_id TEXT NOT NULL,
             deletion_id TEXT NOT NULL,
             created_at_ms INTEGER NOT NULL,
             PRIMARY KEY (tenant_id, project_id)
         );
         CREATE TABLE IF NOT EXISTS unpublished_fact_batches (
             tenant_id TEXT NOT NULL,
             project_id TEXT NOT NULL,
             job_id TEXT NOT NULL,
             job_kind TEXT NOT NULL,
             batch_id TEXT NOT NULL,
             shard_id TEXT NOT NULL,
             sequence INTEGER NOT NULL CHECK (sequence >= 0),
             final_batch INTEGER NOT NULL CHECK (final_batch IN (0, 1)),
             byte_len INTEGER NOT NULL CHECK (byte_len >= 0),
             nonce BLOB NOT NULL,
             ciphertext BLOB NOT NULL,
             PRIMARY KEY (tenant_id, project_id, job_id, job_kind, shard_id, sequence),
             UNIQUE (tenant_id, project_id, job_id, job_kind, batch_id)
         );
         CREATE UNIQUE INDEX IF NOT EXISTS one_final_batch_per_job
             ON unpublished_fact_batches (tenant_id, project_id, job_id, job_kind)
             WHERE final_batch = 1;",
    )?;
    Ok(())
}

fn job_kind(kind: IndexJobKind) -> &'static str {
    match kind {
        IndexJobKind::RepositoryGraph => "repository_graph",
        IndexJobKind::ProjectMemory => "project_memory",
    }
}

#[cfg(test)]
#[path = "fact_store_sqlite_tests.rs"]
mod tests;
