//! Persist task transitions, retry counters, human/consultation pauses, and project selection.

use super::*;

pub async fn list_events(limit: usize, run_id: Option<String>) -> Result<Vec<EventRecord>> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<Vec<EventRecord>> {
        let connection = open_runtime_database(&database_path)?;
        let mut events = Vec::new();
        if let Some(run_id) = run_id {
            let mut statement = connection.prepare(
                r#"
                SELECT id, run_id, type, payload_json, created_at
                FROM events
                WHERE run_id = ?1
                ORDER BY id DESC
                LIMIT ?2
                "#,
            )?;
            let rows = statement.query_map(params![run_id, limit as i64], event_from_row)?;
            for row in rows {
                events.push(row?);
            }
        } else {
            let mut statement = connection.prepare(
                r#"
                SELECT id, run_id, type, payload_json, created_at
                FROM events
                ORDER BY id DESC
                LIMIT ?1
                "#,
            )?;
            let rows = statement.query_map([limit as i64], event_from_row)?;
            for row in rows {
                events.push(row?);
            }
        }
        Ok(events)
    })
    .await?
}

pub async fn read_project_selection() -> Result<ProjectSelection> {
    let database_path = current_database_path().await?;

    tokio::task::spawn_blocking(move || -> Result<ProjectSelection> {
        let connection = open_runtime_database_for_read(&database_path)?;
        read_project_selection_from_database(&connection)
    })
    .await?
}

pub async fn write_project_selection(selection: &ProjectSelection) -> Result<()> {
    let database_path = current_database_path().await?;
    let selection_for_db = selection.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        write_project_selection_to_database(&connection, &selection_for_db)?;
        insert_event(
            &connection,
            None,
            "project_selection_changed",
            &serde_json::json!({
                "selected_spec": selection_for_db.selected_spec,
            }),
        )?;
        Ok(())
    })
    .await??;

    Ok(())
}

pub async fn read_last_spec_path() -> Result<Option<String>> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let connection = open_runtime_database(&database_path)?;
        read_last_spec_path_from_database(&connection)
    })
    .await?
}

pub async fn write_last_spec_path(path: &str) -> Result<()> {
    let database_path = current_database_path().await?;
    let path = path.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        write_last_spec_path_to_database(&connection, Some(&path))?;
        insert_event(
            &connection,
            None,
            "spec_created",
            &serde_json::json!({ "path": path }),
        )?;
        Ok(())
    })
    .await?
}

pub async fn clear_last_spec_path() -> Result<()> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        write_last_spec_path_to_database(&connection, None)?;
        Ok(())
    })
    .await?
}

pub async fn read_last_spec_archive_path() -> Result<Option<String>> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let connection = open_runtime_database(&database_path)?;
        read_last_spec_archive_path_from_database(&connection)
    })
    .await?
}

pub async fn clear_last_spec_archive_path() -> Result<()> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        write_last_spec_archive_path_to_database(&connection, None)?;
        Ok(())
    })
    .await?
}

fn event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRecord> {
    Ok(EventRecord {
        id: row.get(0)?,
        run_id: row.get(1)?,
        event_type: row.get(2)?,
        payload_json: row.get(3)?,
        created_at: row.get(4)?,
    })
}

pub(super) fn task_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRecord> {
    Ok(TaskRecord {
        id: row.get(0)?,
        path: row.get(1)?,
        spec_path: row.get(2)?,
        milestone_id: row.get(3)?,
        status: row.get(4)?,
        paused_status: row.get(5)?,
        claimed_by: row.get(6)?,
        lease_until: row.get(7)?,
        last_heartbeat: row.get(8)?,
        check_retries: row.get::<_, i64>(9)? as u32,
        review_cycles: row.get::<_, i64>(10)? as u32,
        failure_reason: row.get(11)?,
    })
}

#[cfg(test)]
pub async fn record_current_task_status(status: TaskStatus) -> Result<()> {
    let task = current_task_record().await;
    record_task_status_with_origin(
        &task.id,
        &task.path,
        status,
        task.spec_path.as_deref(),
        task.milestone_id.as_deref(),
    )
    .await
}

pub async fn record_task_status(task_id: &str, task_path: &str, status: TaskStatus) -> Result<()> {
    record_task_status_with_origin(task_id, task_path, status, None, None).await
}

