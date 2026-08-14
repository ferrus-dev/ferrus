//! SQLite prototype for idempotent distributed deletion and bounded audits.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use chrono::{DateTime, Utc};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use super::{
    DISTRIBUTED_MAINTENANCE_PROTOCOL_VERSION, DISTRIBUTED_POLICY_VERSION,
    identity::{AuditEventId, DeletionId, RemoteProjectRef, RemoteRepositoryRef, RequestId},
    maintenance::{
        InspectRemoteDeletionRequest, RemoteDeleteRequest, RemoteDeletionResult,
        RemoteMaintenanceApi,
    },
    protocol::{
        DistributedProtocolError, IndexInputRef, IndexJobSpec, RemoteError, RemoteErrorCode,
    },
    publication_sqlite::STORAGE_SCHEMA_VERSION,
    security::{
        AuditCounter, AuditOutcome, AuditRecord, AuditedResource, AuthorizationContext,
        AuthorizationScope, DeleteDataRequest, DeletionState, DeletionTarget, RemotePermission,
        RetentionClass,
    },
};

const MAINTENANCE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum MaintenanceStoreError {
    #[error("distributed maintenance storage is unavailable")]
    Unavailable,
    #[error("distributed maintenance storage schema is incompatible")]
    IncompatibleSchema,
    #[error("distributed maintenance record is inconsistent")]
    IntegrityFailure,
    #[error("distributed maintenance request conflicts with durable state")]
    Conflict,
    #[error("distributed maintenance serialization failed")]
    Serialization,
}

#[derive(Debug, Clone)]
struct StoredDeletion {
    request: DeleteDataRequest,
    state: DeletionState,
    counters: BTreeMap<AuditCounter, u64>,
    audit_event_id: Option<AuditEventId>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Default)]
struct DeletionCounts {
    counters: BTreeMap<AuditCounter, u64>,
}

impl DeletionCounts {
    fn add(&mut self, counter: AuditCounter, value: u64) {
        *self.counters.entry(counter).or_default() = self
            .counters
            .get(&counter)
            .copied()
            .unwrap_or_default()
            .saturating_add(value);
    }

    fn merge_max(&mut self, other: &BTreeMap<AuditCounter, u64>) {
        for (counter, value) in other {
            let stored = self.counters.entry(*counter).or_default();
            *stored = (*stored).max(*value);
        }
    }
}

/// A resumable deletion coordinator over the independent prototype stores.
/// Every step is idempotent, so a retry can continue after a process or store
/// failure without a cross-database transaction.
pub struct SqliteRemoteMaintenance {
    control_path: PathBuf,
    fact_path: PathBuf,
    object_root: PathBuf,
}

impl SqliteRemoteMaintenance {
    pub fn open(
        control_path: impl AsRef<Path>,
        fact_path: impl AsRef<Path>,
        object_root: impl AsRef<Path>,
    ) -> Result<Self, MaintenanceStoreError> {
        let service = Self {
            control_path: control_path.as_ref().to_path_buf(),
            fact_path: fact_path.as_ref().to_path_buf(),
            object_root: object_root.as_ref().to_path_buf(),
        };
        let connection = service.control_connection()?;
        ensure_schema(&connection, "distributed_coordinator_metadata", 1)?;
        ensure_schema(
            &connection,
            "remote_storage_metadata",
            STORAGE_SCHEMA_VERSION,
        )?;
        initialize_schema(&connection)?;
        let facts = open_existing(&service.fact_path)?;
        ensure_schema(&facts, "fact_store_metadata", 1)?;
        let objects = open_existing(&service.object_root.join("object-store.db"))?;
        ensure_schema(&objects, "object_store_metadata", 1)?;
        Ok(service)
    }

