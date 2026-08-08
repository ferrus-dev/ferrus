use super::*;

pub async fn record_runtime_event(
    run_id: Option<String>,
    event_type: &str,
    payload: serde_json::Value,
) -> Result<()> {
    let database_path = current_database_path().await?;
    let event_type = event_type.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        insert_event(&connection, run_id.as_deref(), &event_type, &payload)?;
        Ok(())
    })
    .await?
}

pub async fn record_runtime_event_best_effort(
    run_id: Option<String>,
    event_type: &str,
    payload: serde_json::Value,
) {
    if let Err(err) = record_runtime_event(run_id, event_type, payload).await {
        warn!(error = ?err, event_type, "failed to write runtime event into ferrus.db");
    }
}

pub fn allocate_run_id(role: &str, agent: &str) -> String {
    generate_run_id(role, agent)
}

#[cfg(test)]
pub async fn record_run_started(role: &str, agent: &str, pid: u32) -> Result<RunRecord> {
    let run_id = allocate_run_id(role, agent);
    record_run_started_with_id(&run_id, role, agent, pid).await
}

#[cfg(test)]
pub async fn record_run_started_with_id(
    run_id: &str,
    role: &str,
    agent: &str,
    pid: u32,
) -> Result<RunRecord> {
    let workspace_path = path_string(&canonical_current_dir().await?);
    record_run_started_with_workspace(run_id, role, agent, pid, workspace_path).await
}

#[cfg(test)]
pub async fn record_run_started_with_workspace(
    run_id: &str,
    role: &str,
    agent: &str,
    pid: u32,
    workspace_path: String,
) -> Result<RunRecord> {
    record_run_started_for_task_with_workspace(run_id, role, agent, pid, None, workspace_path).await
}

pub async fn record_run_started_for_task_with_workspace(
    run_id: &str,
    role: &str,
    agent: &str,
    pid: u32,
    task_id: Option<&str>,
    workspace_path: String,
) -> Result<RunRecord> {
    let database_path = current_database_path().await?;
    let (task_id, task_path) = match task_id.map(str::trim).filter(|task_id| !task_id.is_empty()) {
        Some(task_id) => (task_id.to_string(), default_task_path_for_id(task_id)),
        None => current_task_identity().await,
    };
    let run_id = run_id.to_string();
    let role = role.to_string();
    let agent = agent.to_string();
    let started_at = timestamp();
    let updated_at = started_at.clone();
    let record = RunRecord {
        id: run_id.clone(),
        task_id: task_id.clone(),
        role,
        agent,
        status: "running".to_string(),
        started_at: started_at.clone(),
        updated_at: updated_at.clone(),
        pid: Some(pid),
        workspace_path,
    };
    let record_for_insert = record.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        ensure_task_exists(&connection, &task_id, &task_path)?;
        connection.execute(
            r#"
            INSERT INTO runs (
                id, task_id, role, agent, status, started_at, updated_at, pid, workspace_path,
                baseline_snapshot_id, overlay_revision_id, repository_view_snapshot_id,
                repository_view_tree_algorithm, repository_view_tree_digest,
                repository_view_lifecycle, repository_view_status
            )
            SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                   baseline_snapshot_id, overlay_revision_id, repository_view_snapshot_id,
                   repository_view_tree_algorithm, repository_view_tree_digest,
                   repository_view_lifecycle, repository_view_status
            FROM tasks
            WHERE id = ?2
            ON CONFLICT(id) DO UPDATE SET
                status = excluded.status,
                updated_at = excluded.updated_at,
                pid = excluded.pid,
                workspace_path = excluded.workspace_path,
                baseline_snapshot_id = excluded.baseline_snapshot_id,
                overlay_revision_id = excluded.overlay_revision_id,
                repository_view_snapshot_id = excluded.repository_view_snapshot_id,
                repository_view_tree_algorithm = excluded.repository_view_tree_algorithm,
                repository_view_tree_digest = excluded.repository_view_tree_digest,
                repository_view_lifecycle = excluded.repository_view_lifecycle,
                repository_view_status = excluded.repository_view_status
            "#,
            params![
                record_for_insert.id,
                record_for_insert.task_id,
                record_for_insert.role,
                record_for_insert.agent,
                record_for_insert.status,
                started_at,
                updated_at,
                record_for_insert.pid.map(i64::from),
                record_for_insert.workspace_path,
            ],
        )?;
        insert_event(
            &connection,
            Some(&run_id),
            "run_started",
            &serde_json::json!({
                "role": record_for_insert.role,
                "agent": record_for_insert.agent,
                "pid": record_for_insert.pid,
            }),
        )?;
        Ok(())
    })
    .await??;
    Ok(record)
}