pub async fn record_task_status_with_origin(
    task_id: &str,
    task_path: &str,
    status: TaskStatus,
    spec_path: Option<&str>,
    milestone_id: Option<&str>,
) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let task_path = task_path.to_string();
    let spec_path = spec_path.map(str::to_string);
    let milestone_id = milestone_id.map(str::to_string);
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        upsert_task(
            &connection,
            &task_id,
            &task_path,
            status,
            spec_path.as_deref(),
            milestone_id.as_deref(),
        )?;
        if status.clears_lease() {
            clear_task_lease(&connection, &task_id)?;
        }
        insert_event(
            &connection,
            None,
            "task_status_changed",
            &serde_json::json!({
                "task_id": task_id,
                "status": status.as_str(),
            }),
        )?;
        Ok(())
    })
    .await?
}

/// Completes the normal submit transition and, when available, pins the exact
/// materialized graph/source pair in the same runtime transaction. A graph
/// freeze failure is diagnostic-only: it is recorded without changing submit
/// eligibility, retries, or review-cycle accounting.
pub async fn record_task_submitted(
    task_id: &str,
    task_path: &str,
    run_id: Option<&str>,
    frozen_view: Option<&RepositoryViewReference>,
    freeze_failed: bool,
) -> Result<()> {
    if let Some(view) = frozen_view {
        view.validate()?;
        if view.lifecycle != TaskViewLifecycle::FrozenSubmitted {
            anyhow::bail!("A submitted repository view must be frozen");
        }
    }
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let task_path = task_path.to_string();
    let run_id = run_id.map(str::to_string);
    let frozen_view = frozen_view.cloned();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        upsert_task(
            &transaction,
            &task_id,
            &task_path,
            TaskStatus::Reviewing,
            None,
            None,
        )?;
        clear_task_lease(&transaction, &task_id)?;
        if let Some(view) = frozen_view.as_ref() {
            update_repository_view_in_transaction(&transaction, "tasks", &task_id, view)?;
            if let Some(run_id) = run_id.as_deref() {
                update_repository_view_in_transaction(&transaction, "runs", run_id, view)?;
            }
            insert_event_in_transaction(
                &transaction,
                run_id.as_deref(),
                "repository_view_frozen",
                &serde_json::json!({
                    "task_id": task_id,
                    "snapshot_id": view.view_snapshot_id.as_ref().map(SnapshotId::as_str),
                }),
            )?;
        } else if freeze_failed {
            insert_event_in_transaction(
                &transaction,
                run_id.as_deref(),
                "repository_view_freeze_failed",
                &serde_json::json!({ "task_id": task_id }),
            )?;
        }
        insert_event_in_transaction(
            &transaction,
            run_id.as_deref(),
            "task_status_changed",
            &serde_json::json!({
                "task_id": task_id,
                "status": TaskStatus::Reviewing.as_str(),
            }),
        )?;
        transaction.commit()?;
        Ok(())
    })
    .await?
}

fn update_repository_view_in_transaction(
    transaction: &Transaction<'_>,
    owner_table: &'static str,
    owner_id: &str,
    view: &RepositoryViewReference,
) -> Result<()> {
    debug_assert!(matches!(owner_table, "tasks" | "runs"));
    let sql = format!(
        "UPDATE {owner_table} SET baseline_snapshot_id = ?1, overlay_revision_id = ?2, \
         repository_view_snapshot_id = ?3, repository_view_tree_algorithm = ?4, \
         repository_view_tree_digest = ?5, repository_view_lifecycle = ?6, \
         repository_view_status = ?7 WHERE id = ?8"
    );
    let updated = transaction.execute(
        &sql,
        params![
            view.baseline_snapshot_id.as_ref().map(SnapshotId::as_str),
            view.overlay_revision_id
                .as_ref()
                .map(OverlayRevisionId::as_str),
            view.view_snapshot_id.as_ref().map(SnapshotId::as_str),
            view.frozen_source_tree.as_ref().map(Digest::algorithm),
            view.frozen_source_tree.as_ref().map(Digest::value),
            match view.lifecycle {
                TaskViewLifecycle::Mutable => "mutable",
                TaskViewLifecycle::FrozenSubmitted => "frozen_submitted",
            },
            view.status.as_str(),
            owner_id,
        ],
    )?;
    if updated == 0 {
        anyhow::bail!("Cannot record submitted repository view: {owner_table} row does not exist");
    }
    Ok(())
}

