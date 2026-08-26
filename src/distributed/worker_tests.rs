use std::{cell::Cell, time::Duration};

use rusqlite::Connection;
use sha2::{Digest as _, Sha256};

use super::*;
use crate::{
    distributed::{
        coordinator::{AdvanceIndexJobRequest, ClaimIndexJobRequest},
        coordinator_sqlite::{CoordinatorLimits, SqliteIndexJobCoordinator},
        fact_store::{FactBatchProgress, FactBatchStore, FactStoreProtection, PutFactBatchOutcome},
        fact_store_sqlite::{FactStoreQuota, SqliteFactBatchStore},
        identity::{
            MemoryManifestId, ObjectId, RemoteProjectId, RemoteProjectRef, RemoteRepositoryId,
            RemoteRepositoryRef, RepositoryManifestId, RepositoryManifestRef, RequestId, TenantId,
            TenantObjectRef,
        },
        object_store::{EncryptedFilesystemObjectStore, ObjectStoreQuota},
        protocol::{
            CancelIndexJobRequest, FactBatch, FactBatchPayload, IndexJobKind, IndexJobRef,
            IndexJobSpec, IndexSemantics, SubmitIndexJobRequest,
        },
        publication::{RemoteFactCounts, RemoteGraphSnapshotRecord, StoredRemoteGraphSnapshot},
        source::{
            PackagingLimits, RepositoryPackagingPolicy, package_memory_source,
            package_repository_source,
        },
    },
    project_memory::{
        documents::parse_spec_memory,
        domain::{
            AuthorizedSourceDescriptor, MemoryRelationshipTarget, MemorySourceCategory,
            MemorySourceLocator, ProjectId, ProjectNamespace, ProjectRef,
        },
        extractors::canonical_digest as memory_digest,
        policy::MemoryPolicy,
        ports::{MemorySource, MemorySourceContent},
    },
    repository_graph::{
        config::RepositoryGraphConfig,
        domain::{
            BuildId, Confidence, Digest, ExtractorId, ExtractorIdentity, FactProvenance, GraphNode,
            NodeId, RepoPath, RepositoryId, RepositoryNamespace, RepositoryRef, ResolutionState,
            SemanticKey, SnapshotId, SourceEvidence,
        },
        source::{LocalRepositorySource, SourceDiscoveryContext},
    },
};

fn remote_project() -> RemoteProjectRef {
    RemoteProjectRef {
        tenant_id: TenantId::new("tenant-a").unwrap(),
        project_id: RemoteProjectId::new("project").unwrap(),
    }
}

fn remote_repository() -> RemoteRepositoryRef {
    RemoteRepositoryRef {
        project: remote_project(),
        repository_id: RemoteRepositoryId::new("repository").unwrap(),
    }
}

fn local_repository() -> RepositoryRef {
    RepositoryRef {
        namespace: RepositoryNamespace::new("local:test").unwrap(),
        repository_id: RepositoryId::new("repository").unwrap(),
    }
}

fn local_project() -> ProjectRef {
    ProjectRef {
        namespace: ProjectNamespace::new("local:test").unwrap(),
        project_id: ProjectId::new("project").unwrap(),
    }
}

fn git(root: &std::path::Path, args: &[&str]) {
    assert!(
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap()
            .success()
    );
}

