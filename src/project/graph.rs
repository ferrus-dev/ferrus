use super::*;

pub async fn task_repository_view(task_id: &str) -> Result<Option<RepositoryViewReference>> {
    read_repository_view("tasks", task_id).await
}

#[allow(dead_code)]
pub async fn run_repository_view(run_id: &str) -> Result<Option<RepositoryViewReference>> {
    read_repository_view("runs", run_id).await
}

/// Returns the graph identities that ordinary sidecar garbage collection must
/// preserve. Non-terminal tasks and runs retain their publications indefinitely;
/// completed task and run snapshots age out through configured sidecar retention.
pub async fn repository_graph_retention_references() -> Result<RepositoryGraphRetentionReferences> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<RepositoryGraphRetentionReferences> {
        let connection = open_runtime_database(&database_path)?;
        let mut references = RepositoryGraphRetentionReferences::default();
        let mut statement = connection.prepare(
            r#"
            SELECT tasks.id,
                   tasks.baseline_snapshot_id, tasks.repository_view_snapshot_id,
                   runs.baseline_snapshot_id, runs.repository_view_snapshot_id
            FROM tasks
            LEFT JOIN runs ON runs.task_id = tasks.id
            WHERE tasks.status NOT IN ('complete', 'failed', 'reset')
               OR runs.status NOT IN ('completed', 'failed', 'interrupted')
            ORDER BY tasks.id, runs.id
            "#,
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        for row in rows {
            let (task_id, task_baseline, task_view, run_baseline, run_view) = row?;
            for snapshot in [task_baseline, task_view, run_baseline, run_view]
                .into_iter()
                .flatten()
            {
                references.snapshot_ids.insert(SnapshotId::new(snapshot)?);
            }
            references
                .view_names
                .insert(PublishedViewName::new(format!("task-baseline:{task_id}"))?);
            references
                .view_names
                .insert(PublishedViewName::new(format!("task-overlay:{task_id}"))?);
        }
        let canonical_snapshot = connection
            .query_row(
                "SELECT canonical_graph_snapshot_id FROM project_runtime_state WHERE row_id = 1",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        if let Some(snapshot) = canonical_snapshot {
            references.snapshot_ids.insert(SnapshotId::new(snapshot)?);
        }
        Ok(references)
    })
    .await?
}

pub async fn canonical_graph_reference() -> Result<CanonicalGraphReference> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<CanonicalGraphReference> {
        let connection =
            Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("Failed to open {} read-only", database_path.display()))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        let values = connection.query_row(
            r#"
            SELECT canonical_source_revision_id,
                   canonical_manifest_algorithm, canonical_manifest_digest,
                   canonical_graph_snapshot_id, canonical_graph_status
            FROM project_runtime_state
            WHERE row_id = 1
            "#,
            [],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )?;
        canonical_graph_reference_from_database(values)
    })
    .await?
}

pub async fn canonical_graph_refresh_guard() -> Result<CanonicalGraphRefreshGuard> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<CanonicalGraphRefreshGuard> {
        let connection =
            Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("Failed to open {} read-only", database_path.display()))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        Ok(CanonicalGraphRefreshGuard {
            invalidation_event_id: latest_canonical_graph_invalidation_event_id(&connection)?,
            refresh_event_id: latest_canonical_graph_refresh_event_id(&connection)?,
        })
    })
    .await?
}

