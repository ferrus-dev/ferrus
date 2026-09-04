//! Project registration, artifact reservation, spec archival, and runtime inspection.

use super::*;

pub async fn ensure_global_dir() -> Result<PathBuf> {
    let root = global_dir()?;
    tokio::fs::create_dir_all(root.join("projects"))
        .await
        .with_context(|| format!("Failed to create {}", root.join("projects").display()))?;
    Ok(root)
}

pub async fn register_current_project() -> Result<ProjectRegistration> {
    ensure_global_dir().await?;
    let workspace_dir = canonical_current_dir()
        .await
        .context("Failed to resolve current workspace directory")?;
    let ferrus_dir = workspace_dir.join(".ferrus");
    tokio::fs::create_dir_all(&ferrus_dir)
        .await
        .with_context(|| format!("Failed to create {}", ferrus_dir.display()))?;

    let now = timestamp();
    let existing = read_local_project_ref().await.ok();
    let project_id = if let Some(project) = existing.as_ref() {
        validate_project_id(&project.project_id)?;
        project.project_id.clone()
    } else {
        generate_project_id(&workspace_dir)
    };
    let data_dir = project_data_dir(&project_id)?;
    tokio::fs::create_dir_all(data_dir.join("logs"))
        .await
        .with_context(|| format!("Failed to create {}", data_dir.join("logs").display()))?;

    let project_toml_path = data_dir.join("project.toml");
    let previous_metadata = read_project_metadata_from(&project_toml_path).await.ok();
    let created_at = previous_metadata
        .as_ref()
        .map(|metadata| metadata.created_at.clone())
        .unwrap_or_else(|| now.clone());
    let name = workspace_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project")
        .to_string();
    let git = read_git_metadata().await;
    let metadata = ProjectMetadata {
        id: project_id.clone(),
        name: name.clone(),
        workspace_dir: path_string(&workspace_dir),
        ferrus_dir: path_string(&ferrus_dir),
        vcs: git.as_ref().map(|_| "git".to_string()),
        origin_repo: git.as_ref().and_then(|git| git.origin_repo.clone()),
        default_branch: git.as_ref().and_then(|git| git.default_branch.clone()),
        current_head: git.as_ref().and_then(|git| git.current_head.clone()),
        created_at,
        last_opened_at: now,
        version: PROJECT_VERSION,
    };
    write_toml(&project_toml_path, &metadata).await?;

    let local_ref = LocalProjectRef {
        project_id,
        name,
        data_dir: path_string(&data_dir),
    };
    write_toml(&project_path(LOCAL_PROJECT_TOML), &local_ref).await?;

    let database_path = data_dir.join("ferrus.db");
    initialize_database(&database_path).await?;

    Ok(ProjectRegistration {
        local_ref,
        metadata,
        data_dir,
        database_path,
    })
}

pub async fn migrate_current_project() -> Result<ProjectRegistration> {
    let registration = register_current_project().await?;
    tokio::fs::create_dir_all(".ferrus/tasks")
        .await
        .context("Failed to create .ferrus/tasks")?;
    tokio::fs::create_dir_all(".ferrus/runs")
        .await
        .context("Failed to create .ferrus/runs")?;
    if let Ok(state) = legacy_state::read_legacy_state(project_path(".ferrus/STATE.json")).await {
        migrate_legacy_project_selection(&state).await?;
        let state_value = state.state();
        if state_value != LegacyTaskState::Idle {
            copy_legacy_artifacts(true).await?;
            migrate_legacy_active_task(&state).await?;
        } else {
            copy_legacy_artifacts(false).await?;
        }
    } else {
        copy_legacy_artifacts(false).await?;
    }
    retire_legacy_current_task_row().await?;
    remove_legacy_state_files().await?;
    Ok(registration)
}

