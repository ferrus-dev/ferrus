use super::*;
use crate::{
    distributed::{
        coordinator::{AdvanceIndexJobRequest, ClaimIndexJobRequest},
        coordinator_sqlite::{CoordinatorLimits, SqliteIndexJobCoordinator},
        fact_store_sqlite::{FactStoreQuota, SqliteFactBatchStore},
        identity::{
            RemoteProjectId, RemoteProjectRef, RemoteRepositoryId, RemoteRepositoryRef, RequestId,
            TenantId,
        },
        object_store::{EncryptedFilesystemObjectStore, ObjectStoreQuota},
        protocol::{
            CancelIndexJobRequest, IndexJobKind, IndexJobSpec, IndexSemantics,
            SubmitIndexJobRequest,
        },
        source::{
            PackagingLimits, RepositoryPackagingPolicy, package_memory_source,
            package_repository_source,
        },
    },
    project_memory::{
        documents::parse_spec_memory,
        domain::{
            AuthorizedSourceDescriptor, MemorySourceCategory, MemorySourceLocator, ProjectId,
            ProjectNamespace, ProjectRef,
        },
        extractors::canonical_digest as memory_digest,
        policy::MemoryPolicy,
        ports::{MemorySource, MemorySourceContent},
    },
    repository_graph::{
        config::RepositoryGraphConfig,
        domain::{Digest, RepoPath, RepositoryId, RepositoryNamespace, RepositoryRef},
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

fn fact_store(path: &std::path::Path) -> SqliteFactBatchStore {
    SqliteFactBatchStore::open(
        path,
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
fn repository_worker_is_deterministic_and_reuses_durable_batches() {
    let repository_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repository_dir.path().join("src")).unwrap();
    std::fs::write(
        repository_dir.path().join("src/lib.rs"),
        b"pub struct Api;\nimpl Api { pub fn run(&self) {} }\n",
    )
    .unwrap();
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
    let mut facts = fact_store(&storage_dir.path().join("facts.db"));
    let outcome = StatelessIndexWorker::new(worker_limits())
        .execute(&request, &jobs, &objects, &mut facts)
        .unwrap();
    let encoded =
        serde_json::to_string(&facts.load_for_ingestion(&request.job.job).unwrap()).unwrap();
    assert!(outcome.emitted_facts > 0);
    assert!(encoded.contains("rg5.3"));
    assert!(!encoded.contains("Private body"));
}
