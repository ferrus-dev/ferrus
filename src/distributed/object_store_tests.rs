//! Encrypted object integrity, quota enforcement, and idempotent storage.

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
fn verified_batch_rejects_quota_without_persisting_a_prefix() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = EncryptedFilesystemObjectStore::open(
        directory.path(),
        [7; 32],
        ObjectStoreQuota {
            max_objects_per_project: NonZeroU64::new(1).unwrap(),
            max_bytes_per_project: NonZeroU64::new(1024).unwrap(),
            max_object_bytes: NonZeroU64::new(512).unwrap(),
        },
        true,
    )
    .unwrap();
    let first = b"first package object";
    let second = b"second package object";
    let first_digest = digest(first);
    let second_digest = digest(second);

    let result = store.put_verified_batch(
        &project("tenant-a"),
        &[
            VerifiedObject {
                content_identity: &first_digest,
                content: first,
            },
            VerifiedObject {
                content_identity: &second_digest,
                content: second,
            },
        ],
    );

    assert!(matches!(
        result,
        Err(ObjectStoreError::ProjectObjectQuotaExceeded)
    ));
    let stored: i64 = store
        .database
        .query_row("SELECT COUNT(*) FROM source_objects", [], |row| row.get(0))
        .unwrap();
    assert_eq!(stored, 0);
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
fn bounded_reads_reject_oversized_objects_before_returning_content() {
    let directory = tempfile::tempdir().unwrap();
    let mut store =
        EncryptedFilesystemObjectStore::open(directory.path(), [10; 32], quota(), true).unwrap();
    let content = b"manifest larger than worker limit";
    let stored = store
        .put_verified(&project("tenant-a"), &digest(content), content)
        .unwrap();
    let deadline = Instant::now() + std::time::Duration::from_secs(1);

    assert_eq!(
        store
            .read_verified_bounded(
                &stored.object,
                u64::try_from(content.len() - 1).unwrap(),
                deadline,
            )
            .unwrap(),
        BoundedObjectRead::SizeExceeded
    );
    assert_eq!(
        store
            .read_verified_bounded(
                &stored.object,
                u64::try_from(content.len()).unwrap(),
                deadline,
            )
            .unwrap(),
        BoundedObjectRead::Content(content.to_vec())
    );
}

#[test]
fn bounded_reads_stop_at_the_sqlite_lock_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("object-store.db");
    let mut store =
        EncryptedFilesystemObjectStore::open(directory.path(), [12; 32], quota(), true).unwrap();
    let content = b"lock-bounded content";
    let stored = store
        .put_verified(&project("tenant-a"), &digest(content), content)
        .unwrap();
    store
        .database
        .query_row("PRAGMA journal_mode = DELETE", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap();
    let blocker = Connection::open(database_path).unwrap();
    blocker
        .execute_batch(
            "BEGIN EXCLUSIVE;
             UPDATE source_objects SET byte_len = byte_len;",
        )
        .unwrap();
    let started = Instant::now();

    let result = store
        .read_verified_bounded(
            &stored.object,
            u64::try_from(content.len()).unwrap(),
            started + std::time::Duration::from_millis(25),
        )
        .unwrap();

    assert_eq!(result, BoundedObjectRead::DeadlineExceeded);
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    blocker.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn project_deletion_tombstone_rejects_new_objects() {
    let directory = tempfile::tempdir().unwrap();
    let mut store =
        EncryptedFilesystemObjectStore::open(directory.path(), [11; 32], quota(), true).unwrap();
    let project = project("tenant-a");
    store
        .database
        .execute(
            "INSERT INTO project_deletion_tombstones (
                 tenant_id, project_id, deletion_id, created_at_ms
             ) VALUES (?1, ?2, 'delete-project', 1)",
            params![project.tenant_id.as_str(), project.project_id.as_str()],
        )
        .unwrap();

    assert!(matches!(
        store.put_verified(&project, &digest(b"new source"), b"new source"),
        Err(ObjectStoreError::ProjectDeleted)
    ));
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