fn commit_repository(root: &std::path::Path) {
    git(root, &["init"]);
    git(root, &["config", "user.email", "tests@example.com"]);
    git(root, &["config", "user.name", "Ferrus Tests"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    git(root, &["add", "--all"]);
    git(root, &["commit", "-m", "initial"]);
}

fn packaging_limits() -> PackagingLimits {
    PackagingLimits {
        max_objects: NonZeroU64::new(100).unwrap(),
        max_total_bytes: NonZeroU64::new(1024 * 1024).unwrap(),
        max_diagnostics: NonZeroU64::new(100).unwrap(),
    }
}

fn worker_limits() -> WorkerLimits {
    WorkerLimits {
        max_input_objects: NonZeroU64::new(100).unwrap(),
        max_input_bytes: NonZeroU64::new(1024 * 1024).unwrap(),
        max_object_bytes: NonZeroU64::new(1024 * 1024).unwrap(),
        max_facts_per_source: NonZeroU64::new(10_000).unwrap(),
        max_total_facts: NonZeroU64::new(100_000).unwrap(),
        max_diagnostics: NonZeroU64::new(1_000).unwrap(),
        max_parser_duration_ms: NonZeroU64::new(5_000).unwrap(),
        max_resolver_duration_ms: NonZeroU64::new(5_000).unwrap(),
        max_job_duration_ms: NonZeroU64::new(30_000).unwrap(),
        max_facts_per_batch: NonZeroU64::new(3).unwrap(),
        max_batch_bytes: NonZeroU64::new(1024 * 1024).unwrap(),
        max_output_bytes: NonZeroU64::new(16 * 1024 * 1024).unwrap(),
    }
}

fn sandbox() -> WorkerSandbox {
    WorkerSandbox {
        repository_execution: RepositoryExecutionPolicy::Denied,
        egress: WorkerEgressPolicy::ControlAndObjectStoreOnly,
        filesystem: WorkerFilesystemPolicy::EphemeralNoHostMounts,
        memory_limit_bytes: NonZeroU64::new(512 * 1024 * 1024).unwrap(),
        cpu_time_limit_ms: NonZeroU64::new(30_000).unwrap(),
        max_concurrency: NonZeroU32::new(1).unwrap(),
    }
}

fn object_store(path: &std::path::Path) -> EncryptedFilesystemObjectStore {
    EncryptedFilesystemObjectStore::open(
        path,
        [13; 32],
        ObjectStoreQuota {
            max_objects_per_project: NonZeroU64::new(100).unwrap(),
            max_bytes_per_project: NonZeroU64::new(16 * 1024 * 1024).unwrap(),
            max_object_bytes: NonZeroU64::new(1024 * 1024).unwrap(),
        },
        true,
    )
    .unwrap()
}

struct BoundedReadTracker<'a> {
    inner: &'a EncryptedFilesystemObjectStore,
    reads: Cell<usize>,
}

impl<'a> BoundedReadTracker<'a> {
    fn new(inner: &'a EncryptedFilesystemObjectStore) -> Self {
        Self {
            inner,
            reads: Cell::new(0),
        }
    }
}

impl crate::distributed::object_store::TenantObjectStore for BoundedReadTracker<'_> {
    type Error = crate::distributed::object_store::ObjectStoreError;

    fn protection(&self) -> crate::distributed::object_store::ObjectStoreProtection {
        self.inner.protection()
    }

    fn put_verified(
        &mut self,
        _project: &RemoteProjectRef,
        _content_identity: &Digest,
        _content: &[u8],
    ) -> Result<crate::distributed::object_store::PutObjectResult, Self::Error> {
        unreachable!("workers never write source objects")
    }

    fn read_verified(
        &self,
        _object: &crate::distributed::identity::TenantObjectRef,
    ) -> Result<Vec<u8>, Self::Error> {
        panic!("worker source reads must use the bounded object boundary")
    }

    fn read_verified_bounded(
        &self,
        object: &crate::distributed::identity::TenantObjectRef,
        max_bytes: u64,
        deadline: std::time::Instant,
    ) -> Result<crate::distributed::object_store::BoundedObjectRead, Self::Error> {
        self.reads.set(self.reads.get().saturating_add(1));
        self.inner
            .read_verified_bounded(object, max_bytes, deadline)
    }
}

fn fact_store(path: &std::path::Path) -> SqliteFactBatchStore {
    SqliteFactBatchStore::open(
        path,
        path.parent().unwrap().join("jobs.db"),
        [29; 32],
        FactStoreQuota {
            max_batches_per_project: NonZeroU64::new(1_000).unwrap(),
            max_bytes_per_project: NonZeroU64::new(32 * 1024 * 1024).unwrap(),
            max_batch_bytes: NonZeroU64::new(1024 * 1024).unwrap(),
        },
        true,
    )
    .unwrap()
}

struct DelayedFactStore {
    inner: SqliteFactBatchStore,
    delay: Duration,
    delay_put: bool,
    delay_progress: bool,
    put_calls: u64,
    progress_calls: Cell<u64>,
}

impl DelayedFactStore {
    fn new(
        inner: SqliteFactBatchStore,
        delay: Duration,
        delay_put: bool,
        delay_progress: bool,
    ) -> Self {
        Self {
            inner,
            delay,
            delay_put,
            delay_progress,
            put_calls: 0,
            progress_calls: Cell::new(0),
        }
    }
}

