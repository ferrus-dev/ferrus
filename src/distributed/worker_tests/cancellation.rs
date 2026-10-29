//! Tests for cancellation.

use super::*;

#[test]
fn worker_deadline_budget_is_terminal_and_clamped_between_attempts() {
    assert_eq!(remaining_duration_ms(100, 0), Ok(100));
    assert_eq!(remaining_duration_ms(100, 99), Ok(1));
    assert_eq!(
        remaining_duration_ms(100, 100),
        Err(WorkerError::DeadlineExceeded)
    );
    assert_eq!(
        remaining_duration_ms(100, 101),
        Err(WorkerError::DeadlineExceeded)
    );
    let now = Utc::now();
    assert_eq!(
        effective_worker_duration_ms(30_000, now, now + chrono::Duration::milliseconds(250)),
        Ok(250)
    );
    assert_eq!(
        effective_worker_duration_ms(30_000, now, now - chrono::Duration::milliseconds(1)),
        Err(WorkerError::DeadlineExceeded)
    );

    let context = ExtractionContext {
        snapshot_id: crate::repository_graph::domain::SnapshotId::new("snapshot").unwrap(),
        build_id: BuildId::new("build").unwrap(),
        repository: local_repository(),
        max_facts_per_file: 100,
        max_parser_duration_ms: 5_000,
        max_diagnostics: 50,
    };
    let attempt = extraction_context_for_attempt(&context, 7, 1);
    assert_eq!(attempt.max_parser_duration_ms, 1);
    assert_eq!(attempt.max_diagnostics, 7);
}

#[test]
fn worker_authorization_stops_at_the_coordinator_lock_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("jobs.db");
    let mut jobs = coordinator(&database_path);
    let manifest_digest = Digest::new("sha256", "aa").unwrap();
    let running = running_job(
        &mut jobs,
        IndexJobKind::RepositoryGraph,
        IndexInputRef::Repository(RepositoryManifestRef {
            repository: remote_repository(),
            repository_identity: local_repository(),
            manifest_id: RepositoryManifestId::new("aa").unwrap(),
            manifest_digest: manifest_digest.clone(),
            source_policy_digest: Digest::new("sha256", "bb").unwrap(),
            expected_snapshot_id: SnapshotId::new("snapshot-auth-deadline").unwrap(),
            manifest_object: TenantObjectRef {
                project: remote_project(),
                object_id: ObjectId::new("aa").unwrap(),
                content_identity: manifest_digest,
            },
        }),
        IndexSemantics {
            semantic_config_digest: Digest::new("sha256", "cc").unwrap(),
            model_version: NonZeroU32::new(GRAPH_MODEL_VERSION).unwrap(),
            extractor_set_digest: Digest::new("sha256", "dd").unwrap(),
        },
    );
    let request = execution_request(running);
    jobs.use_delete_journal_for_test().unwrap();
    let blocker = Connection::open(&database_path).unwrap();
    blocker
        .execute_batch(
            "BEGIN EXCLUSIVE;
             UPDATE distributed_index_jobs SET updated_at_ms = updated_at_ms;",
        )
        .unwrap();
    let started = Instant::now();
    let deadline = WorkerDeadline {
        started,
        limit_ms: 25,
    };

    let result = StatelessIndexWorker::new(worker_limits()).authorize(&request, &jobs, deadline);

    assert_eq!(result, Err(WorkerError::DeadlineExceeded));
    assert!(started.elapsed() < Duration::from_secs(1));
    blocker.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn worker_rechecks_the_deadline_after_fact_store_operations() {
    let repository_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repository_dir.path().join("src")).unwrap();
    std::fs::write(
        repository_dir.path().join("src/lib.rs"),
        b"pub struct Api;\nimpl Api { pub fn run(&self) {} }\n",
    )
    .unwrap();
    commit_repository(repository_dir.path());
    let config = RepositoryGraphConfig::default();
    let identities = builtin_extractor_identities();
    let context =
        SourceDiscoveryContext::from_config(local_repository(), &config, &identities).unwrap();
    let source = LocalRepositorySource::discover(repository_dir.path(), context).unwrap();
    let storage_dir = tempfile::tempdir().unwrap();
    let mut objects = object_store(&storage_dir.path().join("objects"));
    let manifest = package_repository_source(
        &source,
        remote_repository(),
        RepositoryPackagingPolicy {
            schema_version: 1,
            source_policy_digest: config.source_policy_digest().unwrap(),
        },
        packaging_limits(),
        &mut objects,
    )
    .unwrap();
    let mut jobs = coordinator(&storage_dir.path().join("jobs.db"));
    let running = running_job(
        &mut jobs,
        IndexJobKind::RepositoryGraph,
        IndexInputRef::Repository(manifest.reference.clone()),
        IndexSemantics {
            semantic_config_digest: manifest.body.source_revision.analysis_config_digest.clone(),
            model_version: NonZeroU32::new(GRAPH_MODEL_VERSION).unwrap(),
            extractor_set_digest: manifest.body.extractor_set_digest.clone(),
        },
    );
    let request = execution_request(running);
    let mut limits = worker_limits();
    // One batch makes the post-put check observable without another loop iteration.
    limits.max_facts_per_batch = NonZeroU64::new(100_000).unwrap();
    let mut worker = StatelessIndexWorker::new(limits);
    // Freeze worker elapsed time until the store expires it. Real storage calls
    // retain the normal deadline, so setup speed cannot consume a tiny test budget.
    let elapsed_ms = Arc::new(AtomicU64::new(0));
    worker.elapsed_ms = Some(elapsed_ms.clone());

    let mut unexpired = ExpiringFactStore::new(
        fact_store(&storage_dir.path().join("unexpired-facts.db")),
        elapsed_ms.clone(),
        false,
        false,
    );
    let outcome = worker
        .execute(&request, &jobs, &objects, &mut unexpired)
        .unwrap();
    assert_eq!(outcome.stored_batches, 1);
    assert_eq!(unexpired.progress_calls.get(), 1);

    let mut expired_put = ExpiringFactStore::new(
        fact_store(&storage_dir.path().join("expired-put-facts.db")),
        elapsed_ms.clone(),
        true,
        false,
    );
    assert_eq!(
        worker.execute(&request, &jobs, &objects, &mut expired_put),
        Err(WorkerError::DeadlineExceeded)
    );
    assert_eq!(expired_put.put_calls, 1);
    assert_eq!(expired_put.progress_calls.get(), 0);

    elapsed_ms.store(0, Ordering::Relaxed);
    let mut expired_progress = ExpiringFactStore::new(
        fact_store(&storage_dir.path().join("expired-progress-facts.db")),
        elapsed_ms,
        false,
        true,
    );
    assert_eq!(
        worker.execute(&request, &jobs, &objects, &mut expired_progress),
        Err(WorkerError::DeadlineExceeded)
    );
    assert_eq!(expired_progress.put_calls, 1);
    assert_eq!(expired_progress.progress_calls.get(), 1);
}
