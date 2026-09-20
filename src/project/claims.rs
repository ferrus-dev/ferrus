//! Transactional task claims, lease renewal, and agent-scoped runtime context resolution.

use super::*;

/// Host-owned authority for one managed Executor run. Never deserialize tool arguments into this.
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
        // A relaunched native Executor resumes only its own human wait, never a
        // Supervisor/Consultant question or a paused review/consultation phase.
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

// Validate the exact run in the same transaction as its effects. Looking up the
// latest run or lease by agent alone can silently retarget an old child process.
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
        let context = executor_context_in_transaction(&transaction, &scope)?;
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

fn executor_context_in_transaction(
    transaction: &Transaction<'_>,
    scope: &ExecutorSessionScope,
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
    anyhow::ensure!(
        run.2 == crate::agent_id::ROLE_EXECUTOR,
        "Run role must be executor"
    );
    anyhow::ensure!(
        matches!(run.3.as_str(), "running" | "checking"),
        "Bound run is no longer active"
    );
    anyhow::ensure!(
        Path::new(&run.4).is_absolute() && std::fs::canonicalize(&run.4)? == scope.workspace_path,
        "Run workspace binding mismatch"
    );
    transaction
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
        .context("Bound task is missing")
}

pub async fn claim_task(
    task_id: &str,
    task_path: &str,
    agent_id: &str,
    ttl_secs: u64,
) -> Result<TaskClaim> {
    let database_path = current_database_path().await?;
    claim_task_in_database(
        database_path,
        task_id.to_string(),
        task_path.to_string(),
        agent_id,
        ttl_secs,
    )
    .await
}

#[allow(dead_code)]
pub async fn claim_next_ready_task(agent_id: &str, ttl_secs: u64) -> Result<ReadyTaskClaim> {
    claim_next_task_with_statuses(
        agent_id,
        ttl_secs,
        &[
            TaskStatus::Pending,
            TaskStatus::Executing,
            TaskStatus::Addressing,
        ],
        true,
    )
    .await
}

pub async fn claim_ready_task_by_id(
    task_id: &str,
    agent_id: &str,
    ttl_secs: u64,
) -> Result<ReadyTaskClaim> {
    claim_task_by_id_with_statuses(
        task_id,
        agent_id,
        ttl_secs,
        &[
            TaskStatus::Pending,
            TaskStatus::Executing,
            TaskStatus::Addressing,
        ],
        true,
    )
    .await
}

pub async fn claim_review_task_by_id(
    task_id: &str,
    agent_id: &str,
    ttl_secs: u64,
) -> Result<ReadyTaskClaim> {
    claim_task_by_id_with_statuses(task_id, agent_id, ttl_secs, &[TaskStatus::Reviewing], false)
        .await
}

// Hold an immediate transaction across selection and lease assignment so two
// callers cannot both acquire the same unclaimed or expired task.
async fn claim_task_by_id_with_statuses(
    task_id: &str,
    agent_id: &str,
    ttl_secs: u64,
    allowed_statuses: &[TaskStatus],
    promote_pending: bool,
) -> Result<ReadyTaskClaim> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let agent_id = agent_id.to_string();
    let allowed_statuses = allowed_statuses.to_vec();
    tokio::task::spawn_blocking(move || -> Result<ReadyTaskClaim> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let claim = claim_ready_task_in_transaction(
            &transaction,
            &task_id,
            &agent_id,
            ttl_secs,
            &allowed_statuses,
            promote_pending,
        )?;
        transaction.commit()?;
        Ok(claim)
    })
    .await?
}