impl FactBatchStore for DelayedFactStore {
    type Error = crate::distributed::fact_store_sqlite::FactStoreError;

    fn protection(&self) -> FactStoreProtection {
        self.inner.protection()
    }

    fn put(&mut self, batch: &FactBatch) -> Result<PutFactBatchOutcome, Self::Error> {
        let outcome = self.inner.put(batch)?;
        self.put_calls = self.put_calls.saturating_add(1);
        if self.delay_put && self.put_calls == 1 {
            std::thread::sleep(self.delay);
        }
        Ok(outcome)
    }

    fn progress(&self, job: &IndexJobRef) -> Result<FactBatchProgress, Self::Error> {
        let progress = self.inner.progress(job)?;
        let calls = self.progress_calls.get().saturating_add(1);
        self.progress_calls.set(calls);
        if self.delay_progress && calls == 1 {
            std::thread::sleep(self.delay);
        }
        Ok(progress)
    }

    fn load_for_ingestion(&self, job: &IndexJobRef) -> Result<Vec<FactBatch>, Self::Error> {
        self.inner.load_for_ingestion(job)
    }
}

fn coordinator(path: &std::path::Path) -> SqliteIndexJobCoordinator {
    SqliteIndexJobCoordinator::open(
        path,
        CoordinatorLimits {
            max_attempts: NonZeroU32::new(3).unwrap(),
            lease_ttl_ms: NonZeroU64::new(60_000).unwrap(),
            max_job_duration_ms: NonZeroU64::new(120_000).unwrap(),
        },
    )
    .unwrap()
}

fn running_job(
    coordinator: &mut SqliteIndexJobCoordinator,
    kind: IndexJobKind,
    input: IndexInputRef,
    semantics: IndexSemantics,
) -> IndexJobRecord {
    let now = Utc::now();
    let spec = IndexJobSpec::new(kind, input, semantics).unwrap();
    coordinator
        .submit(
            &SubmitIndexJobRequest {
                protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                request_id: RequestId::new("submit").unwrap(),
                project: remote_project(),
                job: spec,
            },
            now,
        )
        .unwrap();
    let leased = coordinator
        .claim(
            &ClaimIndexJobRequest {
                protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                request_id: RequestId::new("claim").unwrap(),
                project: remote_project(),
                kind,
                worker_id: WorkerId::new("worker-a").unwrap(),
            },
            now,
        )
        .unwrap()
        .unwrap();
    let lease = leased.lease.as_ref().unwrap();
    coordinator
        .start(
            &AdvanceIndexJobRequest {
                protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                request_id: RequestId::new("start").unwrap(),
                job: leased.job.clone(),
                worker_id: lease.worker_id.clone(),
                lease_generation: lease.generation,
            },
            now,
        )
        .unwrap()
}

fn execution_request(job: IndexJobRecord) -> ExecuteIndexJobRequest {
    let lease = job.lease.as_ref().unwrap();
    ExecuteIndexJobRequest {
        protocol_version: DISTRIBUTED_WORKER_PROTOCOL_VERSION,
        worker_id: lease.worker_id.clone(),
        lease_generation: lease.generation,
        job,
        sandbox: sandbox(),
    }
}

#[test]
fn repository_worker_recovers_the_pinned_builtin_extractor_subset() {
    let identities = builtin_extractor_identities();
    let rust_only = identities
        .into_iter()
        .filter(|identity| identity.id.as_str() == "builtin.rust-syntax")
        .collect::<Vec<_>>();
    let selected = repository_extractor_selection(&extractor_set_digest(&rust_only)).unwrap();
    assert!(selected.file_ids.contains("builtin.rust-syntax"));
    assert!(!selected.generic_enabled);
    assert!(!selected.resolver_enabled);
}

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
fn worker_reads_repository_manifests_through_the_object_size_boundary() {
    let repository_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repository_dir.path().join("src")).unwrap();
    std::fs::write(
        repository_dir.path().join("src/lib.rs"),
        b"pub struct Api;\n",
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
    let manifest_bytes = objects
        .read_verified(&manifest.reference.manifest_object)
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
    limits.max_object_bytes = NonZeroU64::new(
        u64::try_from(manifest_bytes.len())
            .unwrap()
            .saturating_sub(1),
    )
    .unwrap();
    let mut facts = fact_store(&storage_dir.path().join("facts.db"));

    assert_eq!(
        StatelessIndexWorker::new(limits).execute(&request, &jobs, &objects, &mut facts),
        Err(WorkerError::InputLimitExceeded)
    );
}