    fn control_connection(&self) -> Result<Connection, MaintenanceStoreError> {
        let connection = open_existing(&self.control_path)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| MaintenanceStoreError::Unavailable)?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|_| MaintenanceStoreError::Unavailable)?;
        Ok(connection)
    }

    fn begin(
        &self,
        deletion: &DeleteDataRequest,
        now: DateTime<Utc>,
    ) -> Result<StoredDeletion, MaintenanceStoreError> {
        let mut connection = self.control_connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| MaintenanceStoreError::Unavailable)?;
        if let Some(existing) = load_by_idempotency(&transaction, deletion)? {
            install_control_tombstone(&transaction, &existing.request, now)?;
            transaction
                .commit()
                .map_err(|_| MaintenanceStoreError::Unavailable)?;
            return Ok(existing);
        }
        if load_by_deletion_id(
            &transaction,
            deletion.target.project(),
            &deletion.deletion_id,
        )?
        .is_some()
        {
            return Err(MaintenanceStoreError::Conflict);
        }
        let request_json =
            serde_json::to_vec(deletion).map_err(|_| MaintenanceStoreError::Serialization)?;
        let counters_json = serde_json::to_vec(&BTreeMap::<AuditCounter, u64>::new())
            .map_err(|_| MaintenanceStoreError::Serialization)?;
        transaction
            .execute(
                "INSERT INTO remote_deletions (
                    tenant_id, project_id, deletion_id, idempotency_algorithm,
                    idempotency_value, request_json, state, counters_json,
                    audit_event_id, updated_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'running', ?7, NULL, ?8)",
                params![
                    deletion.target.project().tenant_id.as_str(),
                    deletion.target.project().project_id.as_str(),
                    deletion.deletion_id.as_str(),
                    deletion.idempotency_key.algorithm(),
                    deletion.idempotency_key.value(),
                    request_json,
                    counters_json,
                    now.timestamp_millis()
                ],
            )
            .map_err(|_| MaintenanceStoreError::Unavailable)?;
        install_control_tombstone(&transaction, deletion, now)?;
        transaction
            .commit()
            .map_err(|_| MaintenanceStoreError::Unavailable)?;
        Ok(StoredDeletion {
            request: deletion.clone(),
            state: DeletionState::Running,
            counters: BTreeMap::new(),
            audit_event_id: None,
            updated_at: now,
        })
    }

    fn execute_deletion(
        &self,
        deletion: &DeleteDataRequest,
        now: DateTime<Utc>,
    ) -> Result<DeletionCounts, MaintenanceStoreError> {
        if matches!(deletion.target, DeletionTarget::Repository(_))
            && deletion.coverage.contains(&RetentionClass::UploadedSource)
        {
            // Source objects are project-scoped and may be shared by another
            // repository or memory input. RG5 does not yet persist a complete
            // repository ownership/refcount index, so this cannot be claimed.
            return Err(MaintenanceStoreError::Conflict);
        }
        let mut counts = DeletionCounts::default();
        let repository_jobs = match &deletion.target {
            DeletionTarget::Project(_) => Vec::new(),
            DeletionTarget::Repository(repository) => self.repository_jobs(repository)?,
        };

        if deletion.coverage.contains(&RetentionClass::UploadedSource) {
            let DeletionTarget::Project(project) = &deletion.target else {
                return Err(MaintenanceStoreError::Conflict);
            };
            counts.add(
                AuditCounter::Objects,
                delete_project_objects(&self.object_root, deletion, project, now)?,
            );
            self.record_progress(deletion, &mut counts, now)?;
        }
        if deletion.coverage.contains(&RetentionClass::UnpublishedFact) {
            counts.add(
                AuditCounter::FactBatches,
                delete_unpublished_facts(&self.fact_path, deletion, &repository_jobs, now)?,
            );
            self.record_progress(deletion, &mut counts, now)?;
        }
        let repository_has_unpublished_facts = match &deletion.target {
            DeletionTarget::Repository(repository)
                if deletion
                    .coverage
                    .contains(&RetentionClass::PublishedGraphSnapshot)
                    || deletion.coverage.contains(&RetentionClass::UnpublishedFact) =>
            {
                repository_jobs_have_unpublished_facts(
                    &self.fact_path,
                    repository,
                    &repository_jobs,
                )?
            }
            DeletionTarget::Repository(_) => true,
            DeletionTarget::Project(_) => false,
        };

        let mut connection = self.control_connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| MaintenanceStoreError::Unavailable)?;
        delete_control_data(
            &transaction,
            deletion,
            &repository_jobs,
            repository_has_unpublished_facts,
            &mut counts,
            now,
        )?;
        transaction
            .commit()
            .map_err(|_| MaintenanceStoreError::Unavailable)?;
        Ok(counts)
    }

    fn record_progress(
        &self,
        deletion: &DeleteDataRequest,
        counts: &mut DeletionCounts,
        now: DateTime<Utc>,
    ) -> Result<(), MaintenanceStoreError> {
        let mut connection = self.control_connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| MaintenanceStoreError::Unavailable)?;
        persist_progress(&transaction, deletion, counts, now)?;
        transaction
            .commit()
            .map_err(|_| MaintenanceStoreError::Unavailable)
    }

    fn repository_jobs(
        &self,
        repository: &RemoteRepositoryRef,
    ) -> Result<Vec<String>, MaintenanceStoreError> {
        let connection = self.control_connection()?;
        let mut statement = connection
            .prepare(
                "SELECT job_id, spec_json FROM distributed_index_jobs
                 WHERE tenant_id = ?1 AND project_id = ?2",
            )
            .map_err(|_| MaintenanceStoreError::Unavailable)?;
        let rows = statement
            .query_map(
                params![
                    repository.project.tenant_id.as_str(),
                    repository.project.project_id.as_str()
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .map_err(|_| MaintenanceStoreError::Unavailable)?;
        let mut jobs = Vec::new();
        for row in rows {
            let (job_id, encoded) = row.map_err(|_| MaintenanceStoreError::Unavailable)?;
            let spec: IndexJobSpec = serde_json::from_slice(&encoded)
                .map_err(|_| MaintenanceStoreError::IntegrityFailure)?;
            if matches!(
                spec.input,
                IndexInputRef::Repository(ref manifest) if manifest.repository == *repository
            ) {
                jobs.push(job_id);
            }
        }
        Ok(jobs)
    }

    fn finish(
        &self,
        authorization: &AuthorizationContext,
        request: &RemoteDeleteRequest,
        mut counts: DeletionCounts,
        now: DateTime<Utc>,
    ) -> Result<StoredDeletion, MaintenanceStoreError> {
        let mut connection = self.control_connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| MaintenanceStoreError::Unavailable)?;
        let current = load_by_idempotency(&transaction, &request.deletion)?
            .ok_or(MaintenanceStoreError::IntegrityFailure)?;
        counts.merge_max(&current.counters);
        let event_id = audit_event_id(&request.deletion, AuditOutcome::Succeeded)?;
        let record = audit_record(
            authorization,
            &current.request,
            event_id.clone(),
            AuditOutcome::Succeeded,
            None,
            counts.counters.clone(),
            now,
        );
        persist_audit(&transaction, &record)?;
        let counters_json = serde_json::to_vec(&counts.counters)
            .map_err(|_| MaintenanceStoreError::Serialization)?;
        transaction
            .execute(
                "UPDATE remote_deletions
                 SET state = 'complete', counters_json = ?4, audit_event_id = ?5,
                     updated_at_ms = ?6
                 WHERE tenant_id = ?1 AND project_id = ?2 AND deletion_id = ?3",
                params![
                    request.deletion.target.project().tenant_id.as_str(),
                    request.deletion.target.project().project_id.as_str(),
                    current.request.deletion_id.as_str(),
                    counters_json,
                    event_id.as_str(),
                    now.timestamp_millis()
                ],
            )
            .map_err(|_| MaintenanceStoreError::Unavailable)?;
        transaction
            .commit()
            .map_err(|_| MaintenanceStoreError::Unavailable)?;
        Ok(StoredDeletion {
            request: current.request,
            state: DeletionState::Complete,
            counters: counts.counters,
            audit_event_id: Some(event_id),
            updated_at: now,
        })
    }

    fn record_failure(
        &self,
        authorization: &AuthorizationContext,
        request: &RemoteDeleteRequest,
        duration_ms: u64,
        now: DateTime<Utc>,
    ) -> Result<(), MaintenanceStoreError> {
        let mut connection = self.control_connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| MaintenanceStoreError::Unavailable)?;
        let Some(current) = load_by_idempotency(&transaction, &request.deletion)? else {
            return Ok(());
        };
        if current.state == DeletionState::Complete {
            return Ok(());
        }
        let mut counters = current.counters;
        counters.insert(AuditCounter::DurationMs, duration_ms);
        let event_id = audit_event_id(&request.deletion, AuditOutcome::Failed)?;
        let record = audit_record(
            authorization,
            &current.request,
            event_id,
            AuditOutcome::Failed,
            Some(RemoteErrorCode::TemporarilyUnavailable),
            counters.clone(),
            now,
        );
        persist_audit(&transaction, &record)?;
        let counters_json =
            serde_json::to_vec(&counters).map_err(|_| MaintenanceStoreError::Serialization)?;
        transaction
            .execute(
                "UPDATE remote_deletions
                 SET state = 'failed', counters_json = ?4, updated_at_ms = ?5
                 WHERE tenant_id = ?1 AND project_id = ?2 AND deletion_id = ?3
                   AND state != 'complete'",
                params![
                    current.request.target.project().tenant_id.as_str(),
                    current.request.target.project().project_id.as_str(),
                    current.request.deletion_id.as_str(),
                    counters_json,
                    now.timestamp_millis()
                ],
            )
            .map_err(|_| MaintenanceStoreError::Unavailable)?;
        transaction
            .commit()
            .map_err(|_| MaintenanceStoreError::Unavailable)
    }
}