fn claim_ready_task_in_transaction(
    transaction: &Transaction<'_>,
    task_id: &str,
    agent_id: &str,
    ttl_secs: u64,
    allowed_statuses: &[TaskStatus],
    promote_pending: bool,
) -> Result<ReadyTaskClaim> {
    let now = Utc::now();
    let Some(mut candidate) = task_candidate_by_id(transaction, task_id)? else {
        return Ok(ReadyTaskClaim::NoAvailable);
    };

    if !allowed_statuses
        .iter()
        .any(|status| status.as_str() == candidate.status)
    {
        return Ok(ReadyTaskClaim::NoAvailable);
    }

    let lease_until = parse_lease_until(candidate.lease_until.as_deref());
    let lease_active = lease_until
        .as_ref()
        .is_some_and(|lease_until| now < *lease_until);
    if lease_active && candidate.claimed_by.as_deref() != Some(agent_id) {
        return Ok(ReadyTaskClaim::NoAvailable);
    }
    if promote_pending && candidate.status == TaskStatus::Pending.as_str() {
        promote_pending_task_in_transaction(transaction, &mut candidate)?;
    }
    if lease_active && candidate.claimed_by.as_deref() == Some(agent_id) {
        return Ok(ReadyTaskClaim::AlreadyClaimed(TaskLease {
            task_id: candidate.id,
            task_path: candidate.path,
            status: candidate.status,
            paused_status: candidate.paused_status,
            check_retries: candidate.check_retries,
            review_cycles: candidate.review_cycles,
            failure_reason: candidate.failure_reason,
            claimed_by: agent_id.to_string(),
            lease_until: lease_until.expect("active lease exists"),
        }));
    }
    let lease_until =
        now + chrono::Duration::try_seconds(ttl_secs as i64).unwrap_or(chrono::Duration::MAX);
    claim_task_in_transaction(transaction, &candidate.id, agent_id, lease_until, now)?;
    Ok(ReadyTaskClaim::Claimed(TaskLease {
        task_id: candidate.id,
        task_path: candidate.path,
        status: candidate.status,
        paused_status: candidate.paused_status,
        check_retries: candidate.check_retries,
        review_cycles: candidate.review_cycles,
        failure_reason: candidate.failure_reason,
        claimed_by: agent_id.to_string(),
        lease_until,
    }))
}

pub async fn claim_next_review_task(agent_id: &str, ttl_secs: u64) -> Result<ReadyTaskClaim> {
    claim_next_task_with_statuses(agent_id, ttl_secs, &[TaskStatus::Reviewing], false).await
}

// Queue scans need the same write reservation as claims by ID; a read followed
// by a separate update would allow competing callers to select the same task.
async fn claim_next_task_with_statuses(
    agent_id: &str,
    ttl_secs: u64,
    statuses: &[TaskStatus],
    promote_pending: bool,
) -> Result<ReadyTaskClaim> {
    let database_path = current_database_path().await?;
    let agent_id = agent_id.to_string();
    let statuses = statuses.to_vec();
    tokio::task::spawn_blocking(move || -> Result<ReadyTaskClaim> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = Utc::now();
        let mut candidates = task_candidates_by_status(&transaction, &statuses)?;

        // Repeated polls must return the caller's current lease before taking new work.
        for candidate in &mut candidates {
            let lease_until = parse_lease_until(candidate.lease_until.as_deref());
            let lease_active = lease_until
                .as_ref()
                .is_some_and(|lease_until| now < *lease_until);
            if lease_active && candidate.claimed_by.as_deref() == Some(agent_id.as_str()) {
                if promote_pending && candidate.status == TaskStatus::Pending.as_str() {
                    promote_pending_task_in_transaction(&transaction, candidate)?;
                }
                transaction.commit()?;
                return Ok(ReadyTaskClaim::AlreadyClaimed(TaskLease {
                    task_id: candidate.id.clone(),
                    task_path: candidate.path.clone(),
                    status: candidate.status.clone(),
                    paused_status: candidate.paused_status.clone(),
                    check_retries: candidate.check_retries,
                    review_cycles: candidate.review_cycles,
                    failure_reason: candidate.failure_reason.clone(),
                    claimed_by: agent_id,
                    lease_until: lease_until.expect("active lease exists"),
                }));
            }
        }

        for mut candidate in candidates {
            let lease_until = parse_lease_until(candidate.lease_until.as_deref());
            let lease_active = lease_until
                .as_ref()
                .is_some_and(|lease_until| now < *lease_until);
            if lease_active {
                continue;
            }

            if promote_pending && candidate.status == TaskStatus::Pending.as_str() {
                promote_pending_task_in_transaction(&transaction, &mut candidate)?;
            }

            let lease_until = now
                + chrono::Duration::try_seconds(ttl_secs as i64).unwrap_or(chrono::Duration::MAX);
            claim_task_in_transaction(&transaction, &candidate.id, &agent_id, lease_until, now)?;
            transaction.commit()?;
            return Ok(ReadyTaskClaim::Claimed(TaskLease {
                task_id: candidate.id,
                task_path: candidate.path,
                status: candidate.status,
                paused_status: candidate.paused_status,
                check_retries: candidate.check_retries,
                review_cycles: candidate.review_cycles,
                failure_reason: candidate.failure_reason,
                claimed_by: agent_id,
                lease_until,
            }));
        }

        transaction.commit()?;
        Ok(ReadyTaskClaim::NoAvailable)
    })
    .await?
}