pub(super) async fn migrate_legacy_active_task(
    state: &legacy_state::LegacyStateData,
) -> Result<()> {
    let database_path = current_database_path().await?;
    let legacy_status = state.state();
    let status = legacy_state::task_status_for_legacy_state(&legacy_status);
    let paused_state = state
        .paused_state
        .as_ref()
        .map(legacy_state::task_status_for_legacy_state)
        .map(TaskStatus::as_str);
    let (paused_status, awaiting_human_status, awaiting_human_by) = match legacy_status {
        LegacyTaskState::Consultation => (paused_state, None, None),
        LegacyTaskState::AwaitingHuman => (None, paused_state, state.awaiting_human_by.clone()),
        _ => (None, None, None),
    };
    let spec_path = state.task_spec.clone();
    let milestone_id = state.task_milestone.clone();
    let check_retries = state.check_retries;
    let review_cycles = state.review_cycles;
    let failure_reason = state.failure_reason.clone();

    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        upsert_task(
            &transaction,
            "t-001",
            ".ferrus/tasks/t-001.md",
            status,
            spec_path.as_deref(),
            milestone_id.as_deref(),
        )?;
        transaction.execute(
            r#"
            UPDATE tasks
            SET paused_status = ?1, check_retries = ?2, review_cycles = ?3,
                failure_reason = ?4, awaiting_human_by = ?5, awaiting_human_status = ?6
            WHERE id = 't-001'
            "#,
            params![
                paused_status,
                check_retries,
                review_cycles,
                failure_reason,
                awaiting_human_by,
                awaiting_human_status,
            ],
        )?;
        insert_event_in_transaction(
            &transaction,
            None,
            "task_migrated_from_legacy_state",
            &serde_json::json!({
                "task_id": "t-001",
                "status": status.as_str(),
                "paused_status": paused_status,
                "awaiting_human_status": awaiting_human_status,
                "check_retries": check_retries,
                "review_cycles": review_cycles,
            }),
        )?;
        transaction.commit()?;
        Ok(())
    })
    .await?
}

pub(super) async fn migrate_legacy_project_selection(
    state: &legacy_state::LegacyStateData,
) -> Result<()> {
    if let Some(selected_spec) = normalized_metadata_value(state.selected_spec.as_deref()) {
        write_project_selection(&ProjectSelection {
            selected_spec: Some(selected_spec),
        })
        .await?;
    }
    Ok(())
}

pub async fn touch_current_project() -> Result<ProjectRegistration> {
    let local_ref = read_local_project_ref()
        .await
        .context(".ferrus/project.toml not found or invalid -- run `ferrus migrate`")?;
    validate_project_id(&local_ref.project_id)?;
    let data_dir = PathBuf::from(&local_ref.data_dir);
    tokio::fs::create_dir_all(data_dir.join("logs"))
        .await
        .with_context(|| format!("Failed to create {}", data_dir.join("logs").display()))?;

    let metadata_path = data_dir.join("project.toml");
    let previous_metadata = read_project_metadata_from(&metadata_path)
        .await
        .with_context(|| format!("Failed to read {}", metadata_path.display()))?;
    if previous_metadata.id != local_ref.project_id {
        anyhow::bail!(
            "local project_id {} does not match global metadata id {}",
            local_ref.project_id,
            previous_metadata.id
        );
    }

    let workspace_dir = canonical_current_dir()
        .await
        .context("Failed to resolve current workspace directory")?;
    let ferrus_dir = workspace_dir.join(".ferrus");
    let name = workspace_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project")
        .to_string();
    let git = read_git_metadata().await;
    let metadata = ProjectMetadata {
        id: local_ref.project_id.clone(),
        name,
        workspace_dir: path_string(&workspace_dir),
        ferrus_dir: path_string(&ferrus_dir),
        vcs: git.as_ref().map(|_| "git".to_string()),
        origin_repo: git.as_ref().and_then(|git| git.origin_repo.clone()),
        default_branch: git.as_ref().and_then(|git| git.default_branch.clone()),
        current_head: git.as_ref().and_then(|git| git.current_head.clone()),
        created_at: previous_metadata.created_at,
        last_opened_at: timestamp(),
        version: PROJECT_VERSION,
    };
    write_toml(&metadata_path, &metadata).await?;
    initialize_database(&data_dir.join("ferrus.db")).await?;

    Ok(ProjectRegistration {
        local_ref,
        metadata,
        database_path: data_dir.join("ferrus.db"),
        data_dir,
    })
}

