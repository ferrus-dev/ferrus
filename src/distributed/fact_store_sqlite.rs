//! Durable encrypted SQLite adapter for unpublished distributed fact batches.

use std::{num::NonZeroU64, path::Path};

use ring::{
    aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey},
    rand::{SecureRandom, SystemRandom},
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use thiserror::Error;

use super::{
    fact_store::{
        FactBatchProgress, FactBatchStore, FactStoreProtection, PutFactBatchOutcome,
        progress_from_headers,
    },
    protocol::{FactBatch, IndexJobKind, IndexJobRef},
};

const FACT_STORE_SCHEMA_VERSION: u32 = 1;
const NONCE_BYTES: usize = 12;
type EncryptedBatchRow = (String, Vec<u8>, Vec<u8>);

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
}

impl SqliteFactBatchStore {
    pub fn open(
        path: impl AsRef<Path>,
        encryption_key: [u8; 32],
        quota: FactStoreQuota,
        authenticated_transport: bool,
    ) -> Result<Self, FactStoreError> {
        if !authenticated_transport {
            return Err(FactStoreError::InsecureProtection);
        }
        let database = Connection::open(path)?;
        database.busy_timeout(std::time::Duration::from_secs(5))?;
        initialize_schema(&database)?;
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
}

impl FactBatchStore for SqliteFactBatchStore {
    type Error = FactStoreError;

    fn protection(&self) -> FactStoreProtection {
        self.protection
    }