pub async fn record_run_started_for_task_with_id_best_effort(
    run_id: &str,
    role: &str,
    agent: &str,
    pid: u32,
    task_id: Option<&str>,
    workspace_path: String,
) -> Option<String> {
    match record_run_started_for_task_with_workspace(
        run_id,
        role,
        agent,
        pid,
        task_id,
        workspace_path,
    )
    .await
    {
        Ok(record) => Some(record.id),
        Err(err) => {
            warn!(error = ?err, run_id, role, agent, pid, task_id, "failed to mirror task-scoped run start into ferrus.db");
            None
        }
    }
}

pub async fn attach_running_run_to_task(
    agent_id: &str,
    task_id: &str,
    task_path: &str,
) -> Result<Option<String>> {
    let database_path = current_database_path().await?;
    let agent_id = agent_id.to_string();
    let task_id = task_id.to_string();
    let task_path = task_path.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let connection = open_runtime_database(&database_path)?;
        ensure_task_exists(&connection, &task_id, &task_path)?;
        let run_id: Option<String> = connection
            .query_row(
                r#"
                SELECT id
                FROM runs
                WHERE agent = ?1 AND status IN ('running', 'checking', 'reviewing')
                ORDER BY started_at DESC, id DESC
                LIMIT 1
                "#,
                [&agent_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(run_id) = run_id else {
            return Ok(None);
        };

        connection.execute(
            r#"
            UPDATE runs
            SET task_id = ?1,
                updated_at = ?2,
                baseline_snapshot_id = (SELECT baseline_snapshot_id FROM tasks WHERE id = ?1),
                overlay_revision_id = (SELECT overlay_revision_id FROM tasks WHERE id = ?1),
                repository_view_snapshot_id = (
                    SELECT repository_view_snapshot_id FROM tasks WHERE id = ?1
                ),
                repository_view_tree_algorithm = (
                    SELECT repository_view_tree_algorithm FROM tasks WHERE id = ?1
                ),
                repository_view_tree_digest = (
                    SELECT repository_view_tree_digest FROM tasks WHERE id = ?1
                ),
                repository_view_lifecycle = (
                    SELECT repository_view_lifecycle FROM tasks WHERE id = ?1
                ),
                repository_view_status = (
                    SELECT repository_view_status FROM tasks WHERE id = ?1
                )
            WHERE id = ?3
            "#,
            params![task_id, timestamp(), run_id],
        )?;
        insert_event(
            &connection,
            Some(&run_id),
            "run_task_attached",
            &serde_json::json!({
                "agent": agent_id,
                "task_id": task_id,
            }),
        )?;
        Ok(Some(run_id))
    })
    .await?
}

pub async fn attach_running_run_to_task_best_effort(
    agent_id: &str,
    task_id: &str,
    task_path: &str,
) {
    if let Err(err) = attach_running_run_to_task(agent_id, task_id, task_path).await {
        warn!(
            error = ?err,
            agent_id,
            task_id,
            "failed to attach running run to task in ferrus.db"
        );
    }
}