pub async fn record_canonical_graph_invalidation(
    task_id: &str,
    run_id: Option<&str>,
    source: Option<&CanonicalSourceIdentity>,
    reason: CanonicalInvalidationReason,
) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let run_id = run_id.map(str::to_string);
    let source = source.cloned();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_project_runtime_state_row(&transaction)?;
        transaction.execute(
            r#"
            UPDATE project_runtime_state
            SET canonical_source_revision_id = ?1,
                canonical_manifest_algorithm = ?2,
                canonical_manifest_digest = ?3,
                canonical_graph_status = 'stale',
                canonical_graph_updated_at = ?4,
                updated_at = ?4
            WHERE row_id = 1
            "#,
            params![
                source
                    .as_ref()
                    .map(|identity| identity.source_revision_id.as_str()),
                source
                    .as_ref()
                    .map(|identity| identity.manifest_digest.algorithm()),
                source
                    .as_ref()
                    .map(|identity| identity.manifest_digest.value()),
                timestamp(),
            ],
        )?;
        insert_event_in_transaction(
            &transaction,
            run_id.as_deref(),
            "canonical_graph_invalidated",
            &serde_json::json!({
                "task_id": task_id,
                "reason": reason.as_str(),
                "source_revision_id": source
                    .as_ref()
                    .map(|identity| identity.source_revision_id.as_str()),
                "manifest_digest": source
                    .as_ref()
                    .map(|identity| identity.manifest_digest.value()),
            }),
        )?;
        transaction.commit()?;
        Ok(())
    })
    .await?
}

pub async fn record_canonical_graph_invalidation_best_effort(
    task_id: &str,
    run_id: Option<&str>,
    source: Option<&CanonicalSourceIdentity>,
    reason: CanonicalInvalidationReason,
) {
    if let Err(error) = record_canonical_graph_invalidation(task_id, run_id, source, reason).await {
        warn!(
            task_id,
            error = ?error,
            "failed to record canonical repository graph invalidation"
        );
    }
}

pub async fn record_canonical_graph_refresh(
    task_id: Option<&str>,
    run_id: Option<&str>,
    guard: CanonicalGraphRefreshGuard,
    source: &CanonicalSourceIdentity,
    snapshot_id: &SnapshotId,
    build_id: &BuildId,
) -> Result<CanonicalGraphRefreshOutcome> {
    let database_path = current_database_path().await?;
    let task_id = task_id.map(str::to_string);
    let run_id = run_id.map(str::to_string);
    let source = source.clone();
    let snapshot_id = snapshot_id.clone();
    let build_id = build_id.clone();
    tokio::task::spawn_blocking(move || -> Result<CanonicalGraphRefreshOutcome> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_project_runtime_state_row(&transaction)?;
        let updated = transaction.execute(
            r#"
            UPDATE project_runtime_state
            SET canonical_source_revision_id = ?1,
                canonical_manifest_algorithm = ?2,
                canonical_manifest_digest = ?3,
                canonical_graph_snapshot_id = ?4,
                canonical_graph_status = 'fresh',
                canonical_graph_updated_at = ?5,
                updated_at = ?5
            WHERE row_id = 1
              AND (
                    SELECT COALESCE(MAX(id), 0)
                    FROM events
                    WHERE type = 'canonical_graph_invalidated'
                  ) = ?6
              AND (
                    SELECT COALESCE(MAX(id), 0)
                    FROM events
                    WHERE type = 'canonical_graph_refreshed'
                  ) = ?7
            "#,
            params![
                source.source_revision_id.as_str(),
                source.manifest_digest.algorithm(),
                source.manifest_digest.value(),
                snapshot_id.as_str(),
                timestamp(),
                guard.invalidation_event_id,
                guard.refresh_event_id,
            ],
        )?;
        if updated == 0 {
            transaction.commit()?;
            return Ok(CanonicalGraphRefreshOutcome::Superseded);
        }
        insert_event_in_transaction(
            &transaction,
            run_id.as_deref(),
            "canonical_graph_refreshed",
            &serde_json::json!({
                "task_id": task_id,
                "source_revision_id": source.source_revision_id.as_str(),
                "snapshot_id": snapshot_id.as_str(),
                "build_id": build_id.as_str(),
            }),
        )?;
        transaction.commit()?;
        Ok(CanonicalGraphRefreshOutcome::Recorded)
    })
    .await?
}

fn latest_canonical_graph_invalidation_event_id(connection: &Connection) -> Result<i64> {
    Ok(connection.query_row(
        r#"
        SELECT COALESCE(MAX(id), 0)
        FROM events
        WHERE type = 'canonical_graph_invalidated'
        "#,
        [],
        |row| row.get(0),
    )?)
}

