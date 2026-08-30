use super::*;
use crate::{
    distributed::{
        DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        coordinator::{AdvanceIndexJobRequest, ClaimIndexJobRequest, ReclaimIndexJobsRequest},
        identity::{
            ObjectId, RemoteProjectId, RemoteRepositoryId, RepositoryManifestId, RequestId,
            TenantId, TenantObjectRef,
        },
        protocol::{IndexInputRef, IndexSemantics},
    },
    repository_graph::domain::{
        Digest, RepositoryId, RepositoryNamespace, RepositoryRef, SnapshotId,
    },
};

fn digest(value: &str) -> Digest {
    Digest::new("sha256", value).unwrap()
}

fn project(tenant: &str) -> super::super::identity::RemoteProjectRef {
    super::super::identity::RemoteProjectRef {
        tenant_id: TenantId::new(tenant).unwrap(),
        project_id: RemoteProjectId::new("project").unwrap(),
    }
}

fn submit_request(tenant: &str) -> SubmitIndexJobRequest {
    let project = project(tenant);
    let input = IndexInputRef::Repository(super::super::identity::RepositoryManifestRef {
        repository: super::super::identity::RemoteRepositoryRef {
            project: project.clone(),
            repository_id: RemoteRepositoryId::new("repository").unwrap(),
        },
        repository_identity: RepositoryRef {
            namespace: RepositoryNamespace::new("remote:test").unwrap(),
            repository_id: RepositoryId::new("root").unwrap(),
        },
        manifest_id: RepositoryManifestId::new("manifest").unwrap(),
        manifest_digest: digest("11"),
        source_policy_digest: digest("22"),
        expected_snapshot_id: SnapshotId::new("snapshot-input").unwrap(),
        manifest_object: TenantObjectRef {
            project: project.clone(),
            object_id: ObjectId::new("11").unwrap(),
            content_identity: digest("11"),
        },
    });
    let job = IndexJobSpec::new(
        IndexJobKind::RepositoryGraph,
        input,
        IndexSemantics {
            semantic_config_digest: digest("33"),
            model_version: NonZeroU32::new(1).unwrap(),
            extractor_set_digest: digest("44"),
        },
    )
    .unwrap();
    SubmitIndexJobRequest {
        protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        request_id: RequestId::new("submit").unwrap(),
        project,
        job,
    }
}

fn limits(max_attempts: u32) -> CoordinatorLimits {
    CoordinatorLimits {
        max_attempts: NonZeroU32::new(max_attempts).unwrap(),
        lease_ttl_ms: NonZeroU64::new(1_000).unwrap(),
        max_job_duration_ms: NonZeroU64::new(60_000).unwrap(),
    }
}

fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

fn claim_request(tenant: &str, worker: &str) -> ClaimIndexJobRequest {
    ClaimIndexJobRequest {
        protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        request_id: RequestId::new(format!("claim-{worker}")).unwrap(),
        project: project(tenant),
        kind: IndexJobKind::RepositoryGraph,
        worker_id: WorkerId::new(worker).unwrap(),
    }
}

fn advance(record: &IndexJobRecord) -> AdvanceIndexJobRequest {
    let lease = record.lease.as_ref().unwrap();
    AdvanceIndexJobRequest {
        protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        request_id: RequestId::new("advance").unwrap(),
        job: record.job.clone(),
        worker_id: lease.worker_id.clone(),
        lease_generation: lease.generation,
    }
}

#[test]
fn repeated_submission_converges_and_survives_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("coordinator.db");
    let request = submit_request("tenant-a");
    let first = {
        let mut coordinator = SqliteIndexJobCoordinator::open(&path, limits(3)).unwrap();
        let first = coordinator.submit(&request, now()).unwrap();
        let repeated = coordinator.submit(&request, now()).unwrap();
        assert_eq!(first, repeated);
        first
    };
    let coordinator = SqliteIndexJobCoordinator::open(&path, limits(3)).unwrap();
    let inspected = coordinator
        .inspect(&InspectIndexJobRequest {
            protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            request_id: RequestId::new("inspect").unwrap(),
            job: first.job.clone(),
        })
        .unwrap()
        .unwrap();
    assert_eq!(inspected, first);
    assert_eq!(inspected.state, IndexJobState::Queued);
}

