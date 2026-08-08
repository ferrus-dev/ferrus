use super::*;

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
        let now = Utc::now();
        let Some(mut candidate) = task_candidate_by_id(&transaction, &task_id)? else {
            transaction.commit()?;
            return Ok(ReadyTaskClaim::NoAvailable);
        };

        if !allowed_statuses
            .iter()
            .any(|status| status.as_str() == candidate.status)
        {
            transaction.commit()?;
            return Ok(ReadyTaskClaim::NoAvailable);
        }

        if promote_pending && candidate.status == TaskStatus::Pending.as_str() {
            promote_pending_task_in_transaction(&transaction, &mut candidate)?;
        }

        let lease_until = parse_lease_until(candidate.lease_until.as_deref());
        let lease_active = lease_until
            .as_ref()
            .is_some_and(|lease_until| now < *lease_until);
        if lease_active && candidate.claimed_by.as_deref() == Some(agent_id.as_str()) {
            transaction.commit()?;
            return Ok(ReadyTaskClaim::AlreadyClaimed(TaskLease {
                task_id: candidate.id,
                task_path: candidate.path,
                status: candidate.status,
                paused_status: candidate.paused_status,
                check_retries: candidate.check_retries,
                review_cycles: candidate.review_cycles,
                failure_reason: candidate.failure_reason,
                claimed_by: agent_id,
                lease_until: lease_until.expect("active lease exists"),
            }));
        }
        if lease_active {
            transaction.commit()?;
            return Ok(ReadyTaskClaim::NoAvailable);
        }

        let lease_until =
            now + chrono::Duration::try_seconds(ttl_secs as i64).unwrap_or(chrono::Duration::MAX);
        claim_task_in_transaction(&transaction, &candidate.id, &agent_id, lease_until, now)?;
        transaction.commit()?;
        Ok(ReadyTaskClaim::Claimed(TaskLease {
            task_id: candidate.id,
            task_path: candidate.path,
            status: candidate.status,
            paused_status: candidate.paused_status,
            check_retries: candidate.check_retries,
            review_cycles: candidate.review_cycles,
            failure_reason: candidate.failure_reason,
            claimed_by: agent_id,
            lease_until,
        }))
    })
    .await?
}

pub async fn claim_next_review_task(agent_id: &str, ttl_secs: u64) -> Result<ReadyTaskClaim> {
    claim_next_task_with_statuses(agent_id, ttl_secs, &[TaskStatus::Reviewing], false).await
}

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