pub async fn attach_running_run_to_next_consultation(
    agent_id: &str,
) -> Result<Option<RuntimeTaskContext>> {
    let database_path = current_database_path().await?;
    let agent_id = agent_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<RuntimeTaskContext>> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(run_id) = latest_active_run_for_agent(&transaction, &agent_id)? else {
            transaction.commit()?;
            return Ok(None);
        };

        if let Some(context) = consultation_context_for_run(&transaction, &run_id)? {
            transaction.commit()?;
            return Ok(Some(context));
        }

        let candidate = transaction
            .query_row(
                r#"
                SELECT id, path, spec_path, milestone_id, status, paused_status,
                       check_retries, review_cycles, failure_reason,
                       baseline_snapshot_id, overlay_revision_id, repository_view_snapshot_id,
                       repository_view_tree_algorithm, repository_view_tree_digest,
                       repository_view_lifecycle, repository_view_status
                FROM tasks
                WHERE status = ?1
                  AND NOT EXISTS (
                      SELECT 1
                      FROM runs
                      WHERE runs.task_id = tasks.id
                        AND runs.role = 'supervisor'
                        AND runs.status IN ('running', 'checking', 'reviewing')
                  )
                ORDER BY id
                LIMIT 1
                "#,
                [TaskStatus::Consultation.as_str()],
                |row| {
                    Ok(RuntimeTaskContext {
                        task_id: row.get(0)?,
                        task_path: row.get(1)?,
                        spec_path: row.get(2)?,
                        milestone_id: row.get(3)?,
                        run_dir: String::new(),
                        status: row.get(4)?,
                        paused_status: row.get(5)?,
                        check_retries: row.get::<_, i64>(6)? as u32,
                        review_cycles: row.get::<_, i64>(7)? as u32,
                        failure_reason: row.get(8)?,
                        run_id: Some(run_id.clone()),
                        run_role: Some("supervisor".to_string()),
                        workspace_path: None,
                        repository_workspace_path: latest_executor_workspace_for_task(
                            &transaction,
                            &row.get::<_, String>(0)?,
                        )?,
                        repository_view: graph::repository_view_reference_from_row(row, 9)?,
                    })
                },
            )
            .optional()?;
        let Some(mut context) = candidate else {
            transaction.commit()?;
            return Ok(None);
        };

        context.run_dir = run_dir_for_task(&context.task_id);
        let attached = transaction.execute(
            r#"
            UPDATE runs
            SET task_id = ?1,
                updated_at = ?2,
                baseline_snapshot_id = (SELECT baseline_snapshot_id FROM tasks WHERE id = ?1),
                overlay_revision_id = (SELECT overlay_revision_id FROM tasks WHERE id = ?1),
                repository_view_snapshot_id = (
                    SELECT repository_view_snapshot_id FROM tasks WHERE id = ?1
                ),
                repository_view_tree_algorithm = (
                    SELECT repository_view_tree_algorithm FROM tasks WHERE id = ?1
                ),
                repository_view_tree_digest = (
                    SELECT repository_view_tree_digest FROM tasks WHERE id = ?1
                ),
                repository_view_lifecycle = (
                    SELECT repository_view_lifecycle FROM tasks WHERE id = ?1
                ),
                repository_view_status = (
                    SELECT repository_view_status FROM tasks WHERE id = ?1
                )
            WHERE id = ?3
              AND NOT EXISTS (
                  SELECT 1
                  FROM runs active_runs
                  WHERE active_runs.task_id = ?1
                    AND active_runs.role = 'supervisor'
                    AND active_runs.status IN ('running', 'checking', 'reviewing')
                    AND active_runs.id <> ?3
              )
            "#,
            params![context.task_id, timestamp(), run_id],
        )?;
        if attached == 0 {
            transaction.commit()?;
            return Ok(None);
        }
        insert_event_in_transaction(
            &transaction,
            Some(&run_id),
            "run_consultation_attached",
            &serde_json::json!({
                "agent": agent_id,
                "task_id": context.task_id,
            }),
        )?;
        transaction.commit()?;
        Ok(Some(context))
    })
    .await?
}