fn latest_canonical_graph_refresh_event_id(connection: &Connection) -> Result<i64> {
    Ok(connection.query_row(
        r#"
        SELECT COALESCE(MAX(id), 0)
        FROM events
        WHERE type = 'canonical_graph_refreshed'
        "#,
        [],
        |row| row.get(0),
    )?)
}

pub async fn record_canonical_graph_refresh_failed_best_effort(
    task_id: &str,
    run_id: Option<&str>,
    guard: CanonicalGraphRefreshGuard,
) {
    let database_path = match current_database_path().await {
        Ok(path) => path,
        Err(error) => {
            warn!(task_id, error = ?error, "failed to resolve canonical graph state database");
            return;
        }
    };
    let task_id = task_id.to_string();
    let task_id_for_log = task_id.clone();
    let run_id = run_id.map(str::to_string);
    let result = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_project_runtime_state_row(&transaction)?;
        let state_updated = transaction.execute(
            r#"
            UPDATE project_runtime_state
            SET canonical_graph_status = 'stale',
                canonical_graph_updated_at = ?1,
                updated_at = ?1
            WHERE row_id = 1
              AND (
                    SELECT COALESCE(MAX(id), 0)
                    FROM events
                    WHERE type = 'canonical_graph_invalidated'
                  ) = ?2
              AND (
                    SELECT COALESCE(MAX(id), 0)
                    FROM events
                    WHERE type = 'canonical_graph_refreshed'
                  ) = ?3
            "#,
            params![
                timestamp(),
                guard.invalidation_event_id,
                guard.refresh_event_id,
            ],
        )?;
        insert_event_in_transaction(
            &transaction,
            run_id.as_deref(),
            "canonical_graph_refresh_failed",
            &serde_json::json!({
                "task_id": task_id,
                "failure_code": "canonical_refresh_failed",
                "state_updated": state_updated == 1,
            }),
        )?;
        transaction.commit()?;
        Ok(())
    })
    .await;
    if let Err(error) = result.unwrap_or_else(|error| Err(error.into())) {
        warn!(task_id = task_id_for_log, error = ?error, "failed to record canonical graph refresh failure");
    }
}

type CanonicalGraphDatabaseValues = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
);

fn canonical_graph_reference_from_database(
    values: CanonicalGraphDatabaseValues,
) -> Result<CanonicalGraphReference> {
    let (source_revision_id, algorithm, digest, snapshot_id, status) = values;
    let source = match (source_revision_id, algorithm, digest) {
        (Some(revision), Some(algorithm), Some(value)) => Some(CanonicalSourceIdentity {
            source_revision_id: SourceRevisionId::new(revision)
                .context("Invalid canonical source revision in ferrus.db")?,
            manifest_digest: Digest::new(algorithm, value)
                .context("Invalid canonical manifest digest in ferrus.db")?,
        }),
        (None, None, None) => None,
        _ => anyhow::bail!("Incomplete canonical source identity in ferrus.db"),
    };
    Ok(CanonicalGraphReference {
        source,
        snapshot_id: snapshot_id
            .map(SnapshotId::new)
            .transpose()
            .context("Invalid canonical graph snapshot in ferrus.db")?,
        status: CanonicalGraphStatus::from_database(&status)?,
    })
}

async fn read_repository_view(
    owner_table: &'static str,
    owner_id: &str,
) -> Result<Option<RepositoryViewReference>> {
    debug_assert!(matches!(owner_table, "tasks" | "runs"));
    let database_path = current_database_path().await?;
    let owner_id = owner_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<RepositoryViewReference>> {
        let connection = open_runtime_database(&database_path)?;
        let sql = format!(
            "SELECT baseline_snapshot_id, overlay_revision_id, repository_view_snapshot_id, \
                    repository_view_tree_algorithm, repository_view_tree_digest, \
                    repository_view_lifecycle, repository_view_status \
             FROM {owner_table} WHERE id = ?1"
        );
        let values = connection
            .query_row(&sql, [&owner_id], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })
            .optional()?;
        values
            .map(repository_view_reference_from_database)
            .transpose()
    })
    .await?
}