#[test]
fn bounded_inspection_stops_at_the_sqlite_lock_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("coordinator.db");
    let mut coordinator = SqliteIndexJobCoordinator::open(&path, limits(3)).unwrap();
    let submitted = coordinator
        .submit(&submit_request("tenant-a"), now())
        .unwrap();
    coordinator
        .connection
        .query_row("PRAGMA journal_mode = DELETE", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap();
    let blocker = Connection::open(&path).unwrap();
    blocker
        .execute_batch(
            "BEGIN EXCLUSIVE;
             UPDATE distributed_index_jobs SET updated_at_ms = updated_at_ms;",
        )
        .unwrap();
    let started = Instant::now();
    let deadline = started + std::time::Duration::from_millis(25);

    let result = coordinator.inspect_bounded(
        &InspectIndexJobRequest {
            protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            request_id: RequestId::new("bounded-inspect").unwrap(),
            job: submitted.job,
        },
        deadline,
    );

    assert!(matches!(result, Err(CoordinatorError::ReadBudgetExceeded)));
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    blocker.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn project_deletion_tombstone_rejects_new_job_submissions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("coordinator.db");
    let mut coordinator = SqliteIndexJobCoordinator::open(&path, limits(3)).unwrap();
    let project = project("tenant-a");
    coordinator
        .connection
        .execute(
            "INSERT INTO project_deletion_tombstones (
                 tenant_id, project_id, deletion_id, created_at_ms
             ) VALUES (?1, ?2, 'delete-project', ?3)",
            params![
                project.tenant_id.as_str(),
                project.project_id.as_str(),
                now().timestamp_millis()
            ],
        )
        .unwrap();

    assert!(matches!(
        coordinator.submit(&submit_request("tenant-a"), now()),
        Err(CoordinatorError::ProjectDeleted)
    ));
}

#[test]
fn concurrent_duplicate_submissions_create_one_durable_job() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("coordinator.db");
    SqliteIndexJobCoordinator::open(&path, limits(3)).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles = (0..2)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut coordinator = SqliteIndexJobCoordinator::open(path, limits(3)).unwrap();
                barrier.wait();
                coordinator
                    .submit(&submit_request("tenant-a"), now())
                    .unwrap()
                    .job
                    .job_id
            })
        })
        .collect::<Vec<_>>();
    let jobs = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(jobs[0], jobs[1]);
}

#[test]
fn lease_heartbeat_and_state_transitions_are_generation_guarded() {
    let directory = tempfile::tempdir().unwrap();
    let mut coordinator =
        SqliteIndexJobCoordinator::open(directory.path().join("jobs.db"), limits(3)).unwrap();
    coordinator
        .submit(&submit_request("tenant-a"), now())
        .unwrap();
    let leased = coordinator
        .claim(&claim_request("tenant-a", "worker-a"), now())
        .unwrap()
        .unwrap();
    assert_eq!(leased.state, IndexJobState::Leased);
    assert!(
        coordinator
            .claim(&claim_request("tenant-a", "worker-b"), now())
            .unwrap()
            .is_none()
    );
    let running = coordinator.start(&advance(&leased), now()).unwrap();
    let lease = running.lease.as_ref().unwrap();
    let heartbeat_at = now() + Duration::milliseconds(500);
    let heartbeat = coordinator
        .heartbeat(
            &HeartbeatJobRequest {
                protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                request_id: RequestId::new("heartbeat").unwrap(),
                job: running.job.clone(),
                worker_id: lease.worker_id.clone(),
                lease_generation: lease.generation,
            },
            heartbeat_at,
        )
        .unwrap();
    assert!(heartbeat.lease.unwrap().expires_at > lease.expires_at);
    let publishing = coordinator
        .begin_publication(&advance(&running), now())
        .unwrap();
    let complete = coordinator.complete(&advance(&publishing), now()).unwrap();
    assert_eq!(complete.state, IndexJobState::Complete);
    assert!(complete.lease.is_none());
    assert!(complete.failure_code.is_none());
}