pub async fn attach_running_run_to_consultation(
    task_id: &str,
    agent_id: &str,
) -> Result<Option<RuntimeTaskContext>> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let agent_id = agent_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<RuntimeTaskContext>> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(run_id) = latest_active_run_for_agent(&transaction, &agent_id)? else {
            transaction.commit()?;
            return Ok(None);
        };

        if let Some(context) = consultation_context_for_run(&transaction, &run_id)? {
            transaction.commit()?;
            return Ok((context.task_id == task_id).then_some(context));
        }

        let candidate = transaction
            .query_row(
                r#"
                SELECT id, path, spec_path, milestone_id, status, paused_status,
                       check_retries, review_cycles, failure_reason,
                       baseline_snapshot_id, overlay_revision_id, repository_view_snapshot_id,
                       repository_view_tree_algorithm, repository_view_tree_digest,
                       repository_view_lifecycle, repository_view_status
                FROM tasks
                WHERE id = ?1
                  AND status = ?2
                  AND NOT EXISTS (
                      SELECT 1
                      FROM runs
                      WHERE runs.task_id = tasks.id
                        AND runs.role = 'supervisor'
                        AND runs.status IN ('running', 'checking', 'reviewing')
                  )
                LIMIT 1
                "#,
                params![task_id, TaskStatus::Consultation.as_str()],
                |row| {
                    Ok(RuntimeTaskContext {
                        task_id: row.get(0)?,
                        task_path: row.get(1)?,
                        spec_path: row.get(2)?,
                        milestone_id: row.get(3)?,
                        run_dir: String::new(),
                        status: row.get(4)?,
                        paused_status: row.get(5)?,
                        check_retries: row.get::<_, i64>(6)? as u32,
                        review_cycles: row.get::<_, i64>(7)? as u32,
                        failure_reason: row.get(8)?,
                        run_id: Some(run_id.clone()),
                        run_role: Some("supervisor".to_string()),
                        workspace_path: None,
                        repository_workspace_path: latest_executor_workspace_for_task(
                            &transaction,
                            &row.get::<_, String>(0)?,
                        )?,
                        repository_view: graph::repository_view_reference_from_row(row, 9)?,
                    })
                },
            )
            .optional()?;
        let Some(mut context) = candidate else {
            transaction.commit()?;
            return Ok(None);
        };

        context.run_dir = run_dir_for_task(&context.task_id);
        let attached = transaction.execute(
            r#"
            UPDATE runs
            SET task_id = ?1,
                updated_at = ?2,
                baseline_snapshot_id = (SELECT baseline_snapshot_id FROM tasks WHERE id = ?1),
                overlay_revision_id = (SELECT overlay_revision_id FROM tasks WHERE id = ?1),
                repository_view_snapshot_id = (
                    SELECT repository_view_snapshot_id FROM tasks WHERE id = ?1
                ),
                repository_view_tree_algorithm = (
                    SELECT repository_view_tree_algorithm FROM tasks WHERE id = ?1
                ),
                repository_view_tree_digest = (
                    SELECT repository_view_tree_digest FROM tasks WHERE id = ?1
                ),
                repository_view_lifecycle = (
                    SELECT repository_view_lifecycle FROM tasks WHERE id = ?1
                ),
                repository_view_status = (
                    SELECT repository_view_status FROM tasks WHERE id = ?1
                )
            WHERE id = ?3
              AND NOT EXISTS (
                  SELECT 1
                  FROM runs active_runs
                  WHERE active_runs.task_id = ?1
                    AND active_runs.role = 'supervisor'
                    AND active_runs.status IN ('running', 'checking', 'reviewing')
                    AND active_runs.id <> ?3
              )
            "#,
            params![context.task_id, timestamp(), run_id],
        )?;
        if attached == 0 {
            transaction.commit()?;
            return Ok(None);
        }
        insert_event_in_transaction(
            &transaction,
            Some(&run_id),
            "run_consultation_attached",
            &serde_json::json!({
                "agent": agent_id,
                "task_id": context.task_id,
            }),
        )?;
        transaction.commit()?;
        Ok(Some(context))
    })
    .await?
}