impl RemoteMaintenanceApi for SqliteRemoteMaintenance {
    fn delete(
        &mut self,
        authorization: &AuthorizationContext,
        request: &RemoteDeleteRequest,
        now: DateTime<Utc>,
    ) -> Result<RemoteDeletionResult, RemoteError> {
        authorize_deletion(authorization, &request.deletion.target, &request.request_id)?;
        request
            .validate()
            .map_err(|error| protocol_error(&request.request_id, error))?;
        if matches!(request.deletion.target, DeletionTarget::Repository(_))
            && request
                .deletion
                .coverage
                .contains(&RetentionClass::UploadedSource)
        {
            return Err(remote_error(
                &request.request_id,
                RemoteErrorCode::InvalidRequest,
                false,
            ));
        }
        let existing = self
            .begin(&request.deletion, now)
            .map_err(|error| store_error(&request.request_id, error))?;
        if existing.state == DeletionState::Complete {
            return Ok(result(&request.request_id, existing));
        }
        let started = Instant::now();
        let mut counts = match self.execute_deletion(&existing.request, now) {
            Ok(counts) => counts,
            Err(error) => {
                let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                let _ = self.record_failure(authorization, request, duration_ms, now);
                return Err(store_error(&request.request_id, error));
            }
        };
        counts.add(
            AuditCounter::DurationMs,
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        );
        self.finish(authorization, request, counts, now)
            .map(|stored| result(&request.request_id, stored))
            .map_err(|error| store_error(&request.request_id, error))
    }

    fn inspect_deletion(
        &self,
        authorization: &AuthorizationContext,
        request: &InspectRemoteDeletionRequest,
    ) -> Result<Option<RemoteDeletionResult>, RemoteError> {
        authorize_deletion(authorization, &request.target, &request.request_id)?;
        request
            .validate()
            .map_err(|error| protocol_error(&request.request_id, error))?;
        let connection = self
            .control_connection()
            .map_err(|error| store_error(&request.request_id, error))?;
        load_by_deletion_id(&connection, request.target.project(), &request.deletion_id)
            .map(|stored| {
                stored
                    .filter(|stored| stored.request.target == request.target)
                    .map(|stored| result(&request.request_id, stored))
            })
            .map_err(|error| store_error(&request.request_id, error))
    }
}