    fn put(&mut self, batch: &FactBatch) -> Result<PutFactBatchOutcome, Self::Error> {
        if !self.protection.authenticated_transport || !self.protection.encrypted_at_rest {
            return Err(FactStoreError::InsecureProtection);
        }
        batch.validate().map_err(|_| FactStoreError::InvalidBatch)?;
        let encoded = serde_json::to_vec(batch).map_err(|_| FactStoreError::Serialization)?;
        let byte_len = u64::try_from(encoded.len()).unwrap_or(u64::MAX);
        if byte_len > self.quota.max_batch_bytes.get() {
            return Err(FactStoreError::BatchQuotaExceeded);
        }
        let (nonce, ciphertext) = Self::encrypt(&self.key, batch, &encoded)?;

        let transaction = self
            .database
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
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

    fn progress(&self, job: &IndexJobRef) -> Result<FactBatchProgress, Self::Error> {
        let batches = self.load_for_ingestion(job)?;
        Ok(progress_from_headers(
            job.clone(),
            batches.into_iter().map(|batch| batch.header),
        ))
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
        "CREATE TABLE IF NOT EXISTS unpublished_fact_batches (
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
mod tests {
    use super::*;
    use crate::{
        distributed::{
            identity::{
                FactShardId, IndexJobId, RemoteGraphSnapshotRef, RemoteProjectId, RemoteProjectRef,
                RemoteRepositoryId, RemoteRepositoryRef, TenantId,
            },
            protocol::{FactBatchPayload, FactTarget},
        },
        repository_graph::domain::{BuildId, Digest, SnapshotId},
    };

    fn project(tenant: &str) -> RemoteProjectRef {
        RemoteProjectRef {
            tenant_id: TenantId::new(tenant).unwrap(),
            project_id: RemoteProjectId::new("project").unwrap(),
        }
    }

    fn job(tenant: &str) -> IndexJobRef {
        IndexJobRef {
            project: project(tenant),
            job_id: IndexJobId::new("job").unwrap(),
            kind: IndexJobKind::RepositoryGraph,
        }
    }

    fn batch(tenant: &str, sequence: u32, final_batch: bool) -> FactBatch {
        let job = job(tenant);
        FactBatch::new(
            job.clone(),
            FactTarget::RepositoryGraph {
                snapshot: RemoteGraphSnapshotRef {
                    repository: RemoteRepositoryRef {
                        project: job.project.clone(),
                        repository_id: RemoteRepositoryId::new("repository").unwrap(),
                    },
                    snapshot_id: SnapshotId::new("snapshot").unwrap(),
                },
                build_id: BuildId::new("build").unwrap(),
            },
            FactShardId::new("all").unwrap(),
            sequence,
            Digest::new("sha256", "11").unwrap(),
            final_batch,
            FactBatchPayload::RepositoryGraph {
                nodes: Vec::new(),
                edges: Vec::new(),
                diagnostics: Vec::new(),
            },
        )
        .unwrap()
    }

    fn quota() -> FactStoreQuota {
        FactStoreQuota {
            max_batches_per_project: NonZeroU64::new(100).unwrap(),
            max_bytes_per_project: NonZeroU64::new(1024 * 1024).unwrap(),
            max_batch_bytes: NonZeroU64::new(1024 * 1024).unwrap(),
        }
    }

    #[test]
    fn batches_are_encrypted_idempotent_and_survive_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("facts.db");
        let expected = batch("tenant-a", 0, true);
        {
            let mut store = SqliteFactBatchStore::open(&path, [23; 32], quota(), true).unwrap();
            assert_eq!(store.put(&expected).unwrap(), PutFactBatchOutcome::Stored);
            assert_eq!(store.put(&expected).unwrap(), PutFactBatchOutcome::Reused);
            let progress = store.progress(&expected.header.job).unwrap();
            assert!(progress.final_batch_seen);
            assert_eq!(progress.batches.len(), 1);
        }
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            !bytes
                .windows(b"payload_digest".len())
                .any(|window| window == b"payload_digest")
        );
        let store = SqliteFactBatchStore::open(&path, [23; 32], quota(), true).unwrap();
        assert_eq!(
            store.load_for_ingestion(&expected.header.job).unwrap(),
            vec![expected]
        );
    }

    #[test]
    fn sequence_conflicts_and_foreign_scope_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let mut store =
            SqliteFactBatchStore::open(directory.path().join("facts.db"), [31; 32], quota(), true)
                .unwrap();
        let first = batch("tenant-a", 0, false);
        store.put(&first).unwrap();
        let conflicting_final = batch("tenant-a", 0, true);
        assert!(matches!(
            store.put(&conflicting_final),
            Err(FactStoreError::SequenceConflict)
        ));
        assert!(
            store
                .load_for_ingestion(&job("tenant-b"))
                .unwrap()
                .is_empty()
        );

        let directory = tempfile::tempdir().unwrap();
        let mut completed = SqliteFactBatchStore::open(
            directory.path().join("complete.db"),
            [37; 32],
            quota(),
            true,
        )
        .unwrap();
        completed.put(&batch("tenant-a", 0, true)).unwrap();
        assert!(matches!(
            completed.put(&batch("tenant-a", 1, false)),
            Err(FactStoreError::SequenceConflict)
        ));
    }

    #[test]
    fn ciphertext_tampering_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let mut store =
            SqliteFactBatchStore::open(directory.path().join("facts.db"), [41; 32], quota(), true)
                .unwrap();
        let expected = batch("tenant-a", 0, true);
        store.put(&expected).unwrap();
        store
            .database
            .execute(
                "UPDATE unpublished_fact_batches SET ciphertext = zeroblob(length(ciphertext))",
                [],
            )
            .unwrap();
        assert!(matches!(
            store.load_for_ingestion(&expected.header.job),
            Err(FactStoreError::IntegrityFailure)
        ));
    }

    #[test]
    fn incompatible_schema_does_not_create_fact_tables() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("facts.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE fact_store_metadata (
                     singleton INTEGER PRIMARY KEY,
                     schema_version INTEGER NOT NULL
                 );
                 INSERT INTO fact_store_metadata VALUES (1, 99);",
            )
            .unwrap();
        drop(connection);
        assert!(matches!(
            SqliteFactBatchStore::open(&path, [7; 32], quota(), true),
            Err(FactStoreError::IncompatibleSchema)
        ));
        let connection = Connection::open(path).unwrap();
        let exists = connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'unpublished_fact_batches'",
                [],
                |_| Ok(()),
            )
            .optional()
            .unwrap()
            .is_some();
        assert!(!exists);
    }
}