pub async fn record_run_finished(run_id: &str, exit_code: i32) -> Result<()> {
    let database_path = current_database_path().await?;
    let run_id = run_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        let status = if exit_code == 0 {
            "completed"
        } else {
            "failed"
        };
        connection.execute(
            "UPDATE runs SET status = ?1, updated_at = ?2, pid = NULL WHERE id = ?3",
            params![status, timestamp(), run_id],
        )?;
        insert_event(
            &connection,
            Some(&run_id),
            "run_finished",
            &serde_json::json!({
                "exit_code": exit_code,
                "status": status,
            }),
        )?;
        Ok(())
    })
    .await?
}

pub async fn record_run_finished_best_effort(run_id: &str, exit_code: i32) {
    if let Err(err) = record_run_finished(run_id, exit_code).await {
        warn!(error = ?err, run_id, exit_code, "failed to mirror run finish into ferrus.db");
    }
}

pub async fn recover_interrupted_runs() -> Result<usize> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<usize> {
        let connection = open_runtime_database(&database_path)?;
        let mut statement = connection.prepare(
            "SELECT id, pid FROM runs WHERE status IN ('running', 'checking', 'reviewing')",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
        })?;

        let mut interrupted = Vec::new();
        for row in rows {
            let (run_id, pid) = row?;
            if pid.is_none_or(|pid| !process_is_alive(pid as u32)) {
                interrupted.push(run_id);
            }
        }

        for run_id in &interrupted {
            connection.execute(
                "UPDATE runs SET status = 'interrupted', updated_at = ?1, pid = NULL WHERE id = ?2",
                params![timestamp(), run_id],
            )?;
            insert_event(
                &connection,
                Some(run_id),
                "run_interrupted",
                &serde_json::json!({}),
            )?;
        }

        Ok(interrupted.len())
    })
    .await?
}

pub async fn recover_expired_task_leases() -> Result<usize> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<usize> {
        let connection = open_runtime_database(&database_path)?;
        let now = Utc::now();
        let live_run_task_ids = live_active_run_task_ids_from_database(&connection)?;
        let mut statement = connection.prepare(
            "SELECT id, claimed_by, lease_until FROM tasks WHERE claimed_by IS NOT NULL",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;

        let mut expired = Vec::new();
        for row in rows {
            let (task_id, claimed_by, lease_until) = row?;
            let parsed_lease = lease_until
                .as_deref()
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc));
            if parsed_lease.is_none_or(|lease_until| now >= lease_until)
                && !live_run_task_ids.contains(&task_id)
            {
                expired.push((task_id, claimed_by, lease_until));
            }
        }

        for (task_id, claimed_by, lease_until) in &expired {
            clear_task_lease(&connection, task_id)?;
            insert_event(
                &connection,
                None,
                "task_lease_expired",
                &serde_json::json!({
                    "task_id": task_id,
                    "claimed_by": claimed_by,
                    "lease_until": lease_until,
                }),
            )?;
        }

        Ok(expired.len())
    })
    .await?
}

pub async fn recover_runtime_state() -> Result<RuntimeRecovery> {
    let interrupted_runs = recover_interrupted_runs().await?;
    let expired_task_leases = recover_expired_task_leases().await?;
    registry::recover_recorded_human_answers().await?;
    Ok(RuntimeRecovery {
        interrupted_runs,
        expired_task_leases,
    })
}

pub async fn preview_runtime_recovery() -> Result<RuntimeRecovery> {
    let database_path = current_database_path().await?;
    preview_runtime_recovery_from(&database_path).await
}