pub async fn canonical_project_root() -> Result<PathBuf> {
    if let Some(project_root) = std::env::var(ENV_PROJECT_ROOT)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        return tokio::fs::canonicalize(&project_root)
            .await
            .with_context(|| {
                format!(
                    "Failed to resolve canonical project root from {ENV_PROJECT_ROOT}: {}",
                    project_root.display()
                )
            });
    }

    let local_ref = read_local_project_ref()
        .await
        .context("Failed to resolve canonical project root from .ferrus/project.toml")?;
    let metadata_path = Path::new(&local_ref.data_dir).join("project.toml");
    let metadata = read_project_metadata_from(&metadata_path)
        .await
        .with_context(|| {
            format!(
                "Failed to resolve canonical project root from {}",
                metadata_path.display()
            )
        })?;
    let project_root = PathBuf::from(metadata.workspace_dir);
    tokio::fs::canonicalize(&project_root)
        .await
        .with_context(|| {
            format!(
                "Failed to canonicalize project workspace {}",
                project_root.display()
            )
        })
}

/// Resolves the registered machine-local project directory without touching
/// runtime state or creating any files. Derived sidecars use this registry
/// boundary instead of inferring identity from the process working directory.
pub async fn current_project_data_dir() -> Result<PathBuf> {
    let local_ref = read_local_project_ref()
        .await
        .context(".ferrus/project.toml not found or invalid -- run `ferrus migrate`")?;
    validate_project_id(&local_ref.project_id)?;
    let data_dir = PathBuf::from(&local_ref.data_dir);
    let metadata_path = data_dir.join("project.toml");
    let metadata = read_project_metadata_from(&metadata_path)
        .await
        .with_context(|| format!("Failed to read {}", metadata_path.display()))?;
    if metadata.id != local_ref.project_id {
        anyhow::bail!(
            "local project_id {} does not match global metadata id {}",
            local_ref.project_id,
            metadata.id
        );
    }
    Ok(data_dir)
}

/// Returns the opaque machine-local project id used as the local repository
/// authority without mutating runtime state.
pub async fn current_project_id() -> Result<String> {
    let local_ref = read_local_project_ref()
        .await
        .context(".ferrus/project.toml not found or invalid -- run `ferrus migrate`")?;
    validate_project_id(&local_ref.project_id)?;
    let data_dir = current_project_data_dir().await?;
    let metadata = read_project_metadata_from(&data_dir.join("project.toml")).await?;
    if metadata.id != local_ref.project_id {
        anyhow::bail!("local and registered project identities do not match");
    }
    Ok(local_ref.project_id)
}

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