fn authorize_deletion(
    authorization: &AuthorizationContext,
    target: &DeletionTarget,
    request_id: &RequestId,
) -> Result<(), RemoteError> {
    let (permission, scope) = match target {
        DeletionTarget::Project(project) => (
            RemotePermission::DeleteProject,
            AuthorizationScope::Project(project.clone()),
        ),
        DeletionTarget::Repository(repository) => (
            RemotePermission::DeleteRepository,
            AuthorizationScope::Repository(repository.clone()),
        ),
    };
    authorization
        .authorize(permission, &scope)
        .map_err(|_| remote_error(request_id, RemoteErrorCode::Unauthorized, false))
}

fn delete_project_objects(
    object_root: &Path,
    deletion: &DeleteDataRequest,
    project: &RemoteProjectRef,
    now: DateTime<Utc>,
) -> Result<u64, MaintenanceStoreError> {
    let mut connection = open_existing(&object_root.join("object-store.db"))?;
    ensure_schema(&connection, "object_store_metadata", 1)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| MaintenanceStoreError::Unavailable)?;
    initialize_tombstone_schema(&transaction)?;
    install_store_tombstone(&transaction, deletion, now)?;
    let count = count(
        &transaction,
        "SELECT COUNT(*) FROM source_objects WHERE tenant_id = ?1 AND project_id = ?2",
        params![project.tenant_id.as_str(), project.project_id.as_str()],
    )?;
    let directory = object_root
        .join("objects")
        .join(project.tenant_id.as_str())
        .join(project.project_id.as_str());
    match fs::remove_dir_all(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(MaintenanceStoreError::Unavailable),
    }
    transaction
        .execute(
            "DELETE FROM source_objects WHERE tenant_id = ?1 AND project_id = ?2",
            params![project.tenant_id.as_str(), project.project_id.as_str()],
        )
        .map_err(|_| MaintenanceStoreError::Unavailable)?;
    transaction
        .commit()
        .map_err(|_| MaintenanceStoreError::Unavailable)?;
    Ok(count)
}

fn delete_unpublished_facts(
    fact_path: &Path,
    deletion: &DeleteDataRequest,
    repository_jobs: &[String],
    now: DateTime<Utc>,
) -> Result<u64, MaintenanceStoreError> {
    let mut connection = open_existing(fact_path)?;
    ensure_schema(&connection, "fact_store_metadata", 1)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| MaintenanceStoreError::Unavailable)?;
    initialize_tombstone_schema(&transaction)?;
    install_store_tombstone(&transaction, deletion, now)?;
    let deleted = match &deletion.target {
        DeletionTarget::Project(project) => transaction
            .execute(
                "DELETE FROM unpublished_fact_batches
                 WHERE tenant_id = ?1 AND project_id = ?2",
                params![project.tenant_id.as_str(), project.project_id.as_str()],
            )
            .map_err(|_| MaintenanceStoreError::Unavailable)?,
        DeletionTarget::Repository(repository) => {
            let mut total = 0usize;
            for job in repository_jobs {
                total = total.saturating_add(
                    transaction
                        .execute(
                            "DELETE FROM unpublished_fact_batches
                             WHERE tenant_id = ?1 AND project_id = ?2 AND job_id = ?3",
                            params![
                                repository.project.tenant_id.as_str(),
                                repository.project.project_id.as_str(),
                                job
                            ],
                        )
                        .map_err(|_| MaintenanceStoreError::Unavailable)?,
                );
            }
            total
        }
    };
    transaction
        .commit()
        .map_err(|_| MaintenanceStoreError::Unavailable)?;
    u64::try_from(deleted).map_err(|_| MaintenanceStoreError::IntegrityFailure)
}

fn is_full_project_deletion(deletion: &DeleteDataRequest) -> bool {
    matches!(deletion.target, DeletionTarget::Project(_))
        && RetentionClass::ALL
            .iter()
            .all(|class| deletion.coverage.contains(class))
}

fn initialize_tombstone_schema(connection: &Connection) -> Result<(), MaintenanceStoreError> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS project_deletion_tombstones (
                 tenant_id TEXT NOT NULL,
                 project_id TEXT NOT NULL,
                 deletion_id TEXT NOT NULL,
                 created_at_ms INTEGER NOT NULL,
                 PRIMARY KEY (tenant_id, project_id)
             );",
        )
        .map_err(|_| MaintenanceStoreError::Unavailable)
}

fn install_control_tombstone(
    transaction: &Transaction<'_>,
    deletion: &DeleteDataRequest,
    now: DateTime<Utc>,
) -> Result<(), MaintenanceStoreError> {
    install_store_tombstone(transaction, deletion, now)
}

fn install_store_tombstone(
    connection: &Connection,
    deletion: &DeleteDataRequest,
    now: DateTime<Utc>,
) -> Result<(), MaintenanceStoreError> {
    if !is_full_project_deletion(deletion) {
        return Ok(());
    }
    let project = deletion.target.project();
    connection
        .execute(
            "INSERT INTO project_deletion_tombstones (
                 tenant_id, project_id, deletion_id, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (tenant_id, project_id) DO NOTHING",
            params![
                project.tenant_id.as_str(),
                project.project_id.as_str(),
                deletion.deletion_id.as_str(),
                now.timestamp_millis()
            ],
        )
        .map(|_| ())
        .map_err(|_| MaintenanceStoreError::Unavailable)
}

