//! Worker tests for immutable input, extraction limits, lease checks, and retry-safe batches.

use std::{
    cell::Cell,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use rusqlite::Connection;
use sha2::{Digest as _, Sha256};

use super::*;
use crate::{
    distributed::{
        coordinator::{AdvanceIndexJobRequest, ClaimIndexJobRequest},
        coordinator_sqlite::{CoordinatorLimits, SqliteIndexJobCoordinator},
        fact_store::{
            FactBatchProgressOutcome, FactBatchStore, FactBatchWriteAuthority, FactStoreProtection,
            PutFactBatchOutcome,
        },
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
        documents::{
            RuntimeRunDocument, RuntimeSourceDocument, RuntimeTaskDocument, parse_spec_memory,
        },
        domain::{
            AuthorizedSourceDescriptor, MemoryRelationshipTarget, MemoryResolutionState,
            MemorySourceCategory, MemorySourceLocator, ProjectId, ProjectNamespace, ProjectRef,
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

fn max_encoded_batch_bytes(batches: &[FactBatch]) -> NonZeroU64 {
    NonZeroU64::new(
        batches
            .iter()
            .map(|batch| serde_json::to_vec(batch).unwrap().len() as u64)
            .max()
            .unwrap(),
    )
    .unwrap()
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

    fn put_verified_batch(
        &mut self,
        _project: &RemoteProjectRef,
        _objects: &[crate::distributed::object_store::VerifiedObject<'_>],
    ) -> Result<Vec<crate::distributed::object_store::PutObjectResult>, Self::Error> {
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

struct ExpiringFactStore {
    inner: SqliteFactBatchStore,
    elapsed_ms: Arc<AtomicU64>,
    expire_put: bool,
    expire_progress: bool,
    put_calls: u64,
    progress_calls: Cell<u64>,
}

impl ExpiringFactStore {
    fn new(
        inner: SqliteFactBatchStore,
        elapsed_ms: Arc<AtomicU64>,
        expire_put: bool,
        expire_progress: bool,
    ) -> Self {
        Self {
            inner,
            elapsed_ms,
            expire_put,
            expire_progress,
            put_calls: 0,
            progress_calls: Cell::new(0),
        }
    }
}

impl FactBatchStore for ExpiringFactStore {
    type Error = crate::distributed::fact_store_sqlite::FactStoreError;

    fn protection(&self) -> FactStoreProtection {
        self.inner.protection()
    }

    fn put(
        &mut self,
        batch: &FactBatch,
        authority: &FactBatchWriteAuthority,
        deadline: Instant,
    ) -> Result<PutFactBatchOutcome, Self::Error> {
        let outcome = self.inner.put(batch, authority, deadline)?;
        self.put_calls = self.put_calls.saturating_add(1);
        if self.expire_put && self.put_calls == 1 {
            self.elapsed_ms.store(u64::MAX, Ordering::Relaxed);
        }

        Ok(outcome)
    }

    fn progress(
        &self,
        job: &IndexJobRef,
        deadline: Instant,
    ) -> Result<FactBatchProgressOutcome, Self::Error> {
        let calls = self.progress_calls.get().saturating_add(1);
        self.progress_calls.set(calls);
        if self.expire_progress && calls == 1 {
            return Ok(FactBatchProgressOutcome::DeadlineExceeded);
        }

        self.inner.progress(job, deadline)
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

struct FakeMemorySource {
    manifest: AuthorizedSourceManifest,
    contents: BTreeMap<MemorySourceCategory, Vec<u8>>,
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
            bytes: self
                .contents
                .get(&source.category)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing fake memory source content"))?,
        })
    }

    fn revalidate(&self, manifest: &AuthorizedSourceManifest) -> Result<(), Self::Error> {
        anyhow::ensure!(manifest == &self.manifest);
        Ok(())
    }
}

#[path = "worker_tests/cancellation.rs"]
mod cancellation_tests;
#[path = "worker_tests/memory.rs"]
mod memory_tests;
#[path = "worker_tests/recovery.rs"]
mod recovery_tests;
#[path = "worker_tests/workspaces.rs"]
mod workspaces_tests;

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
    let first = StatelessIndexWorker::new(limits)
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
