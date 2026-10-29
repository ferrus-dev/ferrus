//! Interrupted run, lease, and worktree recovery.

use super::*;

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
        .stdin(std::process::Stdio::null())
        .arg("-C")
        .arg(project_root)
        .args(["show-ref", "--verify", "--quiet", &baseline_ref])
        .output()
        .await;
    if matches!(exists, Ok(output) if output.status.success()) {
        let output = Command::new("git")
            .stdin(std::process::Stdio::null())
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

pub(super) fn executor_baseline_ref(task_id: &str) -> String {
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

pub(super) async fn orphaned_worktrees() -> Result<Vec<PathBuf>> {
    let registration = touch_current_project().await?;
    orphaned_worktrees_for(&registration).await
}

pub(in super::super) async fn orphaned_worktrees_for(
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

pub(super) async fn protected_worktree_task_ids(
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

pub(super) async fn protected_worktree_paths(
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

pub(in super::super) async fn preview_runtime_recovery_from(
    database_path: &Path,
) -> Result<RuntimeRecovery> {
    Ok(RuntimeRecovery {
        interrupted_runs: preview_interrupted_runs(database_path).await?,
        expired_task_leases: preview_expired_task_leases(database_path).await?,
    })
}

pub(super) async fn preview_interrupted_runs(database_path: &Path) -> Result<usize> {
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

pub(super) async fn preview_expired_task_leases(database_path: &Path) -> Result<usize> {
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