pub async fn preview_orphaned_worktrees() -> Result<usize> {
    Ok(orphaned_worktrees().await?.len())
}

pub async fn pin_executor_baseline_tree(
    project_root: &Path,
    task_id: &str,
    baseline_tree: &str,
) -> Result<()> {
    let baseline_ref = executor_baseline_ref(task_id);
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["update-ref", &baseline_ref, baseline_tree])
        .output()
        .await
        .with_context(|| format!("Failed to create executor baseline ref {baseline_ref}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "Failed to create executor baseline ref {baseline_ref}: {}",
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        );
    }
    Ok(())
}

pub async fn remove_executor_baseline(
    project_root: &Path,
    data_dir: &Path,
    task_id: &str,
) -> Result<()> {
    let baseline_ref = executor_baseline_ref(task_id);
    let exists = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["show-ref", "--verify", "--quiet", &baseline_ref])
        .output()
        .await;
    if matches!(exists, Ok(output) if output.status.success()) {
        let output = Command::new("git")
            .arg("-C")
            .arg(project_root)
            .args(["update-ref", "-d", &baseline_ref])
            .output()
            .await
            .with_context(|| format!("Failed to remove executor baseline ref {baseline_ref}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            anyhow::bail!(
                "Failed to remove executor baseline ref {baseline_ref}: {}",
                if stderr.is_empty() {
                    output.status.to_string()
                } else {
                    stderr
                }
            );
        }
    }

    let metadata_path = data_dir
        .join("worktrees")
        .join(BASELINE_WORKTREE_METADATA_DIR)
        .join(format!("{task_id}.txt"));
    match tokio::fs::remove_file(&metadata_path).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "Failed to remove executor baseline metadata {}",
                    metadata_path.display()
                )
            });
        }
    }
    Ok(())
}

fn executor_baseline_ref(task_id: &str) -> String {
    format!("{BASELINE_REF_PREFIX}/{task_id}")
}

pub async fn recover_orphaned_worktrees() -> Result<usize> {
    let registration = touch_current_project().await?;
    let project_root = PathBuf::from(&registration.metadata.workspace_dir);
    let worktrees = orphaned_worktrees_for(&registration).await?;
    let mut removed = 0usize;
    for worktree in worktrees {
        let task_id = worktree
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("Invalid managed worktree path {}", worktree.display()))?
            .to_string();
        let output = Command::new("git")
            .arg("-C")
            .arg(&project_root)
            .args(["worktree", "remove", "--force"])
            .arg(&worktree)
            .output()
            .await
            .with_context(|| {
                format!(
                    "Failed to run git worktree remove for {}",
                    worktree.display()
                )
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            anyhow::bail!(
                "Failed to remove orphaned worktree at {}: {}",
                worktree.display(),
                if stderr.is_empty() {
                    output.status.to_string()
                } else {
                    stderr
                }
            );
        }
        remove_executor_baseline(&project_root, &registration.data_dir, &task_id).await?;
        removed += 1;
    }
    Ok(removed)
}

async fn orphaned_worktrees() -> Result<Vec<PathBuf>> {
    let registration = touch_current_project().await?;
    orphaned_worktrees_for(&registration).await
}

pub(super) async fn orphaned_worktrees_for(
    registration: &ProjectRegistration,
) -> Result<Vec<PathBuf>> {
    let worktrees_dir = registration.data_dir.join("worktrees");
    if !tokio::fs::try_exists(&worktrees_dir).await? {
        return Ok(Vec::new());
    }

    let protected_task_ids = protected_worktree_task_ids(&registration.database_path).await?;
    let protected_paths = protected_worktree_paths(&registration.database_path).await?;
    let mut entries = tokio::fs::read_dir(&worktrees_dir)
        .await
        .with_context(|| format!("Failed to read {}", worktrees_dir.display()))?;
    let mut orphaned = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !entry.file_type().await?.is_dir() {
            continue;
        }
        let file_name = entry.file_name();
        if file_name.to_string_lossy() == BASELINE_WORKTREE_METADATA_DIR {
            continue;
        }
        let task_id = file_name.to_string_lossy().to_string();
        let canonical_path = tokio::fs::canonicalize(&path)
            .await
            .unwrap_or_else(|_| path.clone());
        if protected_task_ids.contains(&task_id) || protected_paths.contains(&canonical_path) {
            continue;
        }
        orphaned.push(path);
    }
    orphaned.sort();
    Ok(orphaned)
}

