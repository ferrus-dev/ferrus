//! Task artifact reservations and completed specification archives.

use super::*;

pub async fn create_pending_task_artifact(
    description: &str,
    spec_path: Option<&str>,
    milestone_id: Option<&str>,
) -> Result<TaskArtifact> {
    let artifact = reserve_task_artifact().await?;
    let result = async {
        tokio::fs::write(&artifact.path, description)
            .await
            .with_context(|| format!("Failed to write {}", artifact.path))?;
        tokio::fs::create_dir_all(&artifact.run_dir)
            .await
            .with_context(|| format!("Failed to create {}", artifact.run_dir))?;
        finalize_task_artifact_reservation(&artifact, spec_path, milestone_id).await
    }
    .await;

    if let Err(err) = result {
        if let Err(cleanup_err) = discard_task_artifact_reservation(&artifact).await {
            return Err(anyhow::anyhow!(
                "{err}; also failed to discard task reservation {}: {cleanup_err}",
                artifact.id
            ));
        }

        return Err(err);
    }

    Ok(artifact)
}

pub(super) async fn reserve_task_artifact() -> Result<TaskArtifact> {
    let tasks_dir = Path::new(".ferrus/tasks");
    let runs_dir = Path::new(".ferrus/runs");
    tokio::fs::create_dir_all(tasks_dir)
        .await
        .context("Failed to create .ferrus/tasks")?;
    tokio::fs::create_dir_all(runs_dir)
        .await
        .context("Failed to create .ferrus/runs")?;

    let max_file_number = max_task_number_from_files(tasks_dir).await?;
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<TaskArtifact> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let max_number = max_file_number.max(max_task_number_in_database(&transaction)?);
        let mut number = max_number
            .checked_add(1)
            .context("Cannot allocate another task id: numeric range exhausted")?;
        loop {
            let id = format!("t-{number:03}");
            let path = format!(".ferrus/tasks/{id}.md");
            let reserved = transaction.execute(
                "INSERT OR IGNORE INTO tasks (id, path, status) VALUES (?1, ?2, ?3)",
                params![id, path, TaskStatus::Unknown.as_str()],
            )?;
            if reserved == 1 {
                transaction.commit()?;
                return Ok(TaskArtifact {
                    run_dir: format!(".ferrus/runs/{id}"),
                    id,
                    path,
                });
            }

            number = number
                .checked_add(1)
                .context("Cannot allocate another task id: numeric range exhausted")?;
        }
    })
    .await?
}

pub(super) async fn finalize_task_artifact_reservation(
    artifact: &TaskArtifact,
    spec_path: Option<&str>,
    milestone_id: Option<&str>,
) -> Result<()> {
    let database_path = current_database_path().await?;
    let artifact = artifact.clone();
    let spec_path = spec_path.map(str::to_string);
    let milestone_id = milestone_id.map(str::to_string);
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let (Some(spec_path), Some(milestone_id)) =
            (spec_path.as_deref(), milestone_id.as_deref())
        {
            let duplicate = transaction.query_row(
                r#"
                SELECT id
                FROM tasks
                WHERE spec_path = ?1 AND milestone_id = ?2 AND id <> ?3
                  AND status NOT IN (?4, ?5, ?6)
                ORDER BY id
                LIMIT 1
                "#,
                params![
                    spec_path,
                    milestone_id,
                    artifact.id,
                    TaskStatus::Reset.as_str(),
                    TaskStatus::Complete.as_str(),
                    TaskStatus::Failed.as_str(),
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
            if let Some(duplicate) = duplicate {
                anyhow::bail!(
                    "Cannot enqueue task: milestone {milestone_id} from {spec_path} already has task {duplicate}."
                );
            }
        }

        let finalized = transaction.execute(
            r#"
            UPDATE tasks
            SET status = ?1, spec_path = ?2, milestone_id = ?3
            WHERE id = ?4 AND status = ?5
            "#,
            params![
                TaskStatus::Pending.as_str(),
                spec_path,
                milestone_id,
                artifact.id,
                TaskStatus::Unknown.as_str(),
            ],
        )?;
        if finalized != 1 {
            anyhow::bail!("Task reservation {} is no longer available", artifact.id);
        }

        insert_event_in_transaction(
            &transaction,
            None,
            "task_status_changed",
            &serde_json::json!({
                "task_id": artifact.id,
                "status": TaskStatus::Pending.as_str(),
            }),
        )?;
        transaction.commit()?;
        Ok(())
    })
    .await?
}

pub(super) async fn discard_task_artifact_reservation(artifact: &TaskArtifact) -> Result<()> {
    remove_path_if_exists(Path::new(&artifact.path), false).await?;
    remove_path_if_exists(Path::new(&artifact.run_dir), true).await?;
    let database_path = current_database_path().await?;
    let task_id = artifact.id.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        connection.execute(
            "DELETE FROM tasks WHERE id = ?1 AND status = ?2",
            params![task_id, TaskStatus::Unknown.as_str()],
        )?;
        Ok(())
    })
    .await?
}