async fn reserve_task_artifact() -> Result<TaskArtifact> {
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

async fn finalize_task_artifact_reservation(
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

async fn discard_task_artifact_reservation(artifact: &TaskArtifact) -> Result<()> {
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

async fn remove_path_if_exists(path: &Path, directory: bool) -> Result<()> {
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

pub async fn doctor_current_project() -> Result<DoctorReport> {
    let local_ref = read_local_project_ref()
        .await
        .context(".ferrus/project.toml not found or invalid -- run `ferrus migrate`")?;
    let data_dir = PathBuf::from(&local_ref.data_dir);
    let metadata_path = data_dir.join("project.toml");
    let metadata = read_project_metadata_from(&metadata_path)
        .await
        .with_context(|| format!("Failed to read {}", metadata_path.display()))?;
    let database_path = data_dir.join("ferrus.db");
    let current_dir = canonical_current_dir().await?;
    let current_ferrus_dir = current_dir.join(".ferrus");
    let expected_data_dir = project_data_dir(&local_ref.project_id)?;

    let mut checks = Vec::new();
    checks.push(DoctorCheck {
        ok: local_ref.project_id == metadata.id,
        message: format!(
            "local project_id matches global metadata id ({})",
            local_ref.project_id
        ),
    });
    checks.push(DoctorCheck {
        ok: equivalent_paths(&data_dir, &expected_data_dir).await,
        message: format!("data_dir points at {}", expected_data_dir.display()),
    });
    checks.push(DoctorCheck {
        ok: equivalent_paths(Path::new(&metadata.workspace_dir), &current_dir).await,
        message: format!("workspace_dir points at {}", current_dir.display()),
    });
    checks.push(DoctorCheck {
        ok: equivalent_paths(Path::new(&metadata.ferrus_dir), &current_ferrus_dir).await,
        message: format!("ferrus_dir points at {}", current_ferrus_dir.display()),
    });
    checks.push(DoctorCheck {
        ok: tokio::fs::metadata(&database_path).await.is_ok(),
        message: format!("database exists at {}", database_path.display()),
    });
    checks.push(DoctorCheck {
        ok: validate_database_schema(&database_path)
            .await
            .unwrap_or(false),
        message: "database has tasks, runs, events, and task lease columns".to_string(),
    });
    add_recovery_doctor_checks(&mut checks, &database_path).await;
    add_runtime_doctor_checks(&mut checks, &database_path).await;

    Ok(DoctorReport {
        registration: ProjectRegistration {
            local_ref,
            metadata,
            data_dir,
            database_path,
        },
        checks,
    })
}

pub(super) async fn add_recovery_doctor_checks(
    checks: &mut Vec<DoctorCheck>,
    database_path: &Path,
) {
    let recovery = match preview_runtime_recovery_from(database_path).await {
        Ok(recovery) => recovery,
        Err(err) => {
            checks.push(DoctorCheck {
                ok: false,
                message: format!("runtime recovery preview can read ferrus.db ({err})"),
            });
            return;
        }
    };

    checks.push(DoctorCheck {
        ok: recovery.interrupted_runs == 0,
        message: format!(
            "no interrupted run recovery pending ({} found; run `ferrus recover`)",
            recovery.interrupted_runs
        ),
    });
    checks.push(DoctorCheck {
        ok: recovery.expired_task_leases == 0,
        message: format!(
            "no expired task lease recovery pending ({} found; run `ferrus recover`)",
            recovery.expired_task_leases
        ),
    });
}

pub(super) async fn add_runtime_doctor_checks(checks: &mut Vec<DoctorCheck>, database_path: &Path) {
    let data_dir = database_path.parent().unwrap_or_else(|| Path::new(""));
    let task_rows = match read_task_records_from_database(database_path).await {
        Ok(rows) => rows,
        Err(err) => {
            checks.push(DoctorCheck {
                ok: false,
                message: format!("task rows can be read from ferrus.db ({err})"),
            });
            return;
        }
    };
    checks.push(DoctorCheck {
        ok: true,
        message: "task rows can be read from ferrus.db".to_string(),
    });
    for task in task_rows
        .iter()
        .filter(|task| runtime_artifacts_expected_for_status(&task.status))
    {
        checks.push(DoctorCheck {
            ok: tokio::fs::metadata(&task.path).await.is_ok(),
            message: format!("task artifact exists for {} at {}", task.id, task.path),
        });
        let run_dir = doctor_run_artifact_dir(task, data_dir);
        let run_dir_exists = tokio::fs::metadata(&run_dir)
            .await
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false);
        checks.push(DoctorCheck {
            ok: run_dir_exists,
            message: format!(
                "run artifact directory exists for {} at {}",
                task.id,
                path_string(&run_dir)
            ),
        });
    }
}

fn runtime_artifacts_expected_for_status(status: &str) -> bool {
    !matches!(
        status.parse::<TaskStatus>().ok(),
        Some(TaskStatus::Reset | TaskStatus::Unknown)
    )
}

fn doctor_run_artifact_dir(task: &TaskRecord, data_dir: &Path) -> PathBuf {
    archived_run_dir_for_task_path(&task.path, &task.id, data_dir)
        .unwrap_or_else(|| PathBuf::from(run_dir_for_task(&task.id)))
}

fn task_has_checkout_archive_artifacts(task: &TaskRecord) -> bool {
    let task_path = Path::new(&task.path);
    let task_file_in_checkout = checkout_task_artifact_path(task_path) && task_path.exists();
    let run_dir_in_checkout =
        checkout_task_artifact_path(task_path) && Path::new(&run_dir_for_task(&task.id)).exists();
    task_file_in_checkout || run_dir_in_checkout
}

pub(super) fn checkout_task_artifact_path(path: &Path) -> bool {
    path.starts_with(".ferrus/tasks")
}

pub(super) fn archived_run_dir_for_task_path(
    task_path: &str,
    task_id: &str,
    data_dir: &Path,
) -> Option<PathBuf> {
    let task_path = Path::new(task_path);
    let archive_specs_dir = data_dir.join("archive").join("specs");
    if !task_path.is_absolute() || !task_path.starts_with(&archive_specs_dir) {
        return None;
    }
    let expected_file_name = format!("{task_id}.md");
    if task_path.file_name().and_then(|name| name.to_str()) != Some(expected_file_name.as_str()) {
        return None;
    }
    let tasks_dir = task_path.parent()?;
    if tasks_dir.file_name().and_then(|name| name.to_str()) != Some("tasks") {
        return None;
    }
    let archive_dir = tasks_dir.parent()?;
    Some(archive_dir.join("runs").join(task_id))
}

pub async fn list_registered_projects() -> Result<Vec<ProjectListEntry>> {
    let projects_dir = global_dir()?.join("projects");
    list_registered_projects_from(&projects_dir).await
}

pub(super) async fn list_registered_projects_from(
    projects_dir: &Path,
) -> Result<Vec<ProjectListEntry>> {
    if tokio::fs::metadata(projects_dir).await.is_err() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    let mut read_dir = tokio::fs::read_dir(projects_dir)
        .await
        .with_context(|| format!("Failed to read {}", projects_dir.display()))?;
    while let Some(entry) = read_dir
        .next_entry()
        .await
        .with_context(|| format!("Failed to iterate {}", projects_dir.display()))?
    {
        let file_type = entry
            .file_type()
            .await
            .with_context(|| format!("Failed to inspect {}", entry.path().display()))?;
        if !file_type.is_dir() {
            continue;
        }

        let data_dir = entry.path();
        let fallback_id = entry.file_name().to_string_lossy().into_owned();
        let database_exists = tokio::fs::metadata(data_dir.join("ferrus.db"))
            .await
            .is_ok();
        match read_project_metadata_from(&data_dir.join("project.toml")).await {
            Ok(metadata) => entries.push(ProjectListEntry {
                id: metadata.id,
                name: Some(metadata.name),
                workspace_dir: Some(metadata.workspace_dir),
                data_dir,
                database_exists,
                last_opened_at: Some(metadata.last_opened_at),
                error: None,
            }),
            Err(err) => entries.push(ProjectListEntry {
                id: fallback_id,
                name: None,
                workspace_dir: None,
                data_dir,
                database_exists,
                last_opened_at: None,
                error: Some(err.to_string()),
            }),
        }
    }

    entries.sort_by(|left, right| {
        right
            .last_opened_at
            .cmp(&left.last_opened_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(entries)
}

pub async fn list_tasks() -> Result<Vec<TaskRecord>> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<Vec<TaskRecord>> {
        let connection = open_runtime_database(&database_path)?;
        let mut statement = connection.prepare(
            r#"
            SELECT id, path, spec_path, milestone_id, status, paused_status, claimed_by,
                   lease_until, last_heartbeat, check_retries, review_cycles, failure_reason
            FROM tasks
            ORDER BY
                CASE WHEN id = 'current' THEN 0 ELSE 1 END,
                id
            "#,
        )?;
        let rows = statement.query_map([], task_record_from_row)?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row?);
        }
        Ok(tasks)
    })
    .await?
}

pub async fn list_tasks_for_spec(spec_path: &str) -> Result<Vec<TaskRecord>> {
    let database_path = current_database_path().await?;
    let spec_path = spec_path.to_string();
    tokio::task::spawn_blocking(move || -> Result<Vec<TaskRecord>> {
        let connection = open_runtime_database(&database_path)?;
        let mut statement = connection.prepare(
            r#"
            SELECT id, path, spec_path, milestone_id, status, paused_status, claimed_by,
                   lease_until, last_heartbeat, check_retries, review_cycles, failure_reason
            FROM tasks
            WHERE spec_path = ?1
            ORDER BY id
            "#,
        )?;
        let rows = statement.query_map([spec_path], task_record_from_row)?;
        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row?);
        }
        Ok(tasks)
    })
    .await?
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

pub async fn list_human_questions() -> Result<Vec<HumanQuestion>> {
    let database_path = current_database_path().await?;
    let tasks = tokio::task::spawn_blocking(move || -> Result<Vec<TaskRecord>> {
        let connection = open_runtime_database(&database_path)?;
        let mut statement = connection.prepare(
            r#"
            SELECT id, path, spec_path, milestone_id, status, paused_status, claimed_by,
                   lease_until, last_heartbeat, check_retries, review_cycles, failure_reason
            FROM tasks
            WHERE status = ?1 AND human_answer_recorded = 0
            ORDER BY
                CASE WHEN human_question_order IS NULL THEN 0 ELSE 1 END,
                human_question_order,
                id
            "#,
        )?;
        let rows =
            statement.query_map([TaskStatus::AwaitingHuman.as_str()], task_record_from_row)?;
        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row?);
        }
        Ok(tasks)
    })
    .await??;
    let mut questions = Vec::new();
    for task in tasks {
        let run_dir = run_dir_for_task(&task.id);
        let question = crate::state::store::read_question_for_run_dir(&run_dir)
            .await
            .unwrap_or_default()
            .trim()
            .to_string();
        questions.push(HumanQuestion {
            task_id: task.id,
            task_path: task.path,
            run_dir,
            question,
        });
    }
    Ok(questions)
}