fn promote_pending_task_in_transaction(
    transaction: &Transaction<'_>,
    candidate: &mut ReadyTaskCandidate,
) -> Result<()> {
    transaction.execute(
        "UPDATE tasks SET status = ?1, paused_status = NULL WHERE id = ?2 AND status = ?3",
        params![
            TaskStatus::Executing.as_str(),
            candidate.id,
            TaskStatus::Pending.as_str()
        ],
    )?;
    insert_event_in_transaction(
        transaction,
        None,
        "task_scheduled",
        &serde_json::json!({
            "task_id": candidate.id,
            "previous_status": candidate.status,
            "status": TaskStatus::Executing.as_str(),
            "scheduled_at": timestamp(),
        }),
    )?;
    candidate.status = TaskStatus::Executing.as_str().to_string();
    candidate.paused_status = None;
    Ok(())
}

async fn claim_task_in_database(
    database_path: PathBuf,
    task_id: String,
    task_path: String,
    agent_id: &str,
    ttl_secs: u64,
) -> Result<TaskClaim> {
    let agent_id = agent_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<TaskClaim> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_task_exists(&transaction, &task_id, &task_path)?;
        let existing: Option<(Option<String>, Option<String>)> = transaction
            .query_row(
                "SELECT claimed_by, lease_until FROM tasks WHERE id = ?1",
                [&task_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (claimed_by, lease_until) = existing.unwrap_or((None, None));
        let now = Utc::now();
        let existing_lease = lease_until
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc));
        let lease_active = existing_lease.is_some_and(|lease_until| now < lease_until);

        if lease_active && claimed_by.as_deref() == Some(agent_id.as_str()) {
            renew_task_lease_in_transaction(
                &transaction,
                &task_id,
                &agent_id,
                ttl_secs,
                lease_until.as_deref(),
            )?;
            transaction.commit()?;
            return Ok(TaskClaim::AlreadyClaimed);
        }
        if lease_active {
            transaction.commit()?;
            return Ok(TaskClaim::ClaimedByOther {
                claimed_by: claimed_by.unwrap_or_else(|| "unknown".to_string()),
            });
        }

        let lease_until =
            now + chrono::Duration::try_seconds(ttl_secs as i64).unwrap_or(chrono::Duration::MAX);
        claim_task_in_transaction(&transaction, &task_id, &agent_id, lease_until, now)?;
        transaction.commit()?;
        Ok(TaskClaim::Claimed)
    })
    .await?
}