pub(super) async fn remove_path_if_exists(path: &Path, directory: bool) -> Result<()> {
    let result = if directory {
        tokio::fs::remove_dir_all(path).await
    } else {
        tokio::fs::remove_file(path).await
    };
    match result {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("Failed to remove {}", path.display())),
    }
}

pub async fn archive_completed_spec(spec_path: &str, outcome: &str) -> Result<SpecArchiveResult> {
    let spec_path = normalized_metadata_value(Some(spec_path))
        .context("Cannot archive spec: spec_path is empty.")?;
    let outcome = outcome.trim().to_string();
    if outcome.is_empty() {
        anyhow::bail!("Cannot archive spec: outcome content is empty.");
    }

    if !Path::new(&spec_path).exists() {
        anyhow::bail!("Cannot archive spec: {spec_path} does not exist.");
    }

    let spec = crate::specs::load_spec(&spec_path).await?;
    let incomplete = spec
        .milestones
        .iter()
        .filter(|milestone| !milestone.completed)
        .map(|milestone| format!("{} ({})", milestone.display_title(), milestone.id))
        .collect::<Vec<_>>();
    if !incomplete.is_empty() {
        anyhow::bail!(
            "Cannot archive spec: incomplete milestones remain: {}.",
            incomplete.join(", ")
        );
    }

    let tasks = list_tasks_for_spec(&spec_path).await?;
    if tasks.is_empty() {
        anyhow::bail!("Cannot archive spec: no task rows are linked to {spec_path}.");
    }

    let active = tasks
        .iter()
        .filter(|task| {
            task.status
                .parse::<TaskStatus>()
                .map(|status| !status.is_terminal())
                .unwrap_or(true)
        })
        .map(|task| format!("{} ({})", task.id, task.status))
        .collect::<Vec<_>>();
    if !active.is_empty() {
        anyhow::bail!(
            "Cannot archive spec: non-terminal tasks remain: {}.",
            active.join(", ")
        );
    }

    if !tasks.iter().any(task_has_checkout_archive_artifacts) {
        anyhow::bail!(
            "Cannot archive spec: no checkout task or run artifacts remain for {spec_path}."
        );
    }

    crate::specs::upsert_outcome_section(&spec_path, &outcome).await?;

    let registration = touch_current_project().await?;
    let closed_at = timestamp();
    let archive_dir =
        unique_spec_archive_dir(&registration.data_dir, &spec_path, &closed_at).await?;
    let archive_dir_for_fs = archive_dir.clone();
    let spec_path_for_fs = spec_path.clone();
    let tasks_for_fs = tasks.clone();
    let manifest = SpecArchiveManifest::new(&spec_path, &closed_at, &tasks);

    let archived = tokio::task::spawn_blocking(move || -> Result<(usize, usize)> {
        stage_spec_archive_files(
            &archive_dir_for_fs,
            &spec_path_for_fs,
            &tasks_for_fs,
            &manifest,
        )
    })
    .await??;

    let archive_dir_text = path_string(&archive_dir);
    let archive_dir_for_db = archive_dir_text.clone();
    let spec_path_for_db = spec_path.clone();
    let closed_at_for_db = closed_at.clone();
    let tasks_for_db = tasks.clone();
    let outcome_for_db = outcome.clone();
    let database_path = registration.database_path.clone();
    let db_result = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for task in &tasks_for_db {
            let archived_task_path = Path::new(&archive_dir_for_db)
                .join("tasks")
                .join(format!("{}.md", task.id));
            if archived_task_path.exists() {
                transaction.execute(
                    "UPDATE tasks SET path = ?1 WHERE id = ?2",
                    params![path_string(&archived_task_path), task.id],
                )?;
            }
        }

        transaction.execute(
            r#"
            INSERT INTO spec_archives (
                spec_path, archive_dir, closed_at, task_count, run_count, outcome
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
            params![
                spec_path_for_db,
                archive_dir_for_db,
                closed_at_for_db,
                tasks_for_db.len() as i64,
                archived.1 as i64,
                outcome_for_db,
            ],
        )?;
        insert_event_in_transaction(
            &transaction,
            None,
            "spec_archived",
            &serde_json::json!({
                "spec_path": spec_path_for_db,
                "archive_dir": archive_dir_for_db,
                "task_count": tasks_for_db.len(),
                "run_count": archived.1,
            }),
        )?;
        transaction.commit()?;
        Ok(())
    })
    .await?;
    if let Err(err) = db_result {
        let _ = tokio::fs::remove_dir_all(&archive_dir).await;
        return Err(err);
    }

    let tasks_for_cleanup = tasks.clone();
    tokio::task::spawn_blocking(move || cleanup_checkout_archive_artifacts(&tasks_for_cleanup))
        .await??;
    let archive_dir_for_handoff = archive_dir_text.clone();
    let database_path = registration.database_path.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        write_last_spec_archive_path_to_database(&connection, Some(&archive_dir_for_handoff))?;
        Ok(())
    })
    .await??;

    Ok(SpecArchiveResult {
        archive_dir: archive_dir_text,
        archived_tasks: archived.0,
        archived_runs: archived.1,
    })
}
