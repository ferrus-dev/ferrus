//! Tenant-scoped immutable source-object storage.

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    num::NonZeroU64,
    path::{Path, PathBuf},
    time::Instant,
};

use ring::{
    aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey},
    rand::{SecureRandom, SystemRandom},
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::repository_graph::domain::Digest;

use super::identity::{ObjectId, RemoteProjectRef, TenantObjectRef};

const OBJECT_SCHEMA_VERSION: u32 = 1;
const ENVELOPE_MAGIC: &[u8; 8] = b"FERROBJ1";
const NONCE_BYTES: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectStoreProtection {
    pub authenticated_transport: bool,
    pub encrypted_at_rest: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectStoreQuota {
    pub max_objects_per_project: NonZeroU64,
    pub max_bytes_per_project: NonZeroU64,
    pub max_object_bytes: NonZeroU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutObjectOutcome {
    Stored,
    Reused,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutObjectResult {
    pub object: TenantObjectRef,
    pub outcome: PutObjectOutcome,
}

pub trait TenantObjectStore {
    type Error;

    fn protection(&self) -> ObjectStoreProtection;
    fn put_verified(
        &mut self,
        project: &RemoteProjectRef,
        content_identity: &Digest,
        content: &[u8],
    ) -> Result<PutObjectResult, Self::Error>;
    fn read_verified(&self, object: &TenantObjectRef) -> Result<Vec<u8>, Self::Error>;
}

#[derive(Debug, Error)]
pub enum ObjectStoreError {
    #[error("object store requires authenticated transport and encryption at rest")]
    InsecureProtection,
    #[error("object content identity does not match uploaded bytes")]
    ContentIdentityMismatch,
    #[error("object exceeds the per-object byte quota")]
    ObjectQuotaExceeded,
    #[error("project object-count quota exceeded")]
    ProjectObjectQuotaExceeded,
    #[error("project source-byte quota exceeded")]
    ProjectByteQuotaExceeded,
    #[error("tenant object is unavailable or outside the requested scope")]
    ObjectUnavailable,
    #[error("tenant object failed authenticated decryption or integrity verification")]
    IntegrityFailure,
    #[error("object store schema is incompatible")]
    IncompatibleSchema,
    #[error("object store database operation failed")]
    Database(#[source] rusqlite::Error),
    #[error("object store filesystem operation failed")]
    Io(#[source] std::io::Error),
    #[error("object encryption operation failed")]
    Encryption,
}

impl From<rusqlite::Error> for ObjectStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

impl From<std::io::Error> for ObjectStoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Durable prototype adapter. Source bytes are AES-256-GCM encrypted with
/// tenant/project/object scope as authenticated data. SQLite serializes quota
/// checks and idempotent metadata updates; object payloads remain immutable.
pub struct EncryptedFilesystemObjectStore {
    root: PathBuf,
    database: Connection,
    key: LessSafeKey,
    quota: ObjectStoreQuota,
    protection: ObjectStoreProtection,
}

impl EncryptedFilesystemObjectStore {
    pub fn open(
        root: impl AsRef<Path>,
        encryption_key: [u8; 32],
        quota: ObjectStoreQuota,
        authenticated_transport: bool,
    ) -> Result<Self, ObjectStoreError> {
        if !authenticated_transport {
            return Err(ObjectStoreError::InsecureProtection);
        }
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("objects"))?;
        let database = Connection::open(root.join("object-store.db"))?;
        database.busy_timeout(std::time::Duration::from_secs(5))?;
        initialize_schema(&database)?;
        let key = LessSafeKey::new(
            UnboundKey::new(&AES_256_GCM, &encryption_key)
                .map_err(|_| ObjectStoreError::Encryption)?,
        );
        Ok(Self {
            root,
            database,
            key,
            quota,
            protection: ObjectStoreProtection {
                authenticated_transport,
                encrypted_at_rest: true,
            },
        })
    }

    fn object_path(&self, object: &TenantObjectRef) -> PathBuf {
        self.root
            .join("objects")
            .join(object.project.tenant_id.as_str())
            .join(object.project.project_id.as_str())
            .join(format!("{}.enc", object.object_id.as_str()))
    }

    fn aad(object: &TenantObjectRef) -> Vec<u8> {
        format!(
            "{}\0{}\0{}\0{}\0{}",
            object.project.tenant_id,
            object.project.project_id,
            object.object_id,
            object.content_identity.algorithm(),
            object.content_identity.value()
        )
        .into_bytes()
    }

    fn contains(&self, object: &TenantObjectRef) -> Result<bool, ObjectStoreError> {
        Ok(self
            .database
            .query_row(
                "SELECT 1 FROM source_objects
                 WHERE tenant_id = ?1 AND project_id = ?2 AND object_id = ?3
                   AND digest_algorithm = ?4 AND digest_value = ?5",
                params![
                    object.project.tenant_id.as_str(),
                    object.project.project_id.as_str(),
                    object.object_id.as_str(),
                    object.content_identity.algorithm(),
                    object.content_identity.value()
                ],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    fn decrypt_file_until(
        key: &LessSafeKey,
        object: &TenantObjectRef,
        path: &Path,
        deadline: Option<Instant>,
    ) -> Result<Option<Vec<u8>>, ObjectStoreError> {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Ok(None);
        }
        let mut encoded = Vec::new();
        let mut file = File::open(path)?;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(None);
            }
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            encoded.extend_from_slice(&buffer[..read]);
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Ok(None);
        }
        if encoded.len() < ENVELOPE_MAGIC.len() + NONCE_BYTES + AES_256_GCM.tag_len()
            || &encoded[..ENVELOPE_MAGIC.len()] != ENVELOPE_MAGIC
        {
            return Err(ObjectStoreError::IntegrityFailure);
        }
        let nonce_start = ENVELOPE_MAGIC.len();
        let payload_start = nonce_start + NONCE_BYTES;
        let nonce = Nonce::try_assume_unique_for_key(&encoded[nonce_start..payload_start])
            .map_err(|_| ObjectStoreError::IntegrityFailure)?;
        let mut payload = encoded[payload_start..].to_vec();
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Ok(None);
        }
        let plaintext = key
            .open_in_place(nonce, Aad::from(Self::aad(object)), &mut payload)
            .map_err(|_| ObjectStoreError::IntegrityFailure)?;
        let plaintext = plaintext.to_vec();
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Ok(None);
        }
        if !verify_digest_until(&object.content_identity, &plaintext, deadline)? {
            return Ok(None);
        }
        Ok(Some(plaintext))
    }

    fn decrypt_file(
        key: &LessSafeKey,
        object: &TenantObjectRef,
        path: &Path,
    ) -> Result<Vec<u8>, ObjectStoreError> {
        Self::decrypt_file_until(key, object, path, None)?.ok_or(ObjectStoreError::IntegrityFailure)
    }

    fn write_encrypted(
        key: &LessSafeKey,
        object: &TenantObjectRef,
        path: &Path,
        content: &[u8],
    ) -> Result<(), ObjectStoreError> {
        let parent = path.parent().ok_or(ObjectStoreError::Encryption)?;
        fs::create_dir_all(parent)?;
        let mut nonce_bytes = [0u8; NONCE_BYTES];
        SystemRandom::new()
            .fill(&mut nonce_bytes)
            .map_err(|_| ObjectStoreError::Encryption)?;
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let mut payload = content.to_vec();
        key.seal_in_place_append_tag(nonce, Aad::from(Self::aad(object)), &mut payload)
            .map_err(|_| ObjectStoreError::Encryption)?;

        let nonce_suffix = nonce_bytes[..4]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let temporary = parent.join(format!(
            ".{}.{}.{}.tmp",
            object.object_id,
            std::process::id(),
            nonce_suffix
        ));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(ENVELOPE_MAGIC)?;
        file.write_all(&nonce_bytes)?;
        file.write_all(&payload)?;
        file.sync_all()?;
        drop(file);
        match fs::rename(&temporary, path) {
            Ok(()) => Ok(()),
            Err(_error) if path.exists() => {
                let _ = fs::remove_file(&temporary);
                if Self::decrypt_file(key, object, path)? == content {
                    Ok(())
                } else {
                    Err(ObjectStoreError::IntegrityFailure)
                }
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                Err(ObjectStoreError::Io(error))
            }
        }
    }

    /// Reads and verifies an object only while the caller's deadline remains.
    /// `None` means the deadline elapsed before verification completed.
    pub fn read_verified_until(
        &self,
        object: &TenantObjectRef,
        deadline: Instant,
    ) -> Result<Option<Vec<u8>>, ObjectStoreError> {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        let exists = self.contains(object)?;
        if Instant::now() >= deadline {
            return Ok(None);
        }
        if !exists {
            return Err(ObjectStoreError::ObjectUnavailable);
        }
        Self::decrypt_file_until(&self.key, object, &self.object_path(object), Some(deadline))
    }
}

impl TenantObjectStore for EncryptedFilesystemObjectStore {
    type Error = ObjectStoreError;

    fn protection(&self) -> ObjectStoreProtection {
        self.protection
    }

    fn put_verified(
        &mut self,
        project: &RemoteProjectRef,
        content_identity: &Digest,
        content: &[u8],
    ) -> Result<PutObjectResult, Self::Error> {
        if !self.protection.authenticated_transport || !self.protection.encrypted_at_rest {
            return Err(ObjectStoreError::InsecureProtection);
        }
        verify_digest(content_identity, content)?;
        let byte_len = u64::try_from(content.len()).unwrap_or(u64::MAX);
        if byte_len > self.quota.max_object_bytes.get() {
            return Err(ObjectStoreError::ObjectQuotaExceeded);
        }
        let object = TenantObjectRef {
            project: project.clone(),
            object_id: ObjectId::new(content_identity.value())
                .map_err(|_| ObjectStoreError::ContentIdentityMismatch)?,
            content_identity: content_identity.clone(),
        };
        let path = self.object_path(&object);
        let transaction = self
            .database
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = transaction
            .query_row(
                "SELECT byte_len FROM source_objects
                 WHERE tenant_id = ?1 AND project_id = ?2 AND object_id = ?3",
                params![
                    project.tenant_id.as_str(),
                    project.project_id.as_str(),
                    object.object_id.as_str()
                ],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if let Some(stored_len) = existing {
            if stored_len < 0 || stored_len as u64 != byte_len {
                return Err(ObjectStoreError::IntegrityFailure);
            }
            drop(transaction);
            if Self::decrypt_file(&self.key, &object, &path)? != content {
                return Err(ObjectStoreError::IntegrityFailure);
            }
            return Ok(PutObjectResult {
                object,
                outcome: PutObjectOutcome::Reused,
            });
        }

        let (objects, bytes): (i64, i64) = transaction.query_row(
            "SELECT COUNT(*), COALESCE(SUM(byte_len), 0) FROM source_objects
             WHERE tenant_id = ?1 AND project_id = ?2",
            params![project.tenant_id.as_str(), project.project_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let objects = u64::try_from(objects).map_err(|_| ObjectStoreError::IntegrityFailure)?;
        let bytes = u64::try_from(bytes).map_err(|_| ObjectStoreError::IntegrityFailure)?;
        if objects >= self.quota.max_objects_per_project.get() {
            return Err(ObjectStoreError::ProjectObjectQuotaExceeded);
        }
        if bytes.saturating_add(byte_len) > self.quota.max_bytes_per_project.get() {
            return Err(ObjectStoreError::ProjectByteQuotaExceeded);
        }

        Self::write_encrypted(&self.key, &object, &path, content)?;
        transaction.execute(
            "INSERT INTO source_objects (
                tenant_id, project_id, object_id, digest_algorithm, digest_value, byte_len
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                project.tenant_id.as_str(),
                project.project_id.as_str(),
                object.object_id.as_str(),
                content_identity.algorithm(),
                content_identity.value(),
                i64::try_from(byte_len).map_err(|_| ObjectStoreError::ObjectQuotaExceeded)?
            ],
        )?;
        transaction.commit()?;
        Ok(PutObjectResult {
            object,
            outcome: PutObjectOutcome::Stored,
        })
    }

    fn read_verified(&self, object: &TenantObjectRef) -> Result<Vec<u8>, Self::Error> {
        if !self.contains(object)? {
            return Err(ObjectStoreError::ObjectUnavailable);
        }
        Self::decrypt_file(&self.key, object, &self.object_path(object))
    }
}

fn initialize_schema(connection: &Connection) -> Result<(), ObjectStoreError> {
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS object_store_metadata (
             singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
             schema_version INTEGER NOT NULL
         );",
    )?;
    let version = connection
        .query_row(
            "SELECT schema_version FROM object_store_metadata WHERE singleton = 1",
            [],
            |row| row.get::<_, u32>(0),
        )
        .optional()?;
    match version {
        None => {
            connection.execute(
                "INSERT OR IGNORE INTO object_store_metadata (singleton, schema_version)
                 VALUES (1, ?1)",
                [OBJECT_SCHEMA_VERSION],
            )?;
            let installed = connection.query_row(
                "SELECT schema_version FROM object_store_metadata WHERE singleton = 1",
                [],
                |row| row.get::<_, u32>(0),
            )?;
            if installed != OBJECT_SCHEMA_VERSION {
                return Err(ObjectStoreError::IncompatibleSchema);
            }
        }
        Some(OBJECT_SCHEMA_VERSION) => {}
        Some(_) => return Err(ObjectStoreError::IncompatibleSchema),
    }
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS source_objects (
             tenant_id TEXT NOT NULL,
             project_id TEXT NOT NULL,
             object_id TEXT NOT NULL,
             digest_algorithm TEXT NOT NULL,
             digest_value TEXT NOT NULL,
             byte_len INTEGER NOT NULL CHECK (byte_len >= 0),
             PRIMARY KEY (tenant_id, project_id, object_id)
         );",
    )?;
    Ok(())
}

fn verify_digest(expected: &Digest, content: &[u8]) -> Result<(), ObjectStoreError> {
    if verify_digest_until(expected, content, None)? {
        Ok(())
    } else {
        Err(ObjectStoreError::IntegrityFailure)
    }
}

fn verify_digest_until(
    expected: &Digest,
    content: &[u8],
    deadline: Option<Instant>,
) -> Result<bool, ObjectStoreError> {
    if expected.algorithm() != "sha256" {
        return Err(ObjectStoreError::ContentIdentityMismatch);
    }
    let mut digest = Sha256::new();
    for chunk in content.chunks(64 * 1024) {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Ok(false);
        }
        digest.update(chunk);
    }
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Ok(false);
    }
    let actual = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Ok(false);
    }
    if actual != expected.value() {
        return Err(ObjectStoreError::ContentIdentityMismatch);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::distributed::identity::{RemoteProjectId, TenantId};

    fn project(tenant: &str) -> RemoteProjectRef {
        RemoteProjectRef {
            tenant_id: TenantId::new(tenant).unwrap(),
            project_id: RemoteProjectId::new("project").unwrap(),
        }
    }

    fn digest(content: &[u8]) -> Digest {
        let value = Sha256::digest(content)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Digest::new("sha256", value).unwrap()
    }

    fn quota() -> ObjectStoreQuota {
        ObjectStoreQuota {
            max_objects_per_project: NonZeroU64::new(4).unwrap(),
            max_bytes_per_project: NonZeroU64::new(1024).unwrap(),
            max_object_bytes: NonZeroU64::new(512).unwrap(),
        }
    }

    #[test]
    fn encrypted_objects_are_idempotent_and_plaintext_is_not_at_rest() {
        let directory = tempfile::tempdir().unwrap();
        let mut store =
            EncryptedFilesystemObjectStore::open(directory.path(), [7; 32], quota(), true).unwrap();
        let content = b"private repository content";
        let first = store
            .put_verified(&project("tenant-a"), &digest(content), content)
            .unwrap();
        assert_eq!(first.outcome, PutObjectOutcome::Stored);
        let repeated = store
            .put_verified(&project("tenant-a"), &digest(content), content)
            .unwrap();
        assert_eq!(repeated.outcome, PutObjectOutcome::Reused);
        assert_eq!(store.read_verified(&first.object).unwrap(), content);
        let encoded = fs::read(store.object_path(&first.object)).unwrap();
        assert!(
            !encoded
                .windows(content.len())
                .any(|window| window == content)
        );
    }

    #[test]
    fn verified_reads_stop_at_the_caller_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let mut store =
            EncryptedFilesystemObjectStore::open(directory.path(), [8; 32], quota(), true).unwrap();
        let content = b"deadline-bounded content";
        let stored = store
            .put_verified(&project("tenant-a"), &digest(content), content)
            .unwrap();

        assert_eq!(
            store
                .read_verified_until(&stored.object, Instant::now())
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .read_verified_until(
                    &stored.object,
                    Instant::now() + std::time::Duration::from_secs(1),
                )
                .unwrap()
                .unwrap(),
            content
        );
    }

    #[test]
    fn identical_content_remains_separate_and_unreadable_across_tenants() {
        let directory = tempfile::tempdir().unwrap();
        let mut store =
            EncryptedFilesystemObjectStore::open(directory.path(), [9; 32], quota(), true).unwrap();
        let content = b"same bytes";
        let left = store
            .put_verified(&project("tenant-a"), &digest(content), content)
            .unwrap();
        let right = store
            .put_verified(&project("tenant-b"), &digest(content), content)
            .unwrap();
        assert_ne!(left.object, right.object);
        assert_ne!(
            store.object_path(&left.object),
            store.object_path(&right.object)
        );
        let mut foreign = left.object.clone();
        foreign.project = project("tenant-c");
        assert!(matches!(
            store.read_verified(&foreign),
            Err(ObjectStoreError::ObjectUnavailable)
        ));
    }

    #[test]
    fn quotas_and_protection_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        assert!(matches!(
            EncryptedFilesystemObjectStore::open(directory.path(), [1; 32], quota(), false),
            Err(ObjectStoreError::InsecureProtection)
        ));
        let mut store = EncryptedFilesystemObjectStore::open(
            directory.path(),
            [1; 32],
            ObjectStoreQuota {
                max_objects_per_project: NonZeroU64::new(1).unwrap(),
                ..quota()
            },
            true,
        )
        .unwrap();
        store
            .put_verified(&project("tenant-a"), &digest(b"first"), b"first")
            .unwrap();
        assert!(matches!(
            store.put_verified(&project("tenant-a"), &digest(b"second"), b"second"),
            Err(ObjectStoreError::ProjectObjectQuotaExceeded)
        ));
    }

    #[test]
    fn authenticated_encryption_rejects_tampered_ciphertext() {
        let directory = tempfile::tempdir().unwrap();
        let mut store =
            EncryptedFilesystemObjectStore::open(directory.path(), [3; 32], quota(), true).unwrap();
        let content = b"integrity protected";
        let stored = store
            .put_verified(&project("tenant-a"), &digest(content), content)
            .unwrap();
        let path = store.object_path(&stored.object);
        let mut encoded = fs::read(&path).unwrap();
        *encoded.last_mut().unwrap() ^= 1;
        fs::write(path, encoded).unwrap();
        assert!(matches!(
            store.read_verified(&stored.object),
            Err(ObjectStoreError::IntegrityFailure)
        ));
    }

    #[test]
    fn incompatible_metadata_does_not_create_object_tables() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("object-store.db");
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE object_store_metadata (
                     singleton INTEGER PRIMARY KEY,
                     schema_version INTEGER NOT NULL
                 );
                 INSERT INTO object_store_metadata VALUES (1, 999);",
            )
            .unwrap();
        drop(connection);
        assert!(matches!(
            EncryptedFilesystemObjectStore::open(directory.path(), [1; 32], quota(), true),
            Err(ObjectStoreError::IncompatibleSchema)
        ));
        let connection = Connection::open(database_path).unwrap();
        let created = connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'source_objects'",
                [],
                |_| Ok(()),
            )
            .optional()
            .unwrap();
        assert!(created.is_none());
    }
}