pub async fn renew_claimed_task_lease(agent_id: &str, ttl_secs: u64) -> Result<LeaseRenewal> {
    let database_path = current_database_path().await?;
    let agent_id = agent_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<LeaseRenewal> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        let existing: Option<(String, String, Option<String>)> = transaction
            .query_row(
                r#"
                SELECT id, path, lease_until
                FROM tasks
                WHERE claimed_by = ?1
                ORDER BY
                    CASE WHEN lease_until IS NULL THEN 1 ELSE 0 END,
                    lease_until DESC,
                    id
                LIMIT 1
                "#,
                [&agent_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((task_id, task_path, lease_until)) = existing else {
            transaction.commit()?;
            return Ok(LeaseRenewal::NotClaimed);
        };

        let Some(lease_until) = renew_task_lease_in_transaction(
            &transaction,
            &task_id,
            &agent_id,
            ttl_secs,
            lease_until.as_deref(),
        )?
        else {
            transaction.commit()?;
            return Ok(LeaseRenewal::Expired);
        };
        transaction.commit()?;
        Ok(LeaseRenewal::Renewed {
            task_id,
            task_path,
            claimed_by: agent_id,
            lease_until,
        })
    })
    .await?
}

pub async fn runtime_task_context_for_agent(agent_id: &str) -> Result<Option<RuntimeTaskContext>> {
    runtime_task_context_for_agent_with_open_mode(agent_id, false).await
}

pub(crate) async fn runtime_task_context_for_agent_read_only(
    agent_id: &str,
) -> Result<Option<RuntimeTaskContext>> {
    runtime_task_context_for_agent_with_open_mode(agent_id, true).await
}

async fn runtime_task_context_for_agent_with_open_mode(
    agent_id: &str,
    read_only: bool,
) -> Result<Option<RuntimeTaskContext>> {
    let database_path = current_database_path().await?;
    let agent_id = agent_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<RuntimeTaskContext>> {
        let connection = if read_only {
            let connection =
                Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .with_context(|| {
                        format!("Failed to open {} read-only", database_path.display())
                    })?;
            connection.busy_timeout(Duration::from_secs(5))?;
            connection
        } else {
            open_runtime_database(&database_path)?
        };
        if let Some((
            task_id,
            task_path,
            spec_path,
            milestone_id,
            status,
            paused_status,
            check_retries,
            review_cycles,
            failure_reason,
            repository_view,
        )) = connection
            .query_row(
                r#"
                SELECT id, path, spec_path, milestone_id, status, paused_status,
                       check_retries, review_cycles, failure_reason,
                       baseline_snapshot_id, overlay_revision_id, repository_view_snapshot_id,
                       repository_view_tree_algorithm, repository_view_tree_digest,
                       repository_view_lifecycle, repository_view_status
                FROM tasks
                WHERE claimed_by = ?1
                ORDER BY
                    CASE WHEN lease_until IS NULL THEN 1 ELSE 0 END,
                    lease_until DESC,
                    id
                LIMIT 1
                "#,
                [&agent_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, i64>(6)? as u32,
                        row.get::<_, i64>(7)? as u32,
                        row.get::<_, Option<String>>(8)?,
                        graph::repository_view_reference_from_row(row, 9)?,
                    ))
                },
            )
            .optional()?
        {
            let run = latest_run_for_agent_task(&connection, &agent_id, &task_id)?;
            let run_role = run.as_ref().map(|run| run.role.clone());
            let workspace_path = run.as_ref().map(|run| run.workspace_path.clone());
            let repository_workspace_path = match run_role.as_deref() {
                Some("executor") => workspace_path.clone(),
                Some("supervisor") if status != TaskStatus::Reviewing.as_str() => {
                    latest_executor_workspace_for_task(&connection, &task_id)?
                }
                _ => None,
            };
            let repository_view = match run.as_ref() {
                Some(run)
                    if run.role == "supervisor" && status == TaskStatus::Reviewing.as_str() =>
                {
                    run.repository_view.clone()
                }
                _ => repository_view,
            };
            return Ok(Some(RuntimeTaskContext {
                run_dir: run_dir_for_task(&task_id),
                task_id,
                task_path,
                spec_path,
                milestone_id,
                status,
                paused_status,
                check_retries,
                review_cycles,
                failure_reason,
                run_id: run.as_ref().map(|run| run.id.clone()),
                run_role,
                workspace_path,
                repository_workspace_path,
                repository_view,
            }));
        }

        let context = connection
            .query_row(
                r#"
                SELECT runs.id, runs.role, runs.workspace_path,
                       tasks.id, tasks.path, tasks.spec_path, tasks.milestone_id,
                       tasks.status, tasks.paused_status,
                       tasks.check_retries, tasks.review_cycles, tasks.failure_reason,
                       runs.baseline_snapshot_id, runs.overlay_revision_id,
                       runs.repository_view_snapshot_id,
                       runs.repository_view_tree_algorithm,
                       runs.repository_view_tree_digest,
                       runs.repository_view_lifecycle, runs.repository_view_status
                FROM runs
                JOIN tasks ON tasks.id = runs.task_id
                WHERE runs.agent = ?1 AND runs.status IN ('running', 'checking', 'reviewing')
                ORDER BY runs.updated_at DESC, runs.started_at DESC, runs.id DESC
                LIMIT 1
                "#,
                [&agent_id],
                |row| {
                    let run_id = row.get::<_, String>(0)?;
                    let run_role = row.get::<_, String>(1)?;
                    let workspace_path = row.get::<_, String>(2)?;
                    let task_id = row.get::<_, String>(3)?;
                    let repository_workspace_path = if run_role == "executor" {
                        Some(workspace_path.clone())
                    } else if run_role == "supervisor"
                        && row.get::<_, String>(7)? != TaskStatus::Reviewing.as_str()
                    {
                        latest_executor_workspace_for_task(&connection, &task_id)?
                    } else {
                        None
                    };
                    Ok(RuntimeTaskContext {
                        run_dir: run_dir_for_task(&task_id),
                        task_id,
                        task_path: row.get(4)?,
                        spec_path: row.get(5)?,
                        milestone_id: row.get(6)?,
                        status: row.get(7)?,
                        paused_status: row.get(8)?,
                        check_retries: row.get::<_, i64>(9)? as u32,
                        review_cycles: row.get::<_, i64>(10)? as u32,
                        failure_reason: row.get(11)?,
                        run_id: Some(run_id),
                        run_role: Some(run_role),
                        workspace_path: Some(workspace_path),
                        repository_workspace_path,
                        repository_view: graph::repository_view_reference_from_row(row, 12)?,
                    })
                },
            )
            .optional()?;
        Ok(context)
    })
    .await?
}