#[allow(dead_code)]
pub async fn record_task_repository_view(
    task_id: &str,
    repository_view: &RepositoryViewReference,
) -> Result<()> {
    let database_path = current_database_path().await?;
    record_task_repository_view_at(&database_path, task_id, repository_view).await
}

pub(crate) async fn record_task_repository_view_at(
    database_path: &Path,
    task_id: &str,
    repository_view: &RepositoryViewReference,
) -> Result<()> {
    record_repository_view_at(database_path, "tasks", task_id, repository_view).await
}

pub(crate) async fn compare_and_record_task_repository_view_at(
    database_path: &Path,
    task_id: &str,
    expected: &RepositoryViewReference,
    repository_view: &RepositoryViewReference,
) -> Result<bool> {
    expected.validate()?;
    repository_view.validate()?;
    let database_path = database_path.to_path_buf();
    let task_id = task_id.to_string();
    let expected = expected.clone();
    let repository_view = repository_view.clone();
    tokio::task::spawn_blocking(move || -> Result<bool> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = transaction
            .query_row(
                r#"
            SELECT baseline_snapshot_id, overlay_revision_id, repository_view_snapshot_id,
                   repository_view_tree_algorithm, repository_view_tree_digest,
                   repository_view_lifecycle, repository_view_status
            FROM tasks
            WHERE id = ?1
            "#,
                [&task_id],
                |row| repository_view_reference_from_row(row, 0),
            )
            .optional()?
            .context("Cannot record repository view: tasks row does not exist")?;
        if current != expected {
            transaction.commit()?;
            return Ok(false);
        }
        record_repository_view_in_transaction(&transaction, "tasks", &task_id, &repository_view)?;
        transaction.commit()?;
        Ok(true)
    })
    .await?
}

#[allow(dead_code)]
pub async fn record_run_repository_view(
    run_id: &str,
    repository_view: &RepositoryViewReference,
) -> Result<()> {
    let database_path = current_database_path().await?;
    record_repository_view_at(&database_path, "runs", run_id, repository_view).await
}

async fn record_repository_view_at(
    database_path: &Path,
    owner_table: &'static str,
    owner_id: &str,
    repository_view: &RepositoryViewReference,
) -> Result<()> {
    debug_assert!(matches!(owner_table, "tasks" | "runs"));
    repository_view.validate()?;
    let database_path = database_path.to_path_buf();
    let owner_id = owner_id.to_string();
    let repository_view = repository_view.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        record_repository_view_in_transaction(
            &transaction,
            owner_table,
            &owner_id,
            &repository_view,
        )?;
        transaction.commit()?;
        Ok(())
    })
    .await?
}

