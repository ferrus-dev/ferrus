//! Scoped remote purge and writer quiescence.

use super::*;

pub(super) fn authorize_deletion(
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

pub(super) fn delete_project_objects(
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

pub(super) fn delete_unpublished_facts(
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

pub(super) fn is_full_project_deletion(deletion: &DeleteDataRequest) -> bool {
    matches!(deletion.target, DeletionTarget::Project(_))
        && RetentionClass::ALL
            .iter()
            .all(|class| deletion.coverage.contains(class))
}

pub(super) fn initialize_tombstone_schema(
    connection: &Connection,
) -> Result<(), MaintenanceStoreError> {
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

pub(super) fn install_control_tombstone(
    transaction: &Transaction<'_>,
    deletion: &DeleteDataRequest,
    now: DateTime<Utc>,
) -> Result<(), MaintenanceStoreError> {
    install_store_tombstone(transaction, deletion, now)
}

pub(super) fn install_store_tombstone(
    connection: &Connection,
    deletion: &DeleteDataRequest,
    now: DateTime<Utc>,
) -> Result<(), MaintenanceStoreError> {
    let project = deletion.target.project();
    let existing = connection
        .query_row(
            "SELECT deletion_id FROM project_deletion_tombstones
             WHERE tenant_id = ?1 AND project_id = ?2",
            params![project.tenant_id.as_str(), project.project_id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|_| MaintenanceStoreError::Unavailable)?;
    if existing.as_deref() == Some(deletion.deletion_id.as_str()) {
        return Ok(());
    }

    if existing.is_some() {
        return Err(MaintenanceStoreError::Conflict);
    }

    connection
        .execute(
            "INSERT INTO project_deletion_tombstones (
                 tenant_id, project_id, deletion_id, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4)",
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

pub(super) fn release_store_tombstone(
    path: &Path,
    deletion: &DeleteDataRequest,
) -> Result<(), MaintenanceStoreError> {
    let mut connection = open_existing(path)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| MaintenanceStoreError::Unavailable)?;
    let project = deletion.target.project();
    transaction
        .execute(
            "DELETE FROM project_deletion_tombstones
             WHERE tenant_id = ?1 AND project_id = ?2 AND deletion_id = ?3",
            params![
                project.tenant_id.as_str(),
                project.project_id.as_str(),
                deletion.deletion_id.as_str()
            ],
        )
        .map_err(|_| MaintenanceStoreError::Unavailable)?;
    transaction
        .commit()
        .map_err(|_| MaintenanceStoreError::Unavailable)
}

pub(super) fn repository_jobs_have_unpublished_facts(
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

pub(super) fn delete_control_data(
    transaction: &Transaction<'_>,
    deletion: &DeleteDataRequest,
    repository_jobs: &[String],
    repository_has_unpublished_facts: bool,
    counts: &mut DeletionCounts,
    now: DateTime<Utc>,
) -> Result<(), MaintenanceStoreError> {
    let project = deletion.target.project();
    cancel_covered_jobs(transaction, deletion, repository_jobs, now)?;
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
                    "DELETE FROM remote_memory_repository_link_sets
                     WHERE tenant_id = ?1 AND project_id = ?2",
                    project,
                )?;
                execute_scope_delete(
                    transaction,
                    "DELETE FROM remote_immutable_revisions
                     WHERE tenant_id = ?1 AND project_id = ?2
                       AND domain = 'memory_repository_links'",
                    project,
                )?;
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
                        "DELETE FROM remote_memory_repository_link_sets
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
                           AND domain = 'memory_repository_links' AND repository_id = ?3",
                        params![
                            project.tenant_id.as_str(),
                            project.project_id.as_str(),
                            repository.repository_id.as_str()
                        ],
                    )
                    .map_err(|_| MaintenanceStoreError::Unavailable)?;
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
        execute_scope_delete(
            transaction,
            "DELETE FROM remote_memory_repository_link_sets
             WHERE tenant_id = ?1 AND project_id = ?2",
            project,
        )?;
        execute_scope_delete(
            transaction,
            "DELETE FROM remote_immutable_revisions
             WHERE tenant_id = ?1 AND project_id = ?2
               AND domain = 'memory_repository_links'",
            project,
        )?;
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

pub(super) fn cancel_covered_jobs(
    transaction: &Transaction<'_>,
    deletion: &DeleteDataRequest,
    repository_jobs: &[String],
    now: DateTime<Utc>,
) -> Result<(), MaintenanceStoreError> {
    let project = deletion.target.project();
    match &deletion.target {
        DeletionTarget::Project(_)
            if deletion.coverage.contains(&RetentionClass::UnpublishedFact) =>
        {
            transaction
                .execute(
                    "UPDATE distributed_index_jobs
                     SET state = 'cancelled', cancellation_requested = 1,
                         lease_worker_id = NULL, lease_until_ms = NULL, updated_at_ms = ?3
                     WHERE tenant_id = ?1 AND project_id = ?2",
                    params![
                        project.tenant_id.as_str(),
                        project.project_id.as_str(),
                        now.timestamp_millis()
                    ],
                )
                .map_err(|_| MaintenanceStoreError::Unavailable)?;
        }
        DeletionTarget::Repository(_)
            if deletion
                .coverage
                .contains(&RetentionClass::PublishedGraphSnapshot)
                || deletion.coverage.contains(&RetentionClass::UnpublishedFact) =>
        {
            for job in repository_jobs {
                transaction
                    .execute(
                        "UPDATE distributed_index_jobs
                         SET state = 'cancelled', cancellation_requested = 1,
                             lease_worker_id = NULL, lease_until_ms = NULL, updated_at_ms = ?4
                         WHERE tenant_id = ?1 AND project_id = ?2 AND job_id = ?3",
                        params![
                            project.tenant_id.as_str(),
                            project.project_id.as_str(),
                            job,
                            now.timestamp_millis()
                        ],
                    )
                    .map_err(|_| MaintenanceStoreError::Unavailable)?;
            }
        }
        _ => {}
    }

    Ok(())
}