pub async fn record_task_check_passed(task_id: &str) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        connection.execute(
            "UPDATE tasks SET check_retries = 0, failure_reason = NULL WHERE id = ?1",
            [&task_id],
        )?;
        insert_event(
            &connection,
            None,
            "task_check_passed",
            &serde_json::json!({ "task_id": task_id }),
        )?;
        Ok(())
    })
    .await?
}

#[cfg(test)]
pub async fn mirror_task_check_state(
    task_id: &str,
    status: TaskStatus,
    check_retries: u32,
    failure_reason: Option<&str>,
) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let failure_reason = failure_reason.map(str::to_string);
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        if status.clears_lease() {
            connection.execute(
                r#"
                UPDATE tasks
                SET status = ?1, check_retries = ?2, failure_reason = ?3,
                    claimed_by = NULL, lease_until = NULL, last_heartbeat = NULL
                WHERE id = ?4
                "#,
                params![status.as_str(), check_retries, failure_reason, task_id],
            )?;
        } else {
            connection.execute(
                "UPDATE tasks SET status = ?1, check_retries = ?2, failure_reason = ?3 WHERE id = ?4",
                params![status.as_str(), check_retries, failure_reason, task_id],
            )?;
        }
        insert_event(
            &connection,
            None,
            "task_check_state_mirrored",
            &serde_json::json!({
                "task_id": task_id,
                "status": status.as_str(),
                "check_retries": check_retries,
            }),
        )?;
        Ok(())
    })
    .await?
}

pub async fn record_task_integration_failed(
    task_id: &str,
    run_id: Option<&str>,
    failure_reason: &str,
) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let run_id = run_id.map(str::to_string);
    let failure_reason = failure_reason.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        connection.execute(
            "UPDATE tasks SET failure_reason = ?1 WHERE id = ?2",
            params![failure_reason, task_id],
        )?;
        insert_event(
            &connection,
            run_id.as_deref(),
            "task_integration_failed",
            &serde_json::json!({
                "task_id": task_id,
                "failure_reason": failure_reason,
            }),
        )?;
        Ok(())
    })
    .await?
}

pub async fn record_task_integration_failed_best_effort(
    task_id: &str,
    run_id: Option<&str>,
    failure_reason: &str,
) {
    if let Err(err) = record_task_integration_failed(task_id, run_id, failure_reason).await {
        warn!(error = ?err, task_id, "failed to mirror task integration failure into ferrus.db");
    }
}

pub async fn record_task_check_failed(
    task_id: &str,
    failure_reason: &str,
    max_retries: u32,
) -> Result<TaskCheckFailure> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let failure_reason = failure_reason.to_string();
    tokio::task::spawn_blocking(move || -> Result<TaskCheckFailure> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        let retries = task_check_retries(&transaction, &task_id)? + 1;
        if retries >= max_retries {
            let limit_failure_reason = format!(
                "Check failed {max_retries} consecutive times. Last failure:\n{failure_reason}"
            );
            transaction.execute(
                r#"
                UPDATE tasks
                SET status = ?1, check_retries = ?2, failure_reason = ?3,
                    claimed_by = NULL, lease_until = NULL, last_heartbeat = NULL
                WHERE id = ?4
                "#,
                params![
                    TaskStatus::Failed.as_str(),
                    retries,
                    limit_failure_reason,
                    task_id
                ],
            )?;
            insert_event_in_transaction(
                &transaction,
                None,
                "task_check_limit_exceeded",
                &serde_json::json!({
                    "task_id": task_id,
                    "retries": retries,
                    "max_retries": max_retries,
                }),
            )?;
            transaction.commit()?;
            Ok(TaskCheckFailure::LimitExceeded { retries })
        } else {
            transaction.execute(
                "UPDATE tasks SET check_retries = ?1, failure_reason = ?2 WHERE id = ?3",
                params![retries, failure_reason, task_id],
            )?;
            insert_event_in_transaction(
                &transaction,
                None,
                "task_check_failed",
                &serde_json::json!({
                    "task_id": task_id,
                    "retries": retries,
                    "max_retries": max_retries,
                }),
            )?;
            transaction.commit()?;
            Ok(TaskCheckFailure::Failed { retries })
        }
    })
    .await?
}

