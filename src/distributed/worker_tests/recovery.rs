//! Tests for recovery.

use super::*;

#[test]
fn repository_worker_recovers_the_pinned_builtin_extractor_subset() {
    let identities = builtin_extractor_identities();
    let rust_only = identities
        .into_iter()
        .filter(|identity| identity.id.as_str() == "builtin.rust-syntax")
        .collect::<Vec<_>>();
    let selected =
        repository_extractor_selection(&extractor_set_digest(&rust_only).unwrap()).unwrap();
    assert!(selected.file_ids.contains("builtin.rust-syntax"));
    assert!(!selected.generic_enabled);
    assert!(!selected.resolver_enabled);
}

#[test]
fn repository_worker_is_deterministic_and_reuses_durable_batches() {
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
    let mut per_source_facts = fact_store(&storage_dir.path().join("per-source-facts.db"));
    let mut per_source_limited = worker_limits();
    per_source_limited.max_facts_per_source = NonZeroU64::new(10).unwrap();
    assert_eq!(
        StatelessIndexWorker::new(per_source_limited).execute(
            &request,
            &jobs,
            &objects,
            &mut per_source_facts,
        ),
        Err(WorkerError::OutputLimitExceeded)
    );
    assert!(
        per_source_facts
            .load_for_ingestion(&request.job.job)
            .unwrap()
            .is_empty()
    );
    let mut limited_facts = fact_store(&storage_dir.path().join("limited-facts.db"));
    let mut limited = worker_limits();
    limited.max_total_facts = NonZeroU64::new(1).unwrap();
    assert_eq!(
        StatelessIndexWorker::new(limited).execute(&request, &jobs, &objects, &mut limited_facts,),
        Err(WorkerError::OutputLimitExceeded)
    );
    assert!(
        limited_facts
            .load_for_ingestion(&request.job.job)
            .unwrap()
            .is_empty()
    );
    let mut facts = fact_store(&storage_dir.path().join("facts.db"));
    let worker = StatelessIndexWorker::new(worker_limits());
    let first = worker
        .execute(&request, &jobs, &objects, &mut facts)
        .unwrap();
    let batches = facts.load_for_ingestion(&request.job.job).unwrap();
    assert!(first.progress.final_batch_seen);
    assert!(first.emitted_facts > 0);
    assert!(batches.len() > 1);
    assert!(batches.iter().all(|batch| batch.validate().is_ok()));

    let mut singleton_limits = worker_limits();
    singleton_limits.max_facts_per_batch = NonZeroU64::new(1).unwrap();
    let mut singleton_facts = fact_store(&storage_dir.path().join("singleton-facts.db"));
    StatelessIndexWorker::new(singleton_limits)
        .execute(&request, &jobs, &objects, &mut singleton_facts)
        .unwrap();
    let singleton_batches = singleton_facts
        .load_for_ingestion(&request.job.job)
        .unwrap();
    let batch_cap = max_encoded_batch_bytes(&singleton_batches);
    let mut byte_limited = worker_limits();
    byte_limited.max_facts_per_batch = NonZeroU64::new(100_000).unwrap();
    byte_limited.max_batch_bytes = batch_cap;
    let mut byte_sized_facts = fact_store(&storage_dir.path().join("byte-sized-facts.db"));
    let byte_sized = StatelessIndexWorker::new(byte_limited)
        .execute(&request, &jobs, &objects, &mut byte_sized_facts)
        .unwrap();
    let byte_sized_batches = byte_sized_facts
        .load_for_ingestion(&request.job.job)
        .unwrap();
    assert_eq!(byte_sized.emitted_facts, first.emitted_facts);
    assert!(byte_sized_batches.len() > 1);
    assert!(
        byte_sized_batches
            .iter()
            .all(|batch| { serde_json::to_vec(batch).unwrap().len() as u64 <= batch_cap.get() })
    );

    let repeated = worker
        .execute(&request, &jobs, &objects, &mut facts)
        .unwrap();
    assert_eq!(repeated.target, first.target);
    assert_eq!(repeated.progress, first.progress);
    assert_eq!(repeated.stored_batches, 0);
    assert_eq!(repeated.reused_batches, first.progress.batches.len() as u64);

    jobs.cancel(
        &CancelIndexJobRequest {
            protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            request_id: RequestId::new("cancel").unwrap(),
            job: request.job.job.clone(),
            expected_state: Some(IndexJobState::Running),
        },
        Utc::now(),
    )
    .unwrap();
    assert_eq!(
        worker.execute(&request, &jobs, &objects, &mut facts),
        Err(WorkerError::AuthorityLost)
    );
}