#[test]
fn repository_worker_extracts_cargo_facts_without_process_execution() {
    let repository_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repository_dir.path().join("src")).unwrap();
    std::fs::write(
        repository_dir.path().join("Cargo.toml"),
        b"[package]\nname = \"remote-app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nserde = \"1\"\n",
    )
    .unwrap();
    std::fs::write(
        repository_dir.path().join("src/lib.rs"),
        b"pub struct Api;\n",
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
    assert_eq!(
        request.sandbox.repository_execution,
        RepositoryExecutionPolicy::Denied
    );
    let mut facts = fact_store(&storage_dir.path().join("cargo-facts.db"));
    let bounded_objects = BoundedReadTracker::new(&objects);

    StatelessIndexWorker::new(worker_limits())
        .execute(&request, &jobs, &bounded_objects, &mut facts)
        .unwrap();
    assert_eq!(bounded_objects.reads.get(), manifest.body.files.len() + 1);
    let batches = facts.load_for_ingestion(&request.job.job).unwrap();
    let mut node_kinds = std::collections::BTreeSet::new();
    let mut diagnostic_codes = std::collections::BTreeSet::new();
    for batch in batches {
        let FactBatchPayload::RepositoryGraph {
            nodes, diagnostics, ..
        } = batch.payload
        else {
            panic!("repository worker emitted a memory fact batch");
        };
        node_kinds.extend(nodes.into_iter().map(|node| node.kind));
        diagnostic_codes.extend(
            diagnostics
                .into_iter()
                .map(|diagnostic| diagnostic.code.as_str().to_string()),
        );
    }

    assert!(node_kinds.contains("cargo_package"));
    assert!(node_kinds.contains("declared_dependency"));
    assert!(!diagnostic_codes.contains("cargo.parser_unavailable"));
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
    limits.max_job_duration_ms = NonZeroU64::new(1_000).unwrap();
    let worker = StatelessIndexWorker::new(limits);
    let delay = Duration::from_millis(1_100);

    let mut delayed_put = DelayedFactStore::new(
        fact_store(&storage_dir.path().join("delayed-put-facts.db")),
        delay,
        true,
        false,
    );
    assert_eq!(
        worker.execute(&request, &jobs, &objects, &mut delayed_put),
        Err(WorkerError::DeadlineExceeded)
    );
    assert_eq!(delayed_put.put_calls, 1);

    let mut delayed_progress = DelayedFactStore::new(
        fact_store(&storage_dir.path().join("delayed-progress-facts.db")),
        delay,
        false,
        true,
    );
    assert_eq!(
        worker.execute(&request, &jobs, &objects, &mut delayed_progress),
        Err(WorkerError::DeadlineExceeded)
    );
    assert_eq!(delayed_progress.progress_calls.get(), 1);
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

#[test]
fn repository_worker_applies_one_diagnostic_budget_to_the_whole_job() {
    let repository_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repository_dir.path().join("src")).unwrap();
    for index in 0..4 {
        std::fs::write(
            repository_dir.path().join(format!("src/broken_{index}.rs")),
            b"pub fn broken( {\n",
        )
        .unwrap();
    }
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
    limits.max_diagnostics = NonZeroU64::new(1).unwrap();
    let mut first_facts = fact_store(&storage_dir.path().join("diagnostic-facts.db"));
    let first = StatelessIndexWorker::new(limits.clone())
        .execute(&request, &jobs, &objects, &mut first_facts)
        .unwrap();
    let diagnostics = first_facts
        .load_for_ingestion(&request.job.job)
        .unwrap()
        .iter()
        .map(|batch| match &batch.payload {
            FactBatchPayload::RepositoryGraph { diagnostics, .. } => diagnostics.len(),
            FactBatchPayload::ProjectMemory { .. } => 0,
        })
        .sum::<usize>();
    assert_eq!(diagnostics, 1);

    // The generic and language extractors intentionally emit overlapping facts
    // that the merger de-duplicates. This leaves ten fact slots above the final
    // payload for those overlaps, but no room for one diagnostic per malformed
    // file. A per-invocation diagnostic allowance would exceed this boundary.
    limits.max_total_facts = NonZeroU64::new(first.emitted_facts + 10).unwrap();
    let mut tightly_bounded_facts =
        fact_store(&storage_dir.path().join("diagnostic-total-facts.db"));
    let bounded = StatelessIndexWorker::new(limits)
        .execute(&request, &jobs, &objects, &mut tightly_bounded_facts)
        .unwrap();
    assert_eq!(bounded.emitted_facts, first.emitted_facts);
}