pub async fn record_task_review_rejected(
    task_id: &str,
    max_cycles: u32,
) -> Result<TaskReviewRejection> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<TaskReviewRejection> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        let cycles = task_review_cycles(&transaction, &task_id)? + 1;
        if cycles >= max_cycles {
            transaction.execute(
                r#"
                UPDATE tasks
                SET status = ?1, review_cycles = ?2,
                    failure_reason = ?3,
                    claimed_by = NULL, lease_until = NULL, last_heartbeat = NULL
                WHERE id = ?4
                "#,
                params![
                    TaskStatus::Failed.as_str(),
                    cycles,
                    format!("Task rejected {max_cycles} times without resolution."),
                    task_id
                ],
            )?;
            insert_event_in_transaction(
                &transaction,
                None,
                "task_review_limit_exceeded",
                &serde_json::json!({
                    "task_id": task_id,
                    "review_cycles": cycles,
                    "max_review_cycles": max_cycles,
                }),
            )?;
            transaction.commit()?;
            Ok(TaskReviewRejection::LimitExceeded { cycles })
        } else {
            transaction.execute(
                r#"
                UPDATE tasks
                SET status = ?1, review_cycles = ?2, check_retries = 0,
                    executor_dispatches = 0,
                    failure_reason = NULL,
                    repository_view_tree_algorithm = NULL,
                    repository_view_tree_digest = NULL,
                    repository_view_lifecycle = 'mutable',
                    claimed_by = NULL, lease_until = NULL, last_heartbeat = NULL
                WHERE id = ?3
                "#,
                params![TaskStatus::Addressing.as_str(), cycles, task_id],
            )?;
            insert_event_in_transaction(
                &transaction,
                None,
                "task_rejected",
                &serde_json::json!({
                    "task_id": task_id,
                    "review_cycles": cycles,
                    "max_review_cycles": max_cycles,
                }),
            )?;
            transaction.commit()?;
            Ok(TaskReviewRejection::Addressing { cycles })
        }
    })
    .await?
}

/// Accounts for one executor dispatch (HQ spawning an executor session) for a
/// task and enforces the per-work-phase ceiling. Each spawn increments the
/// counter; a session that exits without advancing the task is respawned and
/// counted again. When the budget is exhausted the task is moved to Failed so a
/// task that never reaches review cannot churn forever. The counter is reset at
/// the start of each work phase (a fresh rejection back to Addressing).
/// Gate an executor dispatch against the per-work-phase budget *without*
/// consuming it. Returns [`ExecutorDispatchOutcome::Proceed`] when another
/// session may be spawned, or transitions the task to `Failed` and returns
/// [`ExecutorDispatchOutcome::LimitExceeded`] when the budget is already spent.
///
/// This only reads the counter; account a dispatch with
/// [`record_executor_dispatch`] *after* the session has actually started, so a
/// failed worktree/process setup does not burn the budget. `max_dispatches == 0`
/// disables the guard.
pub async fn enforce_executor_dispatch_limit(
    task_id: &str,
    max_dispatches: u32,
) -> Result<ExecutorDispatchOutcome> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<ExecutorDispatchOutcome> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        let dispatches = task_executor_dispatches(&transaction, &task_id)?;
        // `max_dispatches == 0` disables the guard entirely.
        if max_dispatches != 0 && dispatches >= max_dispatches {
            transaction.execute(
                r#"
                UPDATE tasks
                SET status = ?1, failure_reason = ?2,
                    claimed_by = NULL, lease_until = NULL, last_heartbeat = NULL
                WHERE id = ?3
                "#,
                params![
                    TaskStatus::Failed.as_str(),
                    format!(
                        "Executor was dispatched {max_dispatches} times in one work phase \
                         without reaching review."
                    ),
                    task_id
                ],
            )?;
            insert_event_in_transaction(
                &transaction,
                None,
                "task_executor_dispatch_limit_exceeded",
                &serde_json::json!({
                    "task_id": task_id,
                    "executor_dispatches": dispatches,
                    "max_executor_dispatches": max_dispatches,
                }),
            )?;
            transaction.commit()?;
            return Ok(ExecutorDispatchOutcome::LimitExceeded { dispatches });
        }
        transaction.commit()?;
        Ok(ExecutorDispatchOutcome::Proceed)
    })
    .await?
}