#[test]
fn expired_leases_requeue_then_stop_at_the_attempt_limit() {
    let directory = tempfile::tempdir().unwrap();
    let mut coordinator =
        SqliteIndexJobCoordinator::open(directory.path().join("jobs.db"), limits(2)).unwrap();
    coordinator
        .submit(&submit_request("tenant-a"), now())
        .unwrap();
    let first = coordinator
        .claim(&claim_request("tenant-a", "worker-a"), now())
        .unwrap()
        .unwrap();
    let after_expiry = now() + Duration::seconds(2);
    let reclaimed = coordinator
        .reclaim(
            &ReclaimIndexJobsRequest {
                protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                request_id: RequestId::new("reclaim-one").unwrap(),
                project: project("tenant-a"),
            },
            after_expiry,
        )
        .unwrap();
    assert_eq!(reclaimed.requeued, 1);
    assert!(matches!(
        coordinator.start(&advance(&first), after_expiry),
        Err(CoordinatorError::LeaseLost)
    ));

    let second = coordinator
        .claim(&claim_request("tenant-a", "worker-b"), after_expiry)
        .unwrap()
        .unwrap();
    assert_eq!(second.attempt.get(), 2);
    assert!(second.lease.as_ref().unwrap().generation.get() > 1);
    let final_reclaim = coordinator
        .reclaim(
            &ReclaimIndexJobsRequest {
                protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                request_id: RequestId::new("reclaim-two").unwrap(),
                project: project("tenant-a"),
            },
            after_expiry + Duration::seconds(2),
        )
        .unwrap();
    assert_eq!(final_reclaim.failed, 1);
    let record = coordinator
        .inspect(&InspectIndexJobRequest {
            protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            request_id: RequestId::new("inspect-failed").unwrap(),
            job: second.job,
        })
        .unwrap()
        .unwrap();
    assert_eq!(record.state, IndexJobState::Failed);
    assert_eq!(record.failure_code.unwrap().as_str(), "job.attempt_limit");
}

#[test]
fn total_job_deadline_is_enforced_before_claim() {
    let directory = tempfile::tempdir().unwrap();
    let mut coordinator = SqliteIndexJobCoordinator::open(
        directory.path().join("jobs.db"),
        CoordinatorLimits {
            max_attempts: NonZeroU32::new(3).unwrap(),
            lease_ttl_ms: NonZeroU64::new(1_000).unwrap(),
            max_job_duration_ms: NonZeroU64::new(500).unwrap(),
        },
    )
    .unwrap();
    let submitted = coordinator
        .submit(&submit_request("tenant-a"), now())
        .unwrap();
    assert!(
        coordinator
            .claim(
                &claim_request("tenant-a", "worker-a"),
                now() + Duration::seconds(1),
            )
            .unwrap()
            .is_none()
    );
    let failed = coordinator
        .inspect(&InspectIndexJobRequest {
            protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            request_id: RequestId::new("inspect-timeout").unwrap(),
            job: submitted.job,
        })
        .unwrap()
        .unwrap();
    assert_eq!(failed.state, IndexJobState::Failed);
    assert_eq!(failed.failure_code.unwrap().as_str(), "job.timeout");
}