fn repository_jobs_have_unpublished_facts(
    fact_path: &Path,
    repository: &RemoteRepositoryRef,
    repository_jobs: &[String],
) -> Result<bool, MaintenanceStoreError> {
    let connection = open_existing(fact_path)?;
    ensure_schema(&connection, "fact_store_metadata", 1)?;
    for job in repository_jobs {
        let exists = connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM unpublished_fact_batches
                    WHERE tenant_id = ?1 AND project_id = ?2 AND job_id = ?3
                 )",
                params![
                    repository.project.tenant_id.as_str(),
                    repository.project.project_id.as_str(),
                    job
                ],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|_| MaintenanceStoreError::Unavailable)?;
        if exists {
            return Ok(true);
        }
    }
    Ok(false)
}

fn delete_control_data(
    transaction: &Transaction<'_>,
    deletion: &DeleteDataRequest,
    repository_jobs: &[String],
    repository_has_unpublished_facts: bool,
    counts: &mut DeletionCounts,
    now: DateTime<Utc>,
) -> Result<(), MaintenanceStoreError> {
    let project = deletion.target.project();
    if deletion
        .coverage
        .contains(&RetentionClass::PublishedGraphSnapshot)
    {
        let snapshots = match &deletion.target {
            DeletionTarget::Project(_) => count(
                transaction,
                "SELECT COUNT(*) FROM remote_immutable_revisions
                 WHERE tenant_id = ?1 AND project_id = ?2 AND domain = 'repository_graph'",
                params![project.tenant_id.as_str(), project.project_id.as_str()],
            )?,
            DeletionTarget::Repository(repository) => count(
                transaction,
                "SELECT COUNT(*) FROM remote_immutable_revisions
                 WHERE tenant_id = ?1 AND project_id = ?2 AND domain = 'repository_graph'
                   AND repository_id = ?3",
                params![
                    project.tenant_id.as_str(),
                    project.project_id.as_str(),
                    repository.repository_id.as_str()
                ],
            )?,
        };
        counts.add(AuditCounter::Snapshots, snapshots);
        match &deletion.target {
            DeletionTarget::Project(_) => {
                execute_scope_delete(
                    transaction,
                    "DELETE FROM remote_graph_views WHERE tenant_id = ?1 AND project_id = ?2",
                    project,
                )?;
                execute_scope_delete(
                    transaction,
                    "DELETE FROM remote_immutable_revisions
                     WHERE tenant_id = ?1 AND project_id = ?2 AND domain = 'repository_graph'",
                    project,
                )?;
            }
            DeletionTarget::Repository(repository) => {
                transaction
                    .execute(
                        "DELETE FROM remote_graph_views
                         WHERE tenant_id = ?1 AND project_id = ?2 AND repository_id = ?3",
                        params![
                            project.tenant_id.as_str(),
                            project.project_id.as_str(),
                            repository.repository_id.as_str()
                        ],
                    )
                    .map_err(|_| MaintenanceStoreError::Unavailable)?;
                transaction
                    .execute(
                        "DELETE FROM remote_immutable_revisions
                         WHERE tenant_id = ?1 AND project_id = ?2
                           AND domain = 'repository_graph' AND repository_id = ?3",
                        params![
                            project.tenant_id.as_str(),
                            project.project_id.as_str(),
                            repository.repository_id.as_str()
                        ],
                    )
                    .map_err(|_| MaintenanceStoreError::Unavailable)?;
            }
        }
    }
    if deletion
        .coverage
        .contains(&RetentionClass::PublishedMemoryRevision)
        && matches!(deletion.target, DeletionTarget::Project(_))
    {
        counts.add(
            AuditCounter::Revisions,
            count(
                transaction,
                "SELECT COUNT(*) FROM remote_immutable_revisions
                 WHERE tenant_id = ?1 AND project_id = ?2 AND domain = 'project_memory'",
                params![project.tenant_id.as_str(), project.project_id.as_str()],
            )?,
        );
        execute_scope_delete(
            transaction,
            "DELETE FROM remote_memory_views WHERE tenant_id = ?1 AND project_id = ?2",
            project,
        )?;
        execute_scope_delete(
            transaction,
            "DELETE FROM remote_immutable_revisions
             WHERE tenant_id = ?1 AND project_id = ?2 AND domain = 'project_memory'",
            project,
        )?;
    }
    if deletion.coverage.contains(&RetentionClass::QueryCache) {
        // No query cache is implemented in the SQLite prototype. Recording an
        // explicit zero preserves coverage without inventing a hidden store.
        counts.add(AuditCounter::CacheEntries, 0);
    }
    if deletion.coverage.contains(&RetentionClass::AuditRecord) {
        let repository_id = match &deletion.target {
            DeletionTarget::Project(_) => None,
            DeletionTarget::Repository(repository) => Some(repository.repository_id.as_str()),
        };
        let deleted = if let Some(repository_id) = repository_id {
            transaction
                .execute(
                    "DELETE FROM remote_audit_records
                     WHERE tenant_id = ?1 AND project_id = ?2 AND repository_id = ?3",
                    params![
                        project.tenant_id.as_str(),
                        project.project_id.as_str(),
                        repository_id
                    ],
                )
                .map_err(|_| MaintenanceStoreError::Unavailable)?
        } else {
            transaction
                .execute(
                    "DELETE FROM remote_audit_records
                     WHERE tenant_id = ?1 AND project_id = ?2",
                    params![project.tenant_id.as_str(), project.project_id.as_str()],
                )
                .map_err(|_| MaintenanceStoreError::Unavailable)?
        };
        counts.add(
            AuditCounter::AuditRecords,
            u64::try_from(deleted).map_err(|_| MaintenanceStoreError::IntegrityFailure)?,
        );
    }
    let delete_graph_jobs = deletion
        .coverage
        .contains(&RetentionClass::PublishedGraphSnapshot);
    let delete_memory_jobs = deletion
        .coverage
        .contains(&RetentionClass::PublishedMemoryRevision)
        && matches!(deletion.target, DeletionTarget::Project(_));
    let delete_repository_jobs = match &deletion.target {
        DeletionTarget::Repository(repository) => {
            !repository_has_unpublished_facts
                && count(
                    transaction,
                    "SELECT COUNT(*) FROM remote_immutable_revisions
                     WHERE tenant_id = ?1 AND project_id = ?2
                       AND domain = 'repository_graph' AND repository_id = ?3",
                    params![
                        project.tenant_id.as_str(),
                        project.project_id.as_str(),
                        repository.repository_id.as_str()
                    ],
                )? == 0
                && (deletion
                    .coverage
                    .contains(&RetentionClass::PublishedGraphSnapshot)
                    || deletion.coverage.contains(&RetentionClass::UnpublishedFact))
        }
        DeletionTarget::Project(_) => false,
    };
    if delete_graph_jobs || delete_memory_jobs || delete_repository_jobs {
        let deleted = match &deletion.target {
            DeletionTarget::Project(_) => transaction
                .execute(
                    "DELETE FROM distributed_index_jobs
                     WHERE tenant_id = ?1 AND project_id = ?2
                       AND ((kind = 'repository_graph' AND ?3)
                         OR (kind = 'project_memory' AND ?4))",
                    params![
                        project.tenant_id.as_str(),
                        project.project_id.as_str(),
                        delete_graph_jobs,
                        delete_memory_jobs
                    ],
                )
                .map_err(|_| MaintenanceStoreError::Unavailable)?,
            DeletionTarget::Repository(_) if delete_repository_jobs => {
                let mut total = 0usize;
                for job in repository_jobs {
                    total = total.saturating_add(
                        transaction
                            .execute(
                                "DELETE FROM distributed_index_jobs
                                 WHERE tenant_id = ?1 AND project_id = ?2 AND job_id = ?3",
                                params![
                                    project.tenant_id.as_str(),
                                    project.project_id.as_str(),
                                    job
                                ],
                            )
                            .map_err(|_| MaintenanceStoreError::Unavailable)?,
                    );
                }
                total
            }
            DeletionTarget::Repository(_) => 0,
        };
        counts.add(
            AuditCounter::Jobs,
            u64::try_from(deleted).map_err(|_| MaintenanceStoreError::IntegrityFailure)?,
        );
    }
    persist_progress(transaction, deletion, counts, now)?;
    Ok(())
}

