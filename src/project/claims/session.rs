//! Managed session ownership and lease-bound operations.

use super::*;

/// Host-owned exact-run fence shared by native task-scoped agents. Never deserialize tool arguments into this.
#[derive(Debug, Clone)]
pub(crate) struct ExecutorSessionScope {
    pub database_path: PathBuf,
    pub agent_id: String,
    pub task_id: String,
    pub run_id: String,
    pub workspace_path: PathBuf,
}

pub(crate) async fn claim_executor_session(
    scope: &ExecutorSessionScope,
    ttl_secs: u64,
) -> Result<ReadyTaskClaim> {
    with_executor_session(scope, true, move |transaction, scope, context| {
        // A relaunched native Executor resumes work waits, never a Supervisor's
        // human question or a paused review phase.
        if context.status == TaskStatus::AwaitingHuman.as_str() {
            let (owner, resume): (Option<String>, Option<String>) = transaction.query_row(
                "SELECT awaiting_human_by, awaiting_human_status FROM tasks WHERE id = ?1",
                [&scope.task_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if owner.as_deref() != Some(&scope.agent_id)
                || !matches!(
                    resume.as_deref().or(context.paused_status.as_deref()),
                    Some("executing" | "addressing")
                )
            {
                return Ok(ReadyTaskClaim::NoAvailable);
            }
        }

        if context.status == TaskStatus::Consultation.as_str()
            && !matches!(
                context.paused_status.as_deref(),
                Some("executing" | "addressing")
            )
        {
            return Ok(ReadyTaskClaim::NoAvailable);
        }

        claim_ready_task_in_transaction(
            transaction,
            &scope.task_id,
            &scope.agent_id,
            ttl_secs,
            &[
                TaskStatus::Pending,
                TaskStatus::Executing,
                TaskStatus::Addressing,
                TaskStatus::AwaitingHuman,
                TaskStatus::Consultation,
            ],
            true,
        )
    })
    .await
}

pub(crate) async fn executor_session_status(
    scope: &ExecutorSessionScope,
) -> Result<RuntimeTaskContext> {
    with_executor_session(scope, false, |_, _, context| Ok(context)).await
}

pub(crate) async fn renew_executor_session_lease(
    scope: &ExecutorSessionScope,
    ttl_secs: u64,
) -> Result<LeaseRenewal> {
    with_executor_session(scope, true, move |transaction, scope, _| {
        let task =
            task_candidate_by_id(transaction, &scope.task_id)?.context("Bound task is missing")?;
        if task.claimed_by.as_deref() != Some(&scope.agent_id) {
            return Ok(LeaseRenewal::NotClaimed);
        }

        anyhow::ensure!(
            matches!(
                task.status.as_str(),
                "executing" | "addressing" | "consultation" | "awaiting_human"
            ),
            "Bound task is outside the Executor work phase"
        );
        let Some(lease_until) = renew_task_lease_in_transaction(
            transaction,
            &scope.task_id,
            &scope.agent_id,
            ttl_secs,
            task.lease_until.as_deref(),
        )?
        else {
            return Ok(LeaseRenewal::Expired);
        };
        Ok(LeaseRenewal::Renewed {
            task_id: task.id,
            task_path: task.path,
            claimed_by: scope.agent_id.clone(),
            lease_until,
        })
    })
    .await
}

pub(crate) async fn with_executor_session<T, F>(
    scope: &ExecutorSessionScope,
    write: bool,
    operation: F,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Transaction<'_>, &ExecutorSessionScope, RuntimeTaskContext) -> Result<T>
        + Send
        + 'static,
{
    with_role_session(scope, write, "executor", operation).await
}

pub(crate) async fn with_supervisor_session<T, F>(
    scope: &ExecutorSessionScope,
    write: bool,
    operation: F,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Transaction<'_>, &ExecutorSessionScope, RuntimeTaskContext) -> Result<T>
        + Send
        + 'static,
{
    with_role_session(scope, write, "supervisor", operation).await
}

pub(super) async fn with_role_session<T, F>(
    scope: &ExecutorSessionScope,
    write: bool,
    role: &'static str,
    operation: F,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Transaction<'_>, &ExecutorSessionScope, RuntimeTaskContext) -> Result<T>
        + Send
        + 'static,
{
    let scope = scope.clone();
    tokio::task::spawn_blocking(move || {
        let flags = if write {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        };
        // HQ owns database preparation. A missing or old database is not a cue
        // for a child to create state or import legacy runtime artifacts.
        let mut connection = Connection::open_with_flags(&scope.database_path, flags)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        let transaction = connection.transaction_with_behavior(if write {
            TransactionBehavior::Immediate
        } else {
            TransactionBehavior::Deferred
        })?;
        validate_runtime_migration_history(&transaction)?;
        anyhow::ensure!(
            runtime_schema_version(&transaction)? == RUNTIME_SCHEMA_VERSION,
            "Managed session requires the current runtime schema"
        );
        let context = executor_context_in_transaction(&transaction, &scope, role)?;
        let result = operation(&transaction, &scope, context)?;
        transaction.commit()?;
        Ok(result)
    })
    .await?
}

/// Exact-run, live-owner fence used by native lifecycle mutations in this transaction.
pub(crate) fn require_executor_owner(
    transaction: &Transaction<'_>,
    scope: &ExecutorSessionScope,
) -> Result<()> {
    let task =
        task_candidate_by_id(transaction, &scope.task_id)?.context("Bound task is missing")?;

    anyhow::ensure!(
        task.claimed_by.as_deref() == Some(&scope.agent_id),
        "Executor lease lost"
    );

    anyhow::ensure!(
        parse_lease_until(task.lease_until.as_deref()).is_some_and(|until| Utc::now() < until),
        "Executor lease expired"
    );

    Ok(())
}

pub(crate) fn executor_event(
    transaction: &Transaction<'_>,
    scope: &ExecutorSessionScope,
    kind: &str,
    payload: serde_json::Value,
) -> Result<()> {
    insert_event_in_transaction(transaction, Some(&scope.run_id), kind, &payload)
}

pub(super) fn executor_context_in_transaction(
    transaction: &Transaction<'_>,
    scope: &ExecutorSessionScope,
    role: &str,
) -> Result<RuntimeTaskContext> {
    let run = transaction
        .query_row(
            "SELECT task_id, agent, role, status, workspace_path FROM runs WHERE id = ?1",
            [&scope.run_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?
        .context("Bound run is missing")?;
    anyhow::ensure!(run.0 == scope.task_id, "Run task binding mismatch");
    anyhow::ensure!(run.1 == scope.agent_id, "Run agent binding mismatch");
    anyhow::ensure!(run.2 == role, "Run role mismatch");
    anyhow::ensure!(
        matches!(run.3.as_str(), "running" | "checking"),
        "Bound run is no longer active"
    );
    anyhow::ensure!(
        Path::new(&run.4).is_absolute() && std::fs::canonicalize(&run.4)? == scope.workspace_path,
        "Run workspace binding mismatch"
    );
    let mut context = transaction
        .query_row(
            "SELECT path, spec_path, milestone_id, status, paused_status, check_retries,
                review_cycles, failure_reason, baseline_snapshot_id, overlay_revision_id,
                repository_view_snapshot_id, repository_view_tree_algorithm,
                repository_view_tree_digest, repository_view_lifecycle, repository_view_status
         FROM tasks WHERE id = ?1",
            [&scope.task_id],
            |row| {
                Ok(RuntimeTaskContext {
                    task_id: scope.task_id.clone(),
                    task_path: row.get(0)?,
                    spec_path: row.get(1)?,
                    milestone_id: row.get(2)?,
                    status: row.get(3)?,
                    paused_status: row.get(4)?,
                    check_retries: row.get::<_, i64>(5)? as u32,
                    review_cycles: row.get::<_, i64>(6)? as u32,
                    failure_reason: row.get(7)?,
                    run_dir: run_dir_for_task(&scope.task_id),
                    run_id: Some(scope.run_id.clone()),
                    run_role: Some(run.2.clone()),
                    workspace_path: Some(run.4.clone()),
                    repository_workspace_path: Some(run.4.clone()),
                    repository_view: graph::repository_view_reference_from_row(row, 8)?,
                })
            },
        )
        .optional()?
        .context("Bound task is missing")?;
    if role == "supervisor" && matches!(context.status.as_str(), "reviewing" | "awaiting_human") {
        context.repository_view = transaction.query_row(
            "SELECT baseline_snapshot_id, overlay_revision_id, repository_view_snapshot_id, repository_view_tree_algorithm, repository_view_tree_digest, repository_view_lifecycle, repository_view_status FROM runs WHERE id = ?1",
            [&scope.run_id], |row| graph::repository_view_reference_from_row(row, 0))?;
    }

    Ok(context)
}