fn renew_task_lease_in_transaction(
    transaction: &Transaction<'_>,
    task_id: &str,
    agent_id: &str,
    ttl_secs: u64,
    existing_lease: Option<&str>,
) -> Result<Option<DateTime<Utc>>> {
    let now = Utc::now();
    let existing_lease = existing_lease
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc));
    if existing_lease.is_none_or(|lease_until| now >= lease_until) {
        return Ok(None);
    }

    let lease_until =
        now + chrono::Duration::try_seconds(ttl_secs as i64).unwrap_or(chrono::Duration::MAX);
    let lease_until_text = lease_until.to_rfc3339_opts(SecondsFormat::Secs, true);
    let now_text = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    transaction.execute(
        "UPDATE tasks SET lease_until = ?1, last_heartbeat = ?2 WHERE id = ?3",
        params![lease_until_text, now_text, task_id],
    )?;
    insert_event_in_transaction(
        transaction,
        None,
        "task_lease_renewed",
        &serde_json::json!({
            "task_id": task_id,
            "claimed_by": agent_id,
            "lease_until": lease_until,
        }),
    )?;
    Ok(Some(lease_until))
}

#[derive(Debug, Clone)]
struct RuntimeRunIdentity {
    id: String,
    role: String,
    workspace_path: String,
    repository_view: RepositoryViewReference,
}