fn persist_progress(
    transaction: &Transaction<'_>,
    deletion: &DeleteDataRequest,
    counts: &mut DeletionCounts,
    now: DateTime<Utc>,
) -> Result<(), MaintenanceStoreError> {
    let current = load_by_idempotency(transaction, deletion)?
        .ok_or(MaintenanceStoreError::IntegrityFailure)?;
    counts.merge_max(&current.counters);
    let counters_json =
        serde_json::to_vec(&counts.counters).map_err(|_| MaintenanceStoreError::Serialization)?;
    transaction
        .execute(
            "UPDATE remote_deletions
             SET state = 'running', counters_json = ?4, updated_at_ms = ?5
             WHERE tenant_id = ?1 AND project_id = ?2 AND deletion_id = ?3
               AND state != 'complete'",
            params![
                current.request.target.project().tenant_id.as_str(),
                current.request.target.project().project_id.as_str(),
                current.request.deletion_id.as_str(),
                counters_json,
                now.timestamp_millis()
            ],
        )
        .map_err(|_| MaintenanceStoreError::Unavailable)?;
    Ok(())
}

fn execute_scope_delete(
    transaction: &Transaction<'_>,
    sql: &str,
    project: &RemoteProjectRef,
) -> Result<(), MaintenanceStoreError> {
    transaction
        .execute(
            sql,
            params![project.tenant_id.as_str(), project.project_id.as_str()],
        )
        .map_err(|_| MaintenanceStoreError::Unavailable)?;
    Ok(())
}

fn count<P: rusqlite::Params>(
    transaction: &Transaction<'_>,
    sql: &str,
    parameters: P,
) -> Result<u64, MaintenanceStoreError> {
    let value = transaction
        .query_row(sql, parameters, |row| row.get::<_, i64>(0))
        .map_err(|_| MaintenanceStoreError::Unavailable)?;
    u64::try_from(value).map_err(|_| MaintenanceStoreError::IntegrityFailure)
}

