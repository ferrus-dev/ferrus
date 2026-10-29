//! Encrypted batch persistence, quotas, authority checks, and idempotent writes.

use super::*;
use crate::{
    distributed::{
        identity::{
            FactShardId, IndexJobId, RemoteGraphSnapshotRef, RemoteProjectId, RemoteProjectRef,
            RemoteRepositoryId, RemoteRepositoryRef, TenantId, WorkerId,
        },
        protocol::{FactBatchPayload, FactTarget},
    },
    repository_graph::domain::{
        BuildId, Digest, RepositoryId, RepositoryNamespace, RepositoryRef, SnapshotId,
    },
};

fn project(tenant: &str) -> RemoteProjectRef {
    RemoteProjectRef {
        tenant_id: TenantId::new(tenant).unwrap(),
        project_id: RemoteProjectId::new("project").unwrap(),
    }
}

fn job_named(tenant: &str, job_id: &str) -> IndexJobRef {
    IndexJobRef {
        project: project(tenant),
        job_id: IndexJobId::new(job_id).unwrap(),
        kind: IndexJobKind::RepositoryGraph,
    }
}

fn job(tenant: &str) -> IndexJobRef {
    job_named(tenant, "job")
}

fn batch_for_job(job: IndexJobRef, sequence: u32, final_batch: bool) -> FactBatch {
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
            repository_identity: RepositoryRef {
                namespace: RepositoryNamespace::new("remote:test").unwrap(),
                repository_id: RepositoryId::new("root").unwrap(),
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

fn batch(tenant: &str, sequence: u32, final_batch: bool) -> FactBatch {
    batch_for_job(job(tenant), sequence, final_batch)
}

fn quota() -> FactStoreQuota {
    FactStoreQuota {
        max_batches_per_project: NonZeroU64::new(100).unwrap(),
        max_bytes_per_project: NonZeroU64::new(1024 * 1024).unwrap(),
        max_batch_bytes: NonZeroU64::new(1024 * 1024).unwrap(),
    }
}

fn authority() -> FactBatchWriteAuthority {
    authority_for("worker-a", 1)
}

fn authority_for(worker_id: &str, generation: u64) -> FactBatchWriteAuthority {
    FactBatchWriteAuthority {
        worker_id: WorkerId::new(worker_id).unwrap(),
        lease_generation: NonZeroU64::new(generation).unwrap(),
    }
}

fn write_deadline() -> Instant {
    Instant::now() + Duration::from_secs(60)
}

#[test]
fn batches_are_encrypted_idempotent_and_survive_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("facts.db");
    let expected = batch("tenant-a", 0, true);
    {
        let mut store =
            SqliteFactBatchStore::open_uncoordinated_for_test(&path, [23; 32], quota(), true)
                .unwrap();
        assert_eq!(
            store
                .put(&expected, &authority(), write_deadline())
                .unwrap(),
            PutFactBatchOutcome::Stored
        );
        assert_eq!(
            store
                .put(&expected, &authority(), write_deadline())
                .unwrap(),
            PutFactBatchOutcome::Reused
        );
        let FactBatchProgressOutcome::Available(progress) = store
            .progress(&expected.header.job, write_deadline())
            .unwrap()
        else {
            panic!("progress read should complete before its deadline");
        };
        assert!(progress.final_batch_seen);
        assert_eq!(progress.batches.len(), 1);
    }
    let bytes = std::fs::read(&path).unwrap();
    assert!(
        !bytes
            .windows(b"payload_digest".len())
            .any(|window| window == b"payload_digest")
    );
    let store =
        SqliteFactBatchStore::open_uncoordinated_for_test(&path, [23; 32], quota(), true).unwrap();
    assert_eq!(
        store.load_for_ingestion(&expected.header.job).unwrap(),
        vec![expected]
    );
}

#[test]
fn terminal_batches_are_reclaimed_before_project_quota_is_charged() {
    let directory = tempfile::tempdir().unwrap();
    let fact_path = directory.path().join("facts.db");
    let coordinator_path = directory.path().join("jobs.db");
    let jobs = ["one", "two", "tri", "end"].map(|id| job_named("tenant-a", id));
    let batches = jobs
        .iter()
        .cloned()
        .map(|job| batch_for_job(job, 0, true))
        .collect::<Vec<_>>();
    let batch_bytes = u64::try_from(serde_json::to_vec(&batches[0]).unwrap().len()).unwrap();
    assert!(batches.iter().all(|batch| {
        serde_json::to_vec(batch).unwrap().len() == usize::try_from(batch_bytes).unwrap()
    }));

    let coordinator = Connection::open(&coordinator_path).unwrap();
    coordinator
        .execute_batch(
            "CREATE TABLE distributed_coordinator_metadata (
                 singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                 schema_version INTEGER NOT NULL
             );
             INSERT INTO distributed_coordinator_metadata VALUES (1, 1);
             CREATE TABLE distributed_index_jobs (
                 tenant_id TEXT NOT NULL,
                 project_id TEXT NOT NULL,
                 job_id TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 state TEXT NOT NULL,
                 lease_worker_id TEXT,
                 lease_generation INTEGER NOT NULL,
                 lease_until_ms INTEGER,
                 cancellation_requested INTEGER NOT NULL,
                 PRIMARY KEY (tenant_id, project_id, job_id)
             );",
        )
        .unwrap();
    coordinator
        .execute(
            "INSERT INTO distributed_index_jobs
             (tenant_id, project_id, job_id, kind, state, lease_worker_id,
              lease_generation, lease_until_ms, cancellation_requested)
             VALUES (?1, ?2, ?3, 'repository_graph', 'running', 'worker-a', 1,
                     ?4, 0)",
            params![
                jobs[0].project.tenant_id.as_str(),
                jobs[0].project.project_id.as_str(),
                jobs[0].job_id.as_str(),
                Utc::now().timestamp_millis() + 60_000
            ],
        )
        .unwrap();

    let mut store = SqliteFactBatchStore::open(
        &fact_path,
        &coordinator_path,
        [47; 32],
        FactStoreQuota {
            max_batches_per_project: NonZeroU64::new(1).unwrap(),
            max_bytes_per_project: NonZeroU64::new(batch_bytes).unwrap(),
            max_batch_bytes: NonZeroU64::new(batch_bytes).unwrap(),
        },
        true,
    )
    .unwrap();
    for (index, terminal_state) in ["complete", "failed", "cancelled"].into_iter().enumerate() {
        assert_eq!(
            store
                .put(&batches[index], &authority(), write_deadline())
                .unwrap(),
            PutFactBatchOutcome::Stored
        );
        coordinator
            .execute(
                "UPDATE distributed_index_jobs SET state = ?1
                 WHERE tenant_id = ?2 AND project_id = ?3 AND job_id = ?4",
                params![
                    terminal_state,
                    jobs[index].project.tenant_id.as_str(),
                    jobs[index].project.project_id.as_str(),
                    jobs[index].job_id.as_str()
                ],
            )
            .unwrap();
        coordinator
            .execute(
                "INSERT INTO distributed_index_jobs
                 (tenant_id, project_id, job_id, kind, state, lease_worker_id,
                  lease_generation, lease_until_ms, cancellation_requested)
                 VALUES (?1, ?2, ?3, 'repository_graph', 'running', 'worker-a', 1,
                         ?4, 0)",
                params![
                    jobs[index + 1].project.tenant_id.as_str(),
                    jobs[index + 1].project.project_id.as_str(),
                    jobs[index + 1].job_id.as_str(),
                    Utc::now().timestamp_millis() + 60_000
                ],
            )
            .unwrap();
    }

    assert_eq!(
        store
            .put(&batches[3], &authority(), write_deadline())
            .unwrap(),
        PutFactBatchOutcome::Stored
    );
    for job in &jobs[..3] {
        assert!(store.load_for_ingestion(job).unwrap().is_empty());
    }

    assert_eq!(
        store.load_for_ingestion(&jobs[3]).unwrap(),
        vec![batches[3].clone()]
    );
}