pub async fn list_answered_human_waiters() -> Result<Vec<AnsweredHumanWaiter>> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<Vec<AnsweredHumanWaiter>> {
        let connection = open_runtime_database(&database_path)?;
        let mut statement = connection.prepare(
            r#"
            SELECT id, awaiting_human_by
            FROM tasks
            WHERE status = ?1
              AND human_answer_recorded = 1
              AND awaiting_human_by IS NOT NULL
            ORDER BY
                CASE WHEN human_question_order IS NULL THEN 0 ELSE 1 END,
                human_question_order,
                id
            "#,
        )?;
        let rows = statement.query_map([TaskStatus::AwaitingHuman.as_str()], |row| {
            Ok(AnsweredHumanWaiter {
                task_id: row.get(0)?,
                awaiting_human_by: row.get(1)?,
            })
        })?;
        let mut waiters = Vec::new();
        for row in rows {
            waiters.push(row?);
        }
        Ok(waiters)
    })
    .await?
}

pub(super) async fn recover_recorded_human_answers() -> Result<usize> {
    let questions = list_human_questions().await?;
    let mut recovered = 0usize;
    for question in questions {
        let answer = crate::state::store::read_answer_for_run_dir(&question.run_dir)
            .await
            .unwrap_or_default();
        if answer.trim().is_empty() {
            continue;
        }
        record_task_human_answer(&question.task_id).await?;
        recovered += 1;
    }
    Ok(recovered)
}