fn audit_record(
    authorization: &AuthorizationContext,
    deletion: &DeleteDataRequest,
    event_id: AuditEventId,
    outcome: AuditOutcome,
    error_code: Option<RemoteErrorCode>,
    counters: BTreeMap<AuditCounter, u64>,
    now: DateTime<Utc>,
) -> AuditRecord {
    AuditRecord {
        policy_version: DISTRIBUTED_POLICY_VERSION,
        event_id,
        principal_id: authorization.principal_id().clone(),
        credential_id: authorization.credential_id().clone(),
        action: match deletion.target {
            DeletionTarget::Project(_) => RemotePermission::DeleteProject,
            DeletionTarget::Repository(_) => RemotePermission::DeleteRepository,
        },
        outcome,
        resource: AuditedResource::Deletion {
            target: deletion.target.clone(),
            deletion_id: deletion.deletion_id.clone(),
        },
        error_code,
        counters,
        observed_at: now,
    }
}

fn persist_audit(
    transaction: &Transaction<'_>,
    record: &AuditRecord,
) -> Result<(), MaintenanceStoreError> {
    let AuditedResource::Deletion { target, .. } = &record.resource else {
        return Err(MaintenanceStoreError::IntegrityFailure);
    };
    let project = target.project();
    let repository_id = match target {
        DeletionTarget::Project(_) => "",
        DeletionTarget::Repository(repository) => repository.repository_id.as_str(),
    };
    let encoded = serde_json::to_vec(record).map_err(|_| MaintenanceStoreError::Serialization)?;
    transaction
        .execute(
            "INSERT INTO remote_audit_records (
                tenant_id, project_id, repository_id, event_id, record_json, observed_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (tenant_id, project_id, event_id) DO UPDATE SET
                record_json = excluded.record_json,
                observed_at_ms = excluded.observed_at_ms",
            params![
                project.tenant_id.as_str(),
                project.project_id.as_str(),
                repository_id,
                record.event_id.as_str(),
                encoded,
                record.observed_at.timestamp_millis()
            ],
        )
        .map_err(|_| MaintenanceStoreError::Unavailable)?;
    Ok(())
}

fn audit_event_id(
    deletion: &DeleteDataRequest,
    outcome: AuditOutcome,
) -> Result<AuditEventId, MaintenanceStoreError> {
    let mut hasher = Sha256::new();
    hasher.update(b"ferrus.distributed.deletion-audit.v1\0");
    hasher.update(deletion.idempotency_key.algorithm().as_bytes());
    hasher.update(deletion.idempotency_key.value().as_bytes());
    hasher.update(match outcome {
        AuditOutcome::Allowed => b"allowed".as_slice(),
        AuditOutcome::Denied => b"denied".as_slice(),
        AuditOutcome::Succeeded => b"succeeded".as_slice(),
        AuditOutcome::Failed => b"failed".as_slice(),
    });
    let value = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    AuditEventId::new(format!("audit-{value}")).map_err(|_| MaintenanceStoreError::IntegrityFailure)
}

fn load_by_idempotency(
    connection: &Connection,
    deletion: &DeleteDataRequest,
) -> Result<Option<StoredDeletion>, MaintenanceStoreError> {
    connection
        .query_row(
            "SELECT request_json, state, counters_json, audit_event_id, updated_at_ms
             FROM remote_deletions
             WHERE tenant_id = ?1 AND project_id = ?2
               AND idempotency_algorithm = ?3 AND idempotency_value = ?4",
            params![
                deletion.target.project().tenant_id.as_str(),
                deletion.target.project().project_id.as_str(),
                deletion.idempotency_key.algorithm(),
                deletion.idempotency_key.value()
            ],
            stored_deletion_row,
        )
        .optional()
        .map_err(|_| MaintenanceStoreError::Unavailable)?
        .map(decode_stored_deletion)
        .transpose()
}

fn load_by_deletion_id(
    connection: &Connection,
    project: &RemoteProjectRef,
    deletion_id: &DeletionId,
) -> Result<Option<StoredDeletion>, MaintenanceStoreError> {
    connection
        .query_row(
            "SELECT request_json, state, counters_json, audit_event_id, updated_at_ms
             FROM remote_deletions
             WHERE tenant_id = ?1 AND project_id = ?2 AND deletion_id = ?3",
            params![
                project.tenant_id.as_str(),
                project.project_id.as_str(),
                deletion_id.as_str()
            ],
            stored_deletion_row,
        )
        .optional()
        .map_err(|_| MaintenanceStoreError::Unavailable)?
        .map(decode_stored_deletion)
        .transpose()
}

type StoredDeletionRow = (Vec<u8>, String, Vec<u8>, Option<String>, i64);

fn stored_deletion_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredDeletionRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

fn decode_stored_deletion(row: StoredDeletionRow) -> Result<StoredDeletion, MaintenanceStoreError> {
    let request =
        serde_json::from_slice(&row.0).map_err(|_| MaintenanceStoreError::IntegrityFailure)?;
    let state = match row.1.as_str() {
        "requested" => DeletionState::Requested,
        "running" => DeletionState::Running,
        "complete" => DeletionState::Complete,
        "failed" => DeletionState::Failed,
        _ => return Err(MaintenanceStoreError::IntegrityFailure),
    };
    let counters =
        serde_json::from_slice(&row.2).map_err(|_| MaintenanceStoreError::IntegrityFailure)?;
    let audit_event_id = row
        .3
        .map(AuditEventId::new)
        .transpose()
        .map_err(|_| MaintenanceStoreError::IntegrityFailure)?;
    let updated_at =
        DateTime::from_timestamp_millis(row.4).ok_or(MaintenanceStoreError::IntegrityFailure)?;
    Ok(StoredDeletion {
        request,
        state,
        counters,
        audit_event_id,
        updated_at,
    })
}