#[test]
fn stale_worker_generation_cannot_write_after_lease_reclaim() {
    let directory = tempfile::tempdir().unwrap();
    let fact_path = directory.path().join("facts.db");
    let coordinator_path = directory.path().join("jobs.db");
    let job = job("tenant-a");
    let coordinator = Connection::open(&coordinator_path).unwrap();
    coordinator
        .execute_batch(
            "CREATE TABLE distributed_coordinator_metadata (
                 singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                 schema_version INTEGER NOT NULL
             );
             INSERT INTO distributed_coordinator_metadata VALUES (1, 1);
             CREATE TABLE distributed_index_jobs (
                 tenant_id TEXT NOT NULL,
                 project_id TEXT NOT NULL,
                 job_id TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 state TEXT NOT NULL,
                 lease_worker_id TEXT,
                 lease_generation INTEGER NOT NULL,
                 lease_until_ms INTEGER,
                 cancellation_requested INTEGER NOT NULL,
                 PRIMARY KEY (tenant_id, project_id, job_id)
             );",
        )
        .unwrap();
    coordinator
        .execute(
            "INSERT INTO distributed_index_jobs (
                 tenant_id, project_id, job_id, kind, state, lease_worker_id,
                 lease_generation, lease_until_ms, cancellation_requested
             ) VALUES (?1, ?2, ?3, 'repository_graph', 'running', 'worker-new', 2,
                       ?4, 0)",
            params![
                job.project.tenant_id.as_str(),
                job.project.project_id.as_str(),
                job.job_id.as_str(),
                Utc::now().timestamp_millis() + 60_000
            ],
        )
        .unwrap();
    let mut store =
        SqliteFactBatchStore::open(&fact_path, &coordinator_path, [53; 32], quota(), true).unwrap();
    let batch = batch_for_job(job, 0, true);

    assert!(matches!(
        store.put(&batch, &authority_for("worker-old", 1), write_deadline()),
        Err(FactStoreError::JobNotWritable)
    ));
    assert_eq!(
        store
            .put(&batch, &authority_for("worker-new", 2), write_deadline())
            .unwrap(),
        PutFactBatchOutcome::Stored
    );
}