#[allow(dead_code)]
pub async fn find_non_terminal_task_by_origin(
    spec_path: &str,
    milestone_id: &str,
) -> Result<Option<TaskRecord>> {
    let database_path = current_database_path().await?;
    let spec_path = spec_path.to_string();
    let milestone_id = milestone_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<TaskRecord>> {
        let connection = open_runtime_database(&database_path)?;
        let task = connection
            .query_row(
                r#"
                SELECT id, path, spec_path, milestone_id, status, paused_status, claimed_by,
                       lease_until, last_heartbeat, check_retries, review_cycles, failure_reason
                FROM tasks
                WHERE spec_path = ?1
                  AND milestone_id = ?2
                  AND status NOT IN (?3, ?4, ?5)
                ORDER BY id
                LIMIT 1
                "#,
                params![
                    spec_path,
                    milestone_id,
                    TaskStatus::Reset.as_str(),
                    TaskStatus::Complete.as_str(),
                    TaskStatus::Failed.as_str()
                ],
                task_record_from_row,
            )
            .optional()?;
        Ok(task)
    })
    .await?
}

pub async fn list_runs(limit: usize) -> Result<Vec<RunRecord>> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<Vec<RunRecord>> {
        let connection = open_runtime_database(&database_path)?;
        let mut statement = connection.prepare(
            r#"
            SELECT id, task_id, role, agent, status, started_at, updated_at, pid, workspace_path
            FROM runs
            ORDER BY updated_at DESC, started_at DESC, id DESC
            LIMIT ?1
            "#,
        )?;
        let rows = statement.query_map([limit as i64], |row| {
            Ok(RunRecord {
                id: row.get(0)?,
                task_id: row.get(1)?,
                role: row.get(2)?,
                agent: row.get(3)?,
                status: row.get(4)?,
                started_at: row.get(5)?,
                updated_at: row.get(6)?,
                pid: row.get::<_, Option<i64>>(7)?.map(|pid| pid as u32),
                workspace_path: row.get(8)?,
            })
        })?;

        let mut runs = Vec::new();
        for row in rows {
            runs.push(row?);
        }
        Ok(runs)
    })
    .await?
}