fn result(request_id: &RequestId, stored: StoredDeletion) -> RemoteDeletionResult {
    RemoteDeletionResult {
        protocol_version: DISTRIBUTED_MAINTENANCE_PROTOCOL_VERSION,
        request_id: request_id.clone(),
        deletion_id: stored.request.deletion_id,
        target: stored.request.target,
        state: stored.state,
        counters: stored.counters,
        audit_event_id: stored.audit_event_id,
        updated_at: stored.updated_at,
    }
}

fn open_existing(path: &Path) -> Result<Connection, MaintenanceStoreError> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| MaintenanceStoreError::Unavailable)
}

fn ensure_schema(
    connection: &Connection,
    table: &str,
    expected: u32,
) -> Result<(), MaintenanceStoreError> {
    let sql = format!("SELECT schema_version FROM {table} WHERE singleton = 1");
    let version = connection
        .query_row(&sql, [], |row| row.get::<_, u32>(0))
        .optional()
        .map_err(|_| MaintenanceStoreError::IncompatibleSchema)?;
    if version != Some(expected) {
        return Err(MaintenanceStoreError::IncompatibleSchema);
    }
    Ok(())
}

fn initialize_schema(connection: &Connection) -> Result<(), MaintenanceStoreError> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS remote_maintenance_metadata (
                 singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                 schema_version INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS project_deletion_tombstones (
                 tenant_id TEXT NOT NULL,
                 project_id TEXT NOT NULL,
                 deletion_id TEXT NOT NULL,
                 created_at_ms INTEGER NOT NULL,
                 PRIMARY KEY (tenant_id, project_id)
             );
             CREATE TABLE IF NOT EXISTS remote_deletions (
                 tenant_id TEXT NOT NULL,
                 project_id TEXT NOT NULL,
                 deletion_id TEXT NOT NULL,
                 idempotency_algorithm TEXT NOT NULL,
                 idempotency_value TEXT NOT NULL,
                 request_json BLOB NOT NULL,
                 state TEXT NOT NULL CHECK (state IN ('requested', 'running', 'complete', 'failed')),
                 counters_json BLOB NOT NULL,
                 audit_event_id TEXT,
                 updated_at_ms INTEGER NOT NULL,
                 PRIMARY KEY (tenant_id, project_id, deletion_id),
                 UNIQUE (tenant_id, project_id, idempotency_algorithm, idempotency_value)
             );
             CREATE TABLE IF NOT EXISTS remote_audit_records (
                 tenant_id TEXT NOT NULL,
                 project_id TEXT NOT NULL,
                 repository_id TEXT NOT NULL,
                 event_id TEXT NOT NULL,
                 record_json BLOB NOT NULL,
                 observed_at_ms INTEGER NOT NULL,
                 PRIMARY KEY (tenant_id, project_id, event_id)
             );",
        )
        .map_err(|_| MaintenanceStoreError::Unavailable)?;
    let version = connection
        .query_row(
            "SELECT schema_version FROM remote_maintenance_metadata WHERE singleton = 1",
            [],
            |row| row.get::<_, u32>(0),
        )
        .optional()
        .map_err(|_| MaintenanceStoreError::Unavailable)?;
    match version {
        None => connection
            .execute(
                "INSERT INTO remote_maintenance_metadata (singleton, schema_version)
                 VALUES (1, ?1)",
                [MAINTENANCE_SCHEMA_VERSION],
            )
            .map(|_| ())
            .map_err(|_| MaintenanceStoreError::Unavailable),
        Some(MAINTENANCE_SCHEMA_VERSION) => Ok(()),
        Some(_) => Err(MaintenanceStoreError::IncompatibleSchema),
    }
}

fn protocol_error(request_id: &RequestId, error: DistributedProtocolError) -> RemoteError {
    remote_error(
        request_id,
        if error == DistributedProtocolError::UnsupportedVersion {
            RemoteErrorCode::UnsupportedVersion
        } else {
            RemoteErrorCode::InvalidRequest
        },
        false,
    )
}

fn store_error(request_id: &RequestId, error: MaintenanceStoreError) -> RemoteError {
    match error {
        MaintenanceStoreError::Unavailable => {
            remote_error(request_id, RemoteErrorCode::TemporarilyUnavailable, true)
        }
        MaintenanceStoreError::Conflict => {
            remote_error(request_id, RemoteErrorCode::Conflict, false)
        }
        MaintenanceStoreError::IncompatibleSchema => {
            remote_error(request_id, RemoteErrorCode::UnsupportedVersion, false)
        }
        MaintenanceStoreError::IntegrityFailure | MaintenanceStoreError::Serialization => {
            remote_error(request_id, RemoteErrorCode::Internal, false)
        }
    }
}

fn remote_error(request_id: &RequestId, code: RemoteErrorCode, retryable: bool) -> RemoteError {
    RemoteError {
        protocol_version: DISTRIBUTED_MAINTENANCE_PROTOCOL_VERSION,
        request_id: request_id.clone(),
        code,
        retryable,
    }
}