/// Record that an executor session was successfully dispatched for `task_id`,
/// incrementing the per-work-phase counter. Call this only once the session has
/// actually started; the budget is gated separately by
/// [`enforce_executor_dispatch_limit`]. Returns the new dispatch count.
pub async fn record_executor_dispatch(task_id: &str) -> Result<u32> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<u32> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        let dispatches = task_executor_dispatches(&transaction, &task_id)? + 1;
        transaction.execute(
            "UPDATE tasks SET executor_dispatches = ?1 WHERE id = ?2",
            params![dispatches, task_id],
        )?;
        insert_event_in_transaction(
            &transaction,
            None,
            "task_executor_dispatched",
            &serde_json::json!({
                "task_id": task_id,
                "executor_dispatches": dispatches,
            }),
        )?;
        transaction.commit()?;
        Ok(dispatches)
    })
    .await?
}

pub async fn record_task_consultation_requested(
    task_id: &str,
    paused_status: TaskStatus,
) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        connection.execute(
            "UPDATE tasks SET status = ?1, paused_status = ?2 WHERE id = ?3",
            params![
                TaskStatus::Consultation.as_str(),
                paused_status.as_str(),
                task_id
            ],
        )?;
        insert_event(
            &connection,
            None,
            "task_consultation_requested",
            &serde_json::json!({
                "task_id": task_id,
                "paused_status": paused_status.as_str(),
            }),
        )?;
        Ok(())
    })
    .await?
}

pub async fn restore_task_from_consultation(task_id: &str) -> Result<TaskConsultRestore> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<TaskConsultRestore> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        let row = transaction
            .query_row(
                "SELECT status, paused_status FROM tasks WHERE id = ?1",
                [&task_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        let Some((status, paused_status)) = row else {
            transaction.commit()?;
            return Ok(TaskConsultRestore::NotInConsultation);
        };
        if status != TaskStatus::Consultation.as_str() {
            transaction.commit()?;
            return Ok(TaskConsultRestore::NotInConsultation);
        }
        let resumed_status =
            paused_status.unwrap_or_else(|| TaskStatus::Executing.as_str().to_string());
        transaction.execute(
            "UPDATE tasks SET status = ?1, paused_status = NULL WHERE id = ?2",
            params![resumed_status, task_id],
        )?;
        insert_event_in_transaction(
            &transaction,
            None,
            "task_consultation_resolved",
            &serde_json::json!({
                "task_id": task_id,
                "resumed_status": resumed_status,
            }),
        )?;
        transaction.commit()?;
        Ok(TaskConsultRestore::Restored {
            status: resumed_status,
        })
    })
    .await?
}

#[cfg(test)]
pub async fn record_task_human_question_requested(
    task_id: &str,
    paused_status: TaskStatus,
    awaiting_human_by: &str,
) -> Result<()> {
    record_task_human_question_requested_with_resume(
        task_id,
        paused_status,
        Some(paused_status),
        awaiting_human_by,
    )
    .await
}

pub async fn record_task_human_question_requested_with_resume(
    task_id: &str,
    resume_status: TaskStatus,
    paused_status: Option<TaskStatus>,
    awaiting_human_by: &str,
) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let awaiting_human_by = awaiting_human_by.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let paused_status = paused_status.map(TaskStatus::as_str);
        transaction.execute(
            r#"
            UPDATE tasks
            SET status = ?1, paused_status = ?2, awaiting_human_by = ?3,
                awaiting_human_status = ?4, human_answer_recorded = 0
            WHERE id = ?5
            "#,
            params![
                TaskStatus::AwaitingHuman.as_str(),
                paused_status,
                awaiting_human_by,
                resume_status.as_str(),
                task_id
            ],
        )?;
        insert_event_in_transaction(
            &transaction,
            None,
            "task_human_question_requested",
            &serde_json::json!({
                "task_id": task_id,
                "paused_status": paused_status,
                "resume_status": resume_status.as_str(),
                "awaiting_human_by": awaiting_human_by,
            }),
        )?;
        let question_order = transaction.last_insert_rowid();
        transaction.execute(
            "UPDATE tasks SET human_question_order = ?1 WHERE id = ?2",
            params![question_order, task_id],
        )?;
        transaction.commit()?;
        Ok(())
    })
    .await?
}