fn latest_run_for_agent_task(
    connection: &Connection,
    agent_id: &str,
    task_id: &str,
) -> Result<Option<RuntimeRunIdentity>> {
    Ok(connection
        .query_row(
            r#"
            SELECT id, role, workspace_path,
                   baseline_snapshot_id, overlay_revision_id, repository_view_snapshot_id,
                   repository_view_tree_algorithm, repository_view_tree_digest,
                   repository_view_lifecycle, repository_view_status
            FROM runs
            WHERE agent = ?1 AND task_id = ?2
            ORDER BY updated_at DESC, started_at DESC, id DESC
            LIMIT 1
            "#,
            params![agent_id, task_id],
            |row| {
                Ok(RuntimeRunIdentity {
                    id: row.get(0)?,
                    role: row.get(1)?,
                    workspace_path: row.get(2)?,
                    repository_view: graph::repository_view_reference_from_row(row, 3)?,
                })
            },
        )
        .optional()?)
}

pub(super) fn latest_executor_workspace_for_task(
    connection: &Connection,
    task_id: &str,
) -> rusqlite::Result<Option<String>> {
    connection
        .query_row(
            r#"
            SELECT workspace_path
            FROM runs
            WHERE task_id = ?1 AND role = 'executor' AND workspace_path <> ''
            ORDER BY updated_at DESC, started_at DESC, id DESC
            LIMIT 1
            "#,
            [task_id],
            |row| row.get(0),
        )
        .optional()
}

pub(super) fn latest_active_run_for_agent(
    connection: &Connection,
    agent_id: &str,
) -> Result<Option<String>> {
    Ok(connection
        .query_row(
            r#"
            SELECT id
            FROM runs
            WHERE agent = ?1 AND status IN ('running', 'checking', 'reviewing')
            ORDER BY updated_at DESC, started_at DESC, id DESC
            LIMIT 1
            "#,
            [agent_id],
            |row| row.get(0),
        )
        .optional()?)
}

pub(super) fn consultation_context_for_run(
    connection: &Connection,
    run_id: &str,
) -> Result<Option<RuntimeTaskContext>> {
    Ok(connection
        .query_row(
            r#"
            SELECT tasks.id, tasks.path, tasks.spec_path, tasks.milestone_id,
                   tasks.status, tasks.paused_status,
                   tasks.check_retries, tasks.review_cycles, tasks.failure_reason,
                   tasks.baseline_snapshot_id, tasks.overlay_revision_id,
                   tasks.repository_view_snapshot_id,
                   tasks.repository_view_tree_algorithm,
                   tasks.repository_view_tree_digest,
                   tasks.repository_view_lifecycle, tasks.repository_view_status
            FROM runs
            JOIN tasks ON tasks.id = runs.task_id
            WHERE runs.id = ?1 AND tasks.status = ?2
            LIMIT 1
            "#,
            params![run_id, TaskStatus::Consultation.as_str()],
            |row| {
                let task_id = row.get::<_, String>(0)?;
                let repository_workspace_path =
                    latest_executor_workspace_for_task(connection, &task_id)?;
                Ok(RuntimeTaskContext {
                    run_dir: run_dir_for_task(&task_id),
                    task_id,
                    task_path: row.get(1)?,
                    spec_path: row.get(2)?,
                    milestone_id: row.get(3)?,
                    status: row.get(4)?,
                    paused_status: row.get(5)?,
                    check_retries: row.get::<_, i64>(6)? as u32,
                    review_cycles: row.get::<_, i64>(7)? as u32,
                    failure_reason: row.get(8)?,
                    run_id: Some(run_id.to_string()),
                    run_role: Some("supervisor".to_string()),
                    workspace_path: None,
                    repository_workspace_path,
                    repository_view: graph::repository_view_reference_from_row(row, 9)?,
                })
            },
        )
        .optional()?)
}

pub(super) fn run_dir_for_task(task_id: &str) -> String {
    format!(".ferrus/runs/{task_id}")
}

pub fn run_dir_for_task_display(task_id: &str) -> String {
    run_dir_for_task(task_id)
}

pub(super) fn default_task_path_for_id(task_id: &str) -> String {
    if task_id == CURRENT_TASK_ID {
        CURRENT_TASK_PATH.to_string()
    } else {
        format!(".ferrus/tasks/{task_id}.md")
    }
}