#[test]
fn fact_batch_writes_stop_at_the_worker_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("facts.db");
    let mut store =
        SqliteFactBatchStore::open_uncoordinated_for_test(&database_path, [57; 32], quota(), true)
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
             UPDATE unpublished_fact_batches SET byte_len = byte_len;",
        )
        .unwrap();
    let started = Instant::now();
    let batch = batch("tenant-a", 0, true);

    let outcome = store
        .put(&batch, &authority(), started + Duration::from_millis(25))
        .unwrap();

    assert_eq!(outcome, PutFactBatchOutcome::DeadlineExceeded);
    assert!(started.elapsed() < Duration::from_secs(1));
    blocker.execute_batch("ROLLBACK").unwrap();
    assert!(
        store
            .load_for_ingestion(&batch.header.job)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn pre_sqlite_batch_processing_stops_at_the_worker_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = SqliteFactBatchStore::open_uncoordinated_for_test(
        directory.path().join("facts.db"),
        [58; 32],
        quota(),
        true,
    )
    .unwrap();
    let mut batch = batch("tenant-a", 0, true);
    batch.header.protocol_version = 0;

    let outcome = store
        .put_inner(&batch, &authority(), Instant::now())
        .unwrap();

    assert_eq!(outcome, PutFactBatchOutcome::DeadlineExceeded);
    assert!(
        store
            .load_for_ingestion(&batch.header.job)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn fact_batch_progress_stops_at_the_worker_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("facts.db");
    let mut store =
        SqliteFactBatchStore::open_uncoordinated_for_test(&database_path, [59; 32], quota(), true)
            .unwrap();
    let batch = batch("tenant-a", 0, true);
    store.put(&batch, &authority(), write_deadline()).unwrap();
    store
        .database
        .query_row("PRAGMA journal_mode = DELETE", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap();
    let blocker = Connection::open(&database_path).unwrap();
    blocker
        .execute_batch(
            "BEGIN EXCLUSIVE;
             UPDATE unpublished_fact_batches SET byte_len = byte_len;",
        )
        .unwrap();
    let started = Instant::now();

    let outcome = store
        .progress(&batch.header.job, started + Duration::from_millis(25))
        .unwrap();

    assert_eq!(outcome, FactBatchProgressOutcome::DeadlineExceeded);
    assert!(started.elapsed() < Duration::from_secs(1));
    blocker.execute_batch("ROLLBACK").unwrap();
    assert!(matches!(
        store
            .progress(&batch.header.job, write_deadline())
            .unwrap(),
        FactBatchProgressOutcome::Available(progress)
            if progress.final_batch_seen && progress.batches.len() == 1
    ));
}

#[test]
fn sequence_conflicts_and_foreign_scope_fail_closed() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = SqliteFactBatchStore::open_uncoordinated_for_test(
        directory.path().join("facts.db"),
        [31; 32],
        quota(),
        true,
    )
    .unwrap();
    let first = batch("tenant-a", 0, false);
    store.put(&first, &authority(), write_deadline()).unwrap();
    let conflicting_final = batch("tenant-a", 0, true);
    assert!(matches!(
        store.put(&conflicting_final, &authority(), write_deadline()),
        Err(FactStoreError::SequenceConflict)
    ));
    assert!(
        store
            .load_for_ingestion(&job("tenant-b"))
            .unwrap()
            .is_empty()
    );

    let directory = tempfile::tempdir().unwrap();
    let mut completed = SqliteFactBatchStore::open_uncoordinated_for_test(
        directory.path().join("complete.db"),
        [37; 32],
        quota(),
        true,
    )
    .unwrap();
    completed
        .put(&batch("tenant-a", 0, true), &authority(), write_deadline())
        .unwrap();
    assert!(matches!(
        completed.put(&batch("tenant-a", 1, false), &authority(), write_deadline()),
        Err(FactStoreError::SequenceConflict)
    ));
}

#[test]
fn project_deletion_tombstone_rejects_new_fact_batches() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = SqliteFactBatchStore::open_uncoordinated_for_test(
        directory.path().join("facts.db"),
        [39; 32],
        quota(),
        true,
    )
    .unwrap();
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
        store.put(&batch("tenant-a", 0, true), &authority(), write_deadline(),),
        Err(FactStoreError::ProjectDeleted)
    ));
}

#[test]
fn ciphertext_tampering_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = SqliteFactBatchStore::open_uncoordinated_for_test(
        directory.path().join("facts.db"),
        [41; 32],
        quota(),
        true,
    )
    .unwrap();
    let expected = batch("tenant-a", 0, true);
    store
        .put(&expected, &authority(), write_deadline())
        .unwrap();
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
        SqliteFactBatchStore::open_uncoordinated_for_test(&path, [7; 32], quota(), true),
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