async fn protected_worktree_task_ids(
    database_path: &Path,
) -> Result<std::collections::HashSet<String>> {
    let database_path = database_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<std::collections::HashSet<String>> {
        let connection = open_runtime_database(&database_path)?;
        let mut statement =
            connection.prepare("SELECT id FROM tasks WHERE status NOT IN (?1, ?2, ?3)")?;
        let rows = statement.query_map(
            params![
                TaskStatus::Reset.as_str(),
                TaskStatus::Complete.as_str(),
                TaskStatus::Failed.as_str()
            ],
            |row| row.get::<_, String>(0),
        )?;
        let mut task_ids = std::collections::HashSet::new();
        for row in rows {
            task_ids.insert(row?);
        }
        Ok(task_ids)
    })
    .await?
}

async fn protected_worktree_paths(
    database_path: &Path,
) -> Result<std::collections::HashSet<PathBuf>> {
    let database_path = database_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<std::collections::HashSet<PathBuf>> {
        let connection = open_runtime_database(&database_path)?;
        let mut statement = connection.prepare(
            "SELECT workspace_path, pid FROM runs WHERE status IN ('running', 'checking', 'reviewing')",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
        })?;
        let mut paths = std::collections::HashSet::new();
        for row in rows {
            let (workspace_path, pid) = row?;
            if pid.is_none_or(|pid| !process_is_alive(pid as u32)) {
                continue;
            }
            let path = PathBuf::from(workspace_path);
            paths.insert(std::fs::canonicalize(&path).unwrap_or(path));
        }
        Ok(paths)
    })
    .await?
}

pub(super) async fn preview_runtime_recovery_from(database_path: &Path) -> Result<RuntimeRecovery> {
    Ok(RuntimeRecovery {
        interrupted_runs: preview_interrupted_runs(database_path).await?,
        expired_task_leases: preview_expired_task_leases(database_path).await?,
    })
}

async fn preview_interrupted_runs(database_path: &Path) -> Result<usize> {
    let database_path = database_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<usize> {
        let connection =
            Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("Failed to open {}", database_path.display()))?;
        let mut statement = connection
            .prepare("SELECT pid FROM runs WHERE status IN ('running', 'checking', 'reviewing')")?;
        let rows = statement.query_map([], |row| row.get::<_, Option<i64>>(0))?;

        let mut interrupted = 0;
        for row in rows {
            if row?.is_none_or(|pid| !process_is_alive(pid as u32)) {
                interrupted += 1;
            }
        }
        Ok(interrupted)
    })
    .await?
}

async fn preview_expired_task_leases(database_path: &Path) -> Result<usize> {
    let database_path = database_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<usize> {
        let connection =
            Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("Failed to open {}", database_path.display()))?;
        let now = Utc::now();
        let live_run_task_ids = live_active_run_task_ids_from_database(&connection)?;
        let mut statement =
            connection.prepare("SELECT id, lease_until FROM tasks WHERE claimed_by IS NOT NULL")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?;

        let mut expired = 0;
        for row in rows {
            let (task_id, lease_until) = row?;
            let parsed_lease = lease_until
                .as_deref()
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc));
            if parsed_lease.is_none_or(|lease_until| now >= lease_until)
                && !live_run_task_ids.contains(&task_id)
            {
                expired += 1;
            }
        }
        Ok(expired)
    })
    .await?
}