fn record_repository_view_in_transaction(
    transaction: &Transaction<'_>,
    owner_table: &'static str,
    owner_id: &str,
    repository_view: &RepositoryViewReference,
) -> Result<()> {
    debug_assert!(matches!(owner_table, "tasks" | "runs"));
    let baseline_snapshot_id = repository_view
        .baseline_snapshot_id
        .as_ref()
        .map(SnapshotId::as_str);
    let overlay_revision_id = repository_view
        .overlay_revision_id
        .as_ref()
        .map(OverlayRevisionId::as_str);
    let view_snapshot_id = repository_view
        .view_snapshot_id
        .as_ref()
        .map(SnapshotId::as_str);
    let tree_algorithm = repository_view
        .frozen_source_tree
        .as_ref()
        .map(Digest::algorithm);
    let tree_digest = repository_view
        .frozen_source_tree
        .as_ref()
        .map(Digest::value);
    let lifecycle = match repository_view.lifecycle {
        TaskViewLifecycle::Mutable => "mutable",
        TaskViewLifecycle::FrozenSubmitted => "frozen_submitted",
    };
    let status = repository_view.status.as_str();
    let sql = format!(
        "UPDATE {owner_table} SET baseline_snapshot_id = ?1, overlay_revision_id = ?2, \
         repository_view_snapshot_id = ?3, repository_view_tree_algorithm = ?4, \
         repository_view_tree_digest = ?5, repository_view_lifecycle = ?6, \
         repository_view_status = ?7 WHERE id = ?8"
    );
    let updated = transaction.execute(
        &sql,
        params![
            baseline_snapshot_id,
            overlay_revision_id,
            view_snapshot_id,
            tree_algorithm,
            tree_digest,
            lifecycle,
            status,
            owner_id,
        ],
    )?;
    if updated == 0 {
        anyhow::bail!("Cannot record repository view: {owner_table} row does not exist");
    }
    if owner_table == "tasks" {
        transaction.execute(
            r#"
            UPDATE runs
            SET baseline_snapshot_id = ?1,
                overlay_revision_id = ?2,
                repository_view_snapshot_id = ?3,
                repository_view_tree_algorithm = ?4,
                repository_view_tree_digest = ?5,
                repository_view_lifecycle = ?6,
                repository_view_status = ?7
            WHERE task_id = ?8
              AND baseline_snapshot_id IS NULL
              AND repository_view_status IN ('not_built', 'unavailable', 'failed')
            "#,
            params![
                baseline_snapshot_id,
                overlay_revision_id,
                view_snapshot_id,
                tree_algorithm,
                tree_digest,
                lifecycle,
                status,
                owner_id,
            ],
        )?;
    }
    Ok(())
}

type RepositoryViewDatabaseValues = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    String,
);

fn repository_view_reference_from_database(
    values: RepositoryViewDatabaseValues,
) -> Result<RepositoryViewReference> {
    let (
        baseline_snapshot_id,
        overlay_revision_id,
        view_snapshot_id,
        tree_algorithm,
        tree_digest,
        lifecycle,
        status,
    ) = values;
    let baseline_snapshot_id = baseline_snapshot_id
        .map(SnapshotId::new)
        .transpose()
        .context("Invalid baseline snapshot identity in ferrus.db")?;
    let overlay_revision_id = overlay_revision_id
        .map(OverlayRevisionId::new)
        .transpose()
        .context("Invalid overlay revision identity in ferrus.db")?;
    let view_snapshot_id = view_snapshot_id
        .map(SnapshotId::new)
        .transpose()
        .context("Invalid materialized repository view snapshot in ferrus.db")?;
    let frozen_source_tree = match (tree_algorithm, tree_digest) {
        (Some(algorithm), Some(value)) => Some(
            Digest::new(algorithm, value)
                .context("Invalid frozen repository source tree in ferrus.db")?,
        ),
        (None, None) => None,
        _ => anyhow::bail!("Incomplete frozen repository source tree in ferrus.db"),
    };
    let lifecycle = match lifecycle.as_str() {
        "mutable" => TaskViewLifecycle::Mutable,
        "frozen_submitted" => TaskViewLifecycle::FrozenSubmitted,
        _ => anyhow::bail!("Unknown repository view lifecycle in ferrus.db: {lifecycle:?}"),
    };
    let reference = RepositoryViewReference {
        baseline_snapshot_id,
        overlay_revision_id,
        view_snapshot_id,
        frozen_source_tree,
        lifecycle,
        status: RepositoryViewStatus::from_database(&status)?,
    };
    reference.validate()?;
    Ok(reference)
}

pub(super) fn repository_view_reference_from_row(
    row: &rusqlite::Row<'_>,
    offset: usize,
) -> rusqlite::Result<RepositoryViewReference> {
    repository_view_reference_from_database((
        row.get(offset)?,
        row.get(offset + 1)?,
        row.get(offset + 2)?,
        row.get(offset + 3)?,
        row.get(offset + 4)?,
        row.get(offset + 5)?,
        row.get(offset + 6)?,
    ))
    .map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(offset, rusqlite::types::Type::Text, error.into())
    })
}