#[test]
fn cancellation_revokes_the_lease_and_prevents_publication() {
    let directory = tempfile::tempdir().unwrap();
    let mut coordinator =
        SqliteIndexJobCoordinator::open(directory.path().join("jobs.db"), limits(3)).unwrap();
    coordinator
        .submit(&submit_request("tenant-a"), now())
        .unwrap();
    let leased = coordinator
        .claim(&claim_request("tenant-a", "worker-a"), now())
        .unwrap()
        .unwrap();
    let request = CancelIndexJobRequest {
        protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        request_id: RequestId::new("cancel").unwrap(),
        job: leased.job.clone(),
        expected_state: Some(IndexJobState::Leased),
    };
    let cancelled = coordinator.cancel(&request, now()).unwrap();
    assert_eq!(cancelled.state, IndexJobState::Cancelled);
    assert!(cancelled.cancellation_requested);
    assert_eq!(coordinator.cancel(&request, now()).unwrap(), cancelled);
    assert!(matches!(
        coordinator.start(&advance(&leased), now()),
        Err(CoordinatorError::LeaseLost)
    ));
}

#[test]
fn retryable_worker_failure_requeues_with_a_bounded_code() {
    let directory = tempfile::tempdir().unwrap();
    let mut coordinator =
        SqliteIndexJobCoordinator::open(directory.path().join("jobs.db"), limits(2)).unwrap();
    coordinator
        .submit(&submit_request("tenant-a"), now())
        .unwrap();
    let leased = coordinator
        .claim(&claim_request("tenant-a", "worker-a"), now())
        .unwrap()
        .unwrap();
    let running = coordinator.start(&advance(&leased), now()).unwrap();
    let lease = running.lease.as_ref().unwrap();
    let requeued = coordinator
        .fail(
            &FailIndexJobRequest {
                protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                request_id: RequestId::new("retry").unwrap(),
                job: running.job.clone(),
                worker_id: lease.worker_id.clone(),
                lease_generation: lease.generation,
                failure_code: IndexJobFailureCode::new("object.timeout").unwrap(),
                retryable: true,
            },
            now(),
        )
        .unwrap();
    assert_eq!(requeued.state, IndexJobState::Queued);
    assert_eq!(
        requeued.failure_code.as_ref().unwrap().as_str(),
        "object.timeout"
    );
    let second = coordinator
        .claim(&claim_request("tenant-a", "worker-b"), now())
        .unwrap()
        .unwrap();
    assert_eq!(second.attempt.get(), 2);
    assert!(second.failure_code.is_none());
}

#[test]
fn job_lookup_and_claim_are_tenant_scoped() {
    let directory = tempfile::tempdir().unwrap();
    let mut coordinator =
        SqliteIndexJobCoordinator::open(directory.path().join("jobs.db"), limits(3)).unwrap();
    let tenant_a = coordinator
        .submit(&submit_request("tenant-a"), now())
        .unwrap();
    coordinator
        .submit(&submit_request("tenant-b"), now())
        .unwrap();
    let mut foreign_job = tenant_a.job.clone();
    foreign_job.project = project("tenant-c");
    assert!(
        coordinator
            .inspect(&InspectIndexJobRequest {
                protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                request_id: RequestId::new("foreign-inspect").unwrap(),
                job: foreign_job,
            })
            .unwrap()
            .is_none()
    );
    let claimed = coordinator
        .claim(&claim_request("tenant-b", "worker-b"), now())
        .unwrap()
        .unwrap();
    assert_eq!(claimed.job.project, project("tenant-b"));
}

#[test]
fn incompatible_metadata_does_not_create_job_tables() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("jobs.db");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE distributed_coordinator_metadata (
                 singleton INTEGER PRIMARY KEY,
                 schema_version INTEGER NOT NULL
             );
             INSERT INTO distributed_coordinator_metadata VALUES (1, 999);",
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        SqliteIndexJobCoordinator::open(&path, limits(3)),
        Err(CoordinatorError::IncompatibleSchema)
    ));
    let connection = Connection::open(path).unwrap();
    let created = connection
        .query_row(
            "SELECT 1 FROM sqlite_master
             WHERE type = 'table' AND name = 'distributed_index_jobs'",
            [],
            |_| Ok(()),
        )
        .optional()
        .unwrap();
    assert!(created.is_none());
}