pub async fn record_task_human_answer(task_id: &str) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            r#"
            UPDATE tasks
            SET human_answer_recorded = 1
            WHERE id = ?1 AND status = ?2
            "#,
            params![task_id, TaskStatus::AwaitingHuman.as_str()],
        )?;
        if changed == 0 {
            anyhow::bail!("Task {task_id} is not waiting for a human answer.");
        }
        insert_event_in_transaction(
            &transaction,
            None,
            "task_human_answer_recorded",
            &serde_json::json!({ "task_id": task_id }),
        )?;
        transaction.commit()?;
        Ok(())
    })
    .await?
}

pub async fn record_scoped_human_answer(question: &HumanQuestion, response: &str) -> Result<()> {
    record_task_human_answer(&question.task_id).await?;
    if let Err(err) =
        crate::state::store::write_answer_for_run_dir(&question.run_dir, response).await
    {
        if let Err(rollback_err) = clear_task_human_answer_recorded(&question.task_id).await {
            return Err(err).with_context(|| {
                format!(
                    "Failed to roll back recorded human answer for {}: {rollback_err}",
                    question.task_id
                )
            });
        }
        return Err(err);
    }
    Ok(())
}

async fn clear_task_human_answer_recorded(task_id: &str) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            r#"
            UPDATE tasks
            SET human_answer_recorded = 0
            WHERE id = ?1 AND status = ?2
            "#,
            params![task_id, TaskStatus::AwaitingHuman.as_str()],
        )?;
        transaction.commit()?;
        Ok(())
    })
    .await?
}

pub async fn task_human_question_owner(task_id: &str) -> Result<Option<String>> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let connection = open_runtime_database(&database_path)?;
        let owner = connection
            .query_row(
                "SELECT awaiting_human_by FROM tasks WHERE id = ?1",
                [&task_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(owner)
    })
    .await?
}

pub async fn task_awaiting_human_status(task_id: &str) -> Result<Option<String>> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let connection = open_runtime_database(&database_path)?;
        let status = connection
            .query_row(
                "SELECT awaiting_human_status FROM tasks WHERE id = ?1",
                [&task_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(status)
    })
    .await?
}

pub async fn restore_task_from_human_answer(task_id: &str) -> Result<TaskHumanAnswerRestore> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<TaskHumanAnswerRestore> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        let row = transaction
            .query_row(
                "SELECT status, paused_status, awaiting_human_status FROM tasks WHERE id = ?1",
                [&task_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((status, paused_status, awaiting_human_status)) = row else {
            transaction.commit()?;
            return Ok(TaskHumanAnswerRestore::NotAwaitingHuman);
        };
        if status != TaskStatus::AwaitingHuman.as_str() {
            transaction.commit()?;
            return Ok(TaskHumanAnswerRestore::NotAwaitingHuman);
        }
        let resumed_status = awaiting_human_status
            .or_else(|| paused_status.clone())
            .unwrap_or_else(|| TaskStatus::Executing.as_str().to_string());
        let restored_paused_status = if resumed_status == TaskStatus::Consultation.as_str() {
            paused_status
        } else {
            None
        };
        transaction.execute(
            r#"
            UPDATE tasks
            SET status = ?1, paused_status = ?2, awaiting_human_by = NULL,
                awaiting_human_status = NULL, human_question_order = NULL,
                human_answer_recorded = 0
            WHERE id = ?3
            "#,
            params![resumed_status, restored_paused_status, task_id],
        )?;
        insert_event_in_transaction(
            &transaction,
            None,
            "task_human_answered",
            &serde_json::json!({
                "task_id": task_id,
                "resumed_status": resumed_status,
            }),
        )?;
        transaction.commit()?;
        Ok(TaskHumanAnswerRestore::Restored {
            status: resumed_status,
        })
    })
    .await?
}