struct FakeMemorySource {
    manifest: AuthorizedSourceManifest,
    content: Vec<u8>,
}

impl MemorySource for FakeMemorySource {
    type Error = anyhow::Error;

    fn manifest(&self) -> Result<AuthorizedSourceManifest, Self::Error> {
        Ok(self.manifest.clone())
    }

    fn read_verified(
        &self,
        source: &AuthorizedSourceDescriptor,
    ) -> Result<MemorySourceContent, Self::Error> {
        anyhow::ensure!(self.manifest.sources.contains(source));
        Ok(MemorySourceContent {
            bytes: self.content.clone(),
        })
    }

    fn revalidate(&self, manifest: &AuthorizedSourceManifest) -> Result<(), Self::Error> {
        anyhow::ensure!(manifest == &self.manifest);
        Ok(())
    }
}

#[test]
fn memory_worker_extracts_only_sanitized_manifest_objects() {
    let content = b"# Example\n\nPrivate body.\n\n- [x] #5.3 Worker\n\nID: rg5.3\n".to_vec();
    let parsed = parse_spec_memory(std::str::from_utf8(&content).unwrap());
    let policy = MemoryPolicy::default();
    let descriptor = AuthorizedSourceDescriptor {
        project: local_project(),
        category: MemorySourceCategory::SpecificationStructure,
        locator: MemorySourceLocator::TrackedFile {
            path: RepoPath::new("docs/specs/example.md").unwrap(),
        },
        fingerprint: memory_digest(&parsed.structure),
        byte_len: content.len() as u64,
    };
    let mut local_manifest = AuthorizedSourceManifest {
        project: local_project(),
        policy_digest: policy.digest(),
        source_set_digest: Digest::new("sha256", "11").unwrap(),
        extractor_set_digest: built_in_extractor_set_digest(),
        sources: vec![descriptor],
    };
    local_manifest.source_set_digest = local_manifest.computed_source_set_digest().unwrap();
    let source = FakeMemorySource {
        manifest: local_manifest,
        content,
    };
    let storage_dir = tempfile::tempdir().unwrap();
    let mut objects = object_store(&storage_dir.path().join("objects"));
    let manifest = package_memory_source(
        &source,
        remote_project(),
        &policy,
        packaging_limits(),
        &mut objects,
    )
    .unwrap();
    let mut jobs = coordinator(&storage_dir.path().join("jobs.db"));
    let running = running_job(
        &mut jobs,
        IndexJobKind::ProjectMemory,
        IndexInputRef::Memory(manifest.reference.clone()),
        IndexSemantics {
            semantic_config_digest: policy.digest(),
            model_version: NonZeroU32::new(MEMORY_MODEL_VERSION).unwrap(),
            extractor_set_digest: built_in_extractor_set_digest(),
        },
    );
    let request = execution_request(running);
    let mut per_source_facts = fact_store(&storage_dir.path().join("memory-per-source-facts.db"));
    let mut per_source_limited = worker_limits();
    per_source_limited.max_facts_per_source = NonZeroU64::new(2).unwrap();
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
    let mut total_facts = fact_store(&storage_dir.path().join("memory-total-facts.db"));
    let mut total_limited = worker_limits();
    total_limited.max_total_facts = NonZeroU64::new(2).unwrap();
    assert_eq!(
        StatelessIndexWorker::new(total_limited).execute(
            &request,
            &jobs,
            &objects,
            &mut total_facts,
        ),
        Err(WorkerError::OutputLimitExceeded)
    );
    assert!(
        total_facts
            .load_for_ingestion(&request.job.job)
            .unwrap()
            .is_empty()
    );
    let mut facts = fact_store(&storage_dir.path().join("facts.db"));
    let bounded_objects = BoundedReadTracker::new(&objects);
    let outcome = StatelessIndexWorker::new(worker_limits())
        .execute(&request, &jobs, &bounded_objects, &mut facts)
        .unwrap();
    assert_eq!(bounded_objects.reads.get(), manifest.body.sources.len() + 1);
    let encoded =
        serde_json::to_string(&facts.load_for_ingestion(&request.job.job).unwrap()).unwrap();
    assert!(outcome.emitted_facts > 0);
    assert!(encoded.contains("rg5.3"));
    assert!(!encoded.contains("Private body"));

    let mut forged = manifest;
    forged.body.sources[0].source_fingerprint = Digest::new("sha256", "00").unwrap();
    let mut forged_sources = AuthorizedSourceManifest {
        project: forged.body.project_identity.clone(),
        policy_digest: forged.body.memory_policy_digest.clone(),
        source_set_digest: forged.body.source_set_digest.clone(),
        extractor_set_digest: forged.body.extractor_set_digest.clone(),
        sources: forged
            .body
            .sources
            .iter()
            .map(|source| AuthorizedSourceDescriptor {
                project: forged.body.project_identity.clone(),
                category: source.category,
                locator: source.locator.clone(),
                fingerprint: source.source_fingerprint.clone(),
                byte_len: source.sanitized_byte_len,
            })
            .collect(),
    };
    forged_sources.source_set_digest = forged_sources.computed_source_set_digest().unwrap();
    let expected_revision_id = forged_sources.revision_id().unwrap();
    forged.body.source_set_digest = forged_sources.source_set_digest;
    let encoded_manifest = serde_json::to_vec(&forged.body).unwrap();
    let manifest_digest = Digest::new(
        "sha256",
        Sha256::digest(&encoded_manifest)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
    )
    .unwrap();
    forged.reference.manifest_id = MemoryManifestId::new(manifest_digest.value()).unwrap();
    forged.reference.manifest_digest = manifest_digest.clone();
    forged.reference.expected_revision_id = expected_revision_id;
    forged.reference.manifest_object = objects
        .put_verified(
            &forged.reference.project,
            &manifest_digest,
            &encoded_manifest,
        )
        .unwrap()
        .object;
    forged.validate::<(), ()>().unwrap();

    let running = running_job(
        &mut jobs,
        IndexJobKind::ProjectMemory,
        IndexInputRef::Memory(forged.reference),
        IndexSemantics {
            semantic_config_digest: policy.digest(),
            model_version: NonZeroU32::new(MEMORY_MODEL_VERSION).unwrap(),
            extractor_set_digest: built_in_extractor_set_digest(),
        },
    );
    let forged_request = execution_request(running);
    let mut forged_facts = fact_store(&storage_dir.path().join("forged-memory-facts.db"));
    assert_eq!(
        StatelessIndexWorker::new(worker_limits()).execute(
            &forged_request,
            &jobs,
            &objects,
            &mut forged_facts,
        ),
        Err(WorkerError::InvalidInput)
    );
    assert!(
        forged_facts
            .load_for_ingestion(&forged_request.job.job)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn memory_worker_emits_a_snapshot_pinned_repository_link_set() {
    let content = b"# Example\n\n## Outcome\n\nDelivered.\n\n### Decisions\n\nUse `path:src/lib.rs` and `symbol:important-type`.\n".to_vec();
    let parsed = parse_spec_memory(std::str::from_utf8(&content).unwrap());
    let policy = MemoryPolicy::default();
    let descriptor = AuthorizedSourceDescriptor {
        project: local_project(),
        category: MemorySourceCategory::ApprovedOutcome,
        locator: MemorySourceLocator::TrackedFile {
            path: RepoPath::new("docs/specs/example.md").unwrap(),
        },
        fingerprint: memory_digest(parsed.outcome.as_ref().unwrap()),
        byte_len: content.len() as u64,
    };
    let mut local_manifest = AuthorizedSourceManifest {
        project: local_project(),
        policy_digest: policy.digest(),
        source_set_digest: Digest::new("sha256", "11").unwrap(),
        extractor_set_digest: built_in_extractor_set_digest(),
        sources: vec![descriptor],
    };
    local_manifest.source_set_digest = local_manifest.computed_source_set_digest().unwrap();
    let source = FakeMemorySource {
        manifest: local_manifest,
        content,
    };
    let storage_dir = tempfile::tempdir().unwrap();
    let mut objects = object_store(&storage_dir.path().join("objects"));
    let mut manifest = package_memory_source(
        &source,
        remote_project(),
        &policy,
        packaging_limits(),
        &mut objects,
    )
    .unwrap();
    let graph_ref = RemoteGraphSnapshotRef {
        repository: remote_repository(),
        snapshot_id: SnapshotId::new("graph-snapshot").unwrap(),
    };
    manifest.reference.repository_snapshot = Some(graph_ref.clone());
    let graph = StoredRemoteGraphSnapshot {
        record: RemoteGraphSnapshotRecord {
            snapshot: graph_ref.clone(),
            repository_identity: local_repository(),
            job: IndexJobRef {
                project: remote_project(),
                job_id: crate::distributed::identity::IndexJobId::new("graph-job").unwrap(),
                kind: IndexJobKind::RepositoryGraph,
            },
            build_id: BuildId::new("graph-build").unwrap(),
            extractor_set_digest: Digest::new("sha256", "22").unwrap(),
            fact_set_digest: Digest::new("sha256", "33").unwrap(),
            counts: RemoteFactCounts {
                primary: 1,
                relationships: 0,
                diagnostics: 0,
            },
            completed_at: Utc::now(),
        },
        nodes: vec![GraphNode {
            snapshot_id: graph_ref.snapshot_id.clone(),
            id: NodeId::new("important-node").unwrap(),
            kind: "symbol".to_string(),
            semantic_key: Some(SemanticKey::new("important-type").unwrap()),
            provenance: FactProvenance {
                extractor: ExtractorIdentity {
                    id: ExtractorId::new("test.graph").unwrap(),
                    version: "1".to_string(),
                    contract_version: 1,
                },
                evidence: Some(SourceEvidence {
                    path: RepoPath::new("src/lib.rs").unwrap(),
                    content_identity: Digest::new("sha256", "44").unwrap(),
                    span: None,
                }),
                resolution: ResolutionState::Resolved,
                confidence: Confidence::Exact,
            },
            properties: BTreeMap::new(),
        }],
        edges: Vec::new(),
        diagnostics: Vec::new(),
    };
    let mut jobs = coordinator(&storage_dir.path().join("jobs.db"));
    let running = running_job(
        &mut jobs,
        IndexJobKind::ProjectMemory,
        IndexInputRef::Memory(manifest.reference.clone()),
        IndexSemantics {
            semantic_config_digest: policy.digest(),
            model_version: NonZeroU32::new(MEMORY_MODEL_VERSION).unwrap(),
            extractor_set_digest: built_in_extractor_set_digest(),
        },
    );
    let request = execution_request(running);
    let mut facts = fact_store(&storage_dir.path().join("linked-facts.db"));

    StatelessIndexWorker::new(worker_limits())
        .execute_with_repository_snapshot(&request, &jobs, &objects, &mut facts, Some(&graph))
        .unwrap();

    let batches = facts.load_for_ingestion(&request.job.job).unwrap();
    assert!(batches.iter().all(|batch| {
        matches!(
            &batch.header.target,
            FactTarget::ProjectMemory {
                repository_links: Some(target),
                ..
            } if target.graph == graph_ref
                && target.link_set.repository_snapshot_id.as_ref() == Some(&graph_ref.snapshot_id)
        )
    }));
    let relationships = batches
        .iter()
        .flat_map(|batch| match &batch.payload {
            FactBatchPayload::ProjectMemory { relationships, .. } => relationships.as_slice(),
            FactBatchPayload::RepositoryGraph { .. } => &[],
        })
        .collect::<Vec<_>>();
    assert!(relationships.iter().any(|relationship| matches!(
        relationship.target,
        MemoryRelationshipTarget::RepositoryPath {
            snapshot_id: Some(ref snapshot_id),
            ..
        } if snapshot_id == &graph_ref.snapshot_id
    )));
    assert!(relationships.iter().any(|relationship| matches!(
        relationship.target,
        MemoryRelationshipTarget::RepositoryNode {
            ref snapshot_id,
            ref node_id,
            ..
        } if snapshot_id == &graph_ref.snapshot_id && node_id.as_str() == "important-node"
    )));
}
