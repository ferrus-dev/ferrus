use super::*;

pub(super) async fn initialize_database(path: &Path) -> Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut connection = Connection::open(&path)
            .with_context(|| format!("Failed to open {}", path.display()))?;
        initialize_schema(&mut connection)?;
        Ok(())
    })
    .await?
}

pub(super) async fn validate_database_schema(path: &Path) -> Result<bool> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<bool> {
        let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("Failed to open {}", path.display()))?;
        if runtime_schema_version(&connection)? != RUNTIME_SCHEMA_VERSION
            || validate_runtime_migration_history(&connection).is_err()
        {
            return Ok(false);
        }
        for table in ["tasks", "runs", "events", "runtime_schema_migrations"] {
            let exists: i64 = connection.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )?;
            if exists == 0 {
                return Ok(false);
            }
        }
        for column in [
            "paused_status",
            "claimed_by",
            "lease_until",
            "last_heartbeat",
            "check_retries",
            "review_cycles",
            "failure_reason",
            "awaiting_human_by",
            "awaiting_human_status",
            "baseline_snapshot_id",
            "overlay_revision_id",
            "repository_view_snapshot_id",
            "repository_view_tree_algorithm",
            "repository_view_tree_digest",
            "repository_view_lifecycle",
            "repository_view_status",
        ] {
            if !column_exists(&connection, "tasks", column)? {
                return Ok(false);
            }
        }
        for column in [
            "baseline_snapshot_id",
            "overlay_revision_id",
            "repository_view_snapshot_id",
            "repository_view_tree_algorithm",
            "repository_view_tree_digest",
            "repository_view_lifecycle",
            "repository_view_status",
        ] {
            if !column_exists(&connection, "runs", column)? {
                return Ok(false);
            }
        }
        for column in [
            "canonical_source_revision_id",
            "canonical_manifest_algorithm",
            "canonical_manifest_digest",
            "canonical_graph_snapshot_id",
            "canonical_graph_status",
            "canonical_graph_updated_at",
        ] {
            if !column_exists(&connection, "project_runtime_state", column)? {
                return Ok(false);
            }
        }
        Ok(true)
    })
    .await?
}

pub(super) async fn max_task_number_from_files(tasks_dir: &Path) -> Result<u32> {
    let mut max_number = 0;
    let mut entries = tokio::fs::read_dir(tasks_dir)
        .await
        .with_context(|| format!("Failed to read {}", tasks_dir.display()))?;
    while let Some(entry) = entries.next_entry().await? {
        let Some(file_name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if let Some(number) = parse_task_number(file_name.strip_suffix(".md").unwrap_or(&file_name))
        {
            max_number = max_number.max(number);
        }
    }
    Ok(max_number)
}

pub(super) fn max_task_number_in_database(connection: &Connection) -> Result<u32> {
    let mut statement = connection.prepare("SELECT id FROM tasks WHERE id LIKE 't-%'")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut max_number = 0;
    for row in rows {
        if let Some(number) = parse_task_number(&row?) {
            max_number = max_number.max(number);
        }
    }
    Ok(max_number)
}

pub(super) async fn read_task_records_from_database(path: &Path) -> Result<Vec<TaskRecord>> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<Vec<TaskRecord>> {
        let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("Failed to open {}", path.display()))?;
        let mut statement = connection.prepare(
            r#"
            SELECT id, path, spec_path, milestone_id, status, paused_status, claimed_by,
                   lease_until, last_heartbeat, check_retries, review_cycles, failure_reason
            FROM tasks
            ORDER BY id
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

pub(super) async fn current_database_path() -> Result<PathBuf> {
    let local_ref = read_local_project_ref()
        .await
        .context(".ferrus/project.toml not found -- run `ferrus migrate`")?;
    Ok(PathBuf::from(local_ref.data_dir).join("ferrus.db"))
}

pub(super) async fn current_task_record() -> CurrentTaskRecord {
    CurrentTaskRecord {
        id: CURRENT_TASK_ID.to_string(),
        path: CURRENT_TASK_PATH.to_string(),
        #[cfg(test)]
        spec_path: None,
        #[cfg(test)]
        milestone_id: None,
    }
}

pub(super) async fn current_task_identity() -> (String, String) {
    let task = current_task_record().await;
    (task.id, task.path)
}

pub(super) fn open_runtime_database(path: &Path) -> Result<Connection> {
    let mut connection =
        Connection::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    connection.busy_timeout(Duration::from_secs(5))?;
    initialize_schema(&mut connection)?;
    Ok(connection)
}

pub(crate) async fn prepare_runtime_database_for_read_only_operations() -> Result<()> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || {
        prepare_runtime_database_for_read_only_operations_at(&database_path)
    })
    .await?
}

pub(super) fn prepare_runtime_database_for_read_only_operations_at(path: &Path) -> Result<()> {
    drop(open_runtime_database(path)?);
    Ok(())
}

pub(super) fn initialize_schema(connection: &mut Connection) -> Result<()> {
    connection.execute_batch("PRAGMA foreign_keys = ON;")?;
    migrate_runtime_schema(connection)?;
    // Some pre-migration installations wrote this compatibility table lazily.
    // Keep importing it idempotently even after the schema baseline is adopted.
    migrate_legacy_runtime_metadata(connection)
}

struct RuntimeMigration {
    version: u32,
    name: &'static str,
    apply: fn(&Transaction<'_>) -> Result<()>,
}

const RUNTIME_MIGRATIONS: &[RuntimeMigration] = &[
    RuntimeMigration {
        version: 1,
        name: "adopt_legacy_runtime_schema",
        apply: adopt_legacy_runtime_schema,
    },
    RuntimeMigration {
        version: 2,
        name: "repository_view_references",
        apply: add_repository_view_references,
    },
    RuntimeMigration {
        version: 3,
        name: "frozen_repository_views",
        apply: add_frozen_repository_views,
    },
    RuntimeMigration {
        version: 4,
        name: "canonical_graph_state",
        apply: add_canonical_graph_state,
    },
];

pub(super) fn migrate_runtime_schema(connection: &mut Connection) -> Result<()> {
    validate_runtime_migration_history(connection)?;

    for migration in RUNTIME_MIGRATIONS {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_version = runtime_schema_version(&transaction)?;
        if current_version >= migration.version {
            transaction.commit()?;
            continue;
        }
        if current_version + 1 != migration.version {
            anyhow::bail!(
                "Cannot apply ferrus.db migration {} after schema version {}",
                migration.version,
                current_version
            );
        }

        (migration.apply)(&transaction).with_context(|| {
            format!(
                "Failed to apply ferrus.db migration {} ({})",
                migration.version, migration.name
            )
        })?;
        transaction.execute(
            r#"
            INSERT INTO runtime_schema_migrations (version, name, applied_at)
            VALUES (?1, ?2, ?3)
            "#,
            params![migration.version, migration.name, timestamp()],
        )?;
        transaction.pragma_update(None, "user_version", migration.version)?;
        transaction.commit()?;
    }

    validate_runtime_migration_history(connection)
}

pub(super) fn validate_runtime_migration_history(connection: &Connection) -> Result<()> {
    let version = runtime_schema_version(connection)?;
    if version > RUNTIME_SCHEMA_VERSION {
        anyhow::bail!(
            "ferrus.db schema version {version} is newer than supported version {RUNTIME_SCHEMA_VERSION}"
        );
    }

    if !table_exists(connection, "runtime_schema_migrations")? {
        if version == 0 {
            return Ok(());
        }
        anyhow::bail!("ferrus.db migration history is missing for schema version {version}");
    }

    let mut statement = connection
        .prepare("SELECT version, name FROM runtime_schema_migrations ORDER BY version")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?))
    })?;
    let history = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    if history.len() != version as usize {
        anyhow::bail!("ferrus.db migration history is incomplete for schema version {version}");
    }
    for (index, (applied_version, applied_name)) in history.iter().enumerate() {
        let expected = &RUNTIME_MIGRATIONS[index];
        if *applied_version != expected.version || applied_name != expected.name {
            anyhow::bail!(
                "ferrus.db migration history diverges at version {}",
                expected.version
            );
        }
    }
    Ok(())
}

pub(super) fn runtime_schema_version(connection: &Connection) -> Result<u32> {
    Ok(connection.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

pub(super) fn adopt_legacy_runtime_schema(connection: &Transaction<'_>) -> Result<()> {
    connection.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS runtime_schema_migrations (
            version INTEGER PRIMARY KEY,
            name TEXT NOT NULL UNIQUE,
            applied_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS tasks (
            id TEXT PRIMARY KEY,
            path TEXT NOT NULL,
            status TEXT NOT NULL,
            paused_status TEXT,
            spec_path TEXT,
            milestone_id TEXT,
            claimed_by TEXT,
            lease_until TEXT,
            last_heartbeat TEXT,
            check_retries INTEGER NOT NULL DEFAULT 0,
            review_cycles INTEGER NOT NULL DEFAULT 0,
            failure_reason TEXT,
            awaiting_human_by TEXT,
            awaiting_human_status TEXT,
            human_question_order INTEGER,
            human_answer_recorded INTEGER NOT NULL DEFAULT 0,
            -- Appended to match the ensure_column migration order for existing DBs.
            executor_dispatches INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS runs (
            id TEXT PRIMARY KEY,
            task_id TEXT NOT NULL,
            role TEXT NOT NULL,
            agent TEXT NOT NULL,
            status TEXT NOT NULL,
            started_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            pid INTEGER,
            workspace_path TEXT NOT NULL,
            FOREIGN KEY(task_id) REFERENCES tasks(id)
        );

        CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id TEXT,
            type TEXT NOT NULL,
            payload_json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            FOREIGN KEY(run_id) REFERENCES runs(id)
        );

        CREATE TABLE IF NOT EXISTS project_runtime_state (
            row_id INTEGER PRIMARY KEY CHECK (row_id = 1),
            selected_spec TEXT,
            last_spec_path TEXT,
            last_archive_path TEXT,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS spec_archives (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            spec_path TEXT NOT NULL,
            archive_dir TEXT NOT NULL,
            closed_at TEXT NOT NULL,
            task_count INTEGER NOT NULL,
            run_count INTEGER NOT NULL,
            outcome TEXT NOT NULL
        );
        "#,
    )?;
    ensure_column(connection, "tasks", "paused_status", "TEXT")?;
    ensure_column(connection, "tasks", "spec_path", "TEXT")?;
    ensure_column(connection, "tasks", "milestone_id", "TEXT")?;
    ensure_column(connection, "tasks", "claimed_by", "TEXT")?;
    ensure_column(connection, "tasks", "lease_until", "TEXT")?;
    ensure_column(connection, "tasks", "last_heartbeat", "TEXT")?;
    ensure_column(
        connection,
        "tasks",
        "check_retries",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        connection,
        "tasks",
        "review_cycles",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        connection,
        "tasks",
        "executor_dispatches",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(connection, "tasks", "failure_reason", "TEXT")?;
    ensure_column(connection, "tasks", "awaiting_human_by", "TEXT")?;
    ensure_column(connection, "tasks", "awaiting_human_status", "TEXT")?;
    ensure_column(connection, "tasks", "human_question_order", "INTEGER")?;
    ensure_column(
        connection,
        "tasks",
        "human_answer_recorded",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(connection, "project_runtime_state", "selected_spec", "TEXT")?;
    ensure_column(
        connection,
        "project_runtime_state",
        "last_spec_path",
        "TEXT",
    )?;
    ensure_column(
        connection,
        "project_runtime_state",
        "last_archive_path",
        "TEXT",
    )?;
    migrate_legacy_runtime_metadata(connection)?;
    Ok(())
}

pub(super) fn add_repository_view_references(connection: &Transaction<'_>) -> Result<()> {
    const STATUS_COLUMN: &str = "TEXT NOT NULL DEFAULT 'not_built' CHECK \
        (repository_view_status IN ('not_built', 'available', 'stale', 'unavailable', 'failed'))";

    for table in ["tasks", "runs"] {
        ensure_column(connection, table, "baseline_snapshot_id", "TEXT")?;
        ensure_column(connection, table, "overlay_revision_id", "TEXT")?;
        ensure_column(connection, table, "repository_view_status", STATUS_COLUMN)?;
    }
    connection.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS idx_tasks_repository_view_baseline
            ON tasks(baseline_snapshot_id);
        CREATE INDEX IF NOT EXISTS idx_runs_repository_view_baseline
            ON runs(baseline_snapshot_id);
        "#,
    )?;
    Ok(())
}

pub(super) fn add_frozen_repository_views(connection: &Transaction<'_>) -> Result<()> {
    const LIFECYCLE_COLUMN: &str = "TEXT NOT NULL DEFAULT 'mutable' CHECK \
        (repository_view_lifecycle IN ('mutable', 'frozen_submitted'))";

    for table in ["tasks", "runs"] {
        ensure_column(connection, table, "repository_view_snapshot_id", "TEXT")?;
        ensure_column(connection, table, "repository_view_tree_algorithm", "TEXT")?;
        ensure_column(connection, table, "repository_view_tree_digest", "TEXT")?;
        ensure_column(
            connection,
            table,
            "repository_view_lifecycle",
            LIFECYCLE_COLUMN,
        )?;
    }
    connection.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS idx_tasks_repository_view_snapshot
            ON tasks(repository_view_snapshot_id);
        CREATE INDEX IF NOT EXISTS idx_runs_repository_view_snapshot
            ON runs(repository_view_snapshot_id);
        "#,
    )?;
    Ok(())
}

pub(super) fn add_canonical_graph_state(connection: &Transaction<'_>) -> Result<()> {
    ensure_column(
        connection,
        "project_runtime_state",
        "canonical_source_revision_id",
        "TEXT",
    )?;
    ensure_column(
        connection,
        "project_runtime_state",
        "canonical_manifest_algorithm",
        "TEXT",
    )?;
    ensure_column(
        connection,
        "project_runtime_state",
        "canonical_manifest_digest",
        "TEXT",
    )?;
    ensure_column(
        connection,
        "project_runtime_state",
        "canonical_graph_snapshot_id",
        "TEXT",
    )?;
    ensure_column(
        connection,
        "project_runtime_state",
        "canonical_graph_status",
        "TEXT NOT NULL DEFAULT 'unknown' CHECK (canonical_graph_status IN ('unknown', 'stale', 'fresh'))",
    )?;
    ensure_column(
        connection,
        "project_runtime_state",
        "canonical_graph_updated_at",
        "TEXT",
    )?;
    ensure_project_runtime_state_row(connection)?;
    Ok(())
}

pub(super) fn upsert_task(
    connection: &Connection,
    id: &str,
    path: &str,
    status: TaskStatus,
    spec_path: Option<&str>,
    milestone_id: Option<&str>,
) -> Result<()> {
    connection.execute(
        r#"
        INSERT INTO tasks (id, path, status, spec_path, milestone_id)
        VALUES (?1, ?2, ?3, ?4, ?5)
        ON CONFLICT(id) DO UPDATE SET
            path = excluded.path,
            status = excluded.status,
            spec_path = COALESCE(excluded.spec_path, tasks.spec_path),
            milestone_id = COALESCE(excluded.milestone_id, tasks.milestone_id)
        "#,
        params![id, path, status.as_str(), spec_path, milestone_id],
    )?;
    Ok(())
}

pub(super) fn ensure_task_exists(connection: &Connection, id: &str, path: &str) -> Result<()> {
    connection.execute(
        "INSERT OR IGNORE INTO tasks (id, path, status) VALUES (?1, ?2, ?3)",
        params![id, path, TaskStatus::Unknown.as_str()],
    )?;
    Ok(())
}

pub(super) fn read_project_selection_from_database(
    connection: &Connection,
) -> Result<ProjectSelection> {
    let selection = connection
        .query_row(
            r#"
            SELECT selected_spec
            FROM project_runtime_state
            WHERE row_id = 1
            "#,
            [],
            |row| {
                Ok(ProjectSelection {
                    selected_spec: normalize_optional_db_string(row.get(0)?),
                })
            },
        )
        .optional()?
        .unwrap_or_default();
    Ok(selection)
}

pub(super) fn write_project_selection_to_database(
    connection: &Connection,
    selection: &ProjectSelection,
) -> Result<()> {
    ensure_project_runtime_state_row(connection)?;
    connection.execute(
        r#"
        UPDATE project_runtime_state
        SET selected_spec = ?1, updated_at = ?2
        WHERE row_id = 1
        "#,
        params![
            normalized_metadata_value(selection.selected_spec.as_deref()),
            timestamp()
        ],
    )?;
    Ok(())
}

pub(super) fn read_last_spec_path_from_database(connection: &Connection) -> Result<Option<String>> {
    let value = connection
        .query_row(
            "SELECT last_spec_path FROM project_runtime_state WHERE row_id = 1",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    Ok(normalize_optional_db_string(value))
}

pub(super) fn write_last_spec_path_to_database(
    connection: &Connection,
    path: Option<&str>,
) -> Result<()> {
    ensure_project_runtime_state_row(connection)?;
    connection.execute(
        r#"
        UPDATE project_runtime_state
        SET last_spec_path = ?1, updated_at = ?2
        WHERE row_id = 1
        "#,
        params![normalized_metadata_value(path), timestamp()],
    )?;
    Ok(())
}

pub(super) fn read_last_spec_archive_path_from_database(
    connection: &Connection,
) -> Result<Option<String>> {
    let value = connection
        .query_row(
            "SELECT last_archive_path FROM project_runtime_state WHERE row_id = 1",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    Ok(normalize_optional_db_string(value))
}

pub(super) fn write_last_spec_archive_path_to_database(
    connection: &Connection,
    path: Option<&str>,
) -> Result<()> {
    ensure_project_runtime_state_row(connection)?;
    connection.execute(
        r#"
        UPDATE project_runtime_state
        SET last_archive_path = ?1, updated_at = ?2
        WHERE row_id = 1
        "#,
        params![normalized_metadata_value(path), timestamp()],
    )?;
    Ok(())
}

pub(super) fn ensure_project_runtime_state_row(connection: &Connection) -> Result<()> {
    connection.execute(
        r#"
        INSERT INTO project_runtime_state (row_id, updated_at)
        VALUES (1, ?1)
        ON CONFLICT(row_id) DO NOTHING
        "#,
        [timestamp()],
    )?;
    Ok(())
}

pub(super) fn migrate_legacy_runtime_metadata(connection: &Connection) -> Result<()> {
    if !table_exists(connection, "runtime_metadata")? {
        return Ok(());
    }

    let current_selection = read_project_selection_from_database(connection)?;
    let current_last_spec_path = read_last_spec_path_from_database(connection)?;
    let selected_spec = current_selection
        .selected_spec
        .or(read_legacy_runtime_metadata(connection, "selected_spec")?);
    let last_spec_path =
        current_last_spec_path.or(read_legacy_runtime_metadata(connection, "last_spec_path")?);

    if selected_spec.is_none() && last_spec_path.is_none() {
        return Ok(());
    }

    ensure_project_runtime_state_row(connection)?;
    connection.execute(
        r#"
        UPDATE project_runtime_state
        SET selected_spec = ?1,
            last_spec_path = ?2,
            updated_at = ?3
        WHERE row_id = 1
        "#,
        params![selected_spec, last_spec_path, timestamp()],
    )?;
    Ok(())
}

pub(super) fn read_legacy_runtime_metadata(
    connection: &Connection,
    metadata_name: &str,
) -> Result<Option<String>> {
    let value = connection
        .query_row(
            "SELECT value FROM runtime_metadata WHERE key = ?1",
            [metadata_name],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    Ok(normalize_optional_db_string(value))
}

pub(super) fn table_exists(connection: &Connection, table_name: &str) -> Result<bool> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1 LIMIT 1",
            [table_name],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    Ok(exists)
}

pub(super) fn normalized_metadata_value(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub(super) fn normalize_optional_db_string(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub(super) fn task_check_retries(connection: &Connection, task_id: &str) -> Result<u32> {
    let retries = connection
        .query_row(
            "SELECT check_retries FROM tasks WHERE id = ?1",
            [task_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0);
    Ok(retries as u32)
}

pub(super) fn task_review_cycles(connection: &Connection, task_id: &str) -> Result<u32> {
    let cycles = connection
        .query_row(
            "SELECT review_cycles FROM tasks WHERE id = ?1",
            [task_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0);
    Ok(cycles as u32)
}

pub(super) fn task_executor_dispatches(connection: &Connection, task_id: &str) -> Result<u32> {
    let dispatches = connection
        .query_row(
            "SELECT executor_dispatches FROM tasks WHERE id = ?1",
            [task_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0);
    Ok(dispatches as u32)
}

pub(super) struct ReadyTaskCandidate {
    pub(super) id: String,
    pub(super) path: String,
    pub(super) status: String,
    pub(super) paused_status: Option<String>,
    pub(super) check_retries: u32,
    pub(super) review_cycles: u32,
    pub(super) failure_reason: Option<String>,
    pub(super) claimed_by: Option<String>,
    pub(super) lease_until: Option<String>,
}

pub(super) fn task_candidates_by_status(
    transaction: &Transaction<'_>,
    statuses: &[TaskStatus],
) -> Result<Vec<ReadyTaskCandidate>> {
    if statuses.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = std::iter::repeat_n("?", statuses.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        r#"
        SELECT id, path, status, paused_status, check_retries, review_cycles, failure_reason,
               claimed_by, lease_until
        FROM tasks
        WHERE status IN ({placeholders})
        ORDER BY id
        "#
    );
    let mut statement = transaction.prepare(&sql)?;
    let rows = statement.query_map(
        rusqlite::params_from_iter(statuses.iter().map(|status| status.as_str())),
        |row| {
            Ok(ReadyTaskCandidate {
                id: row.get(0)?,
                path: row.get(1)?,
                status: row.get(2)?,
                paused_status: row.get(3)?,
                check_retries: row.get::<_, i64>(4)? as u32,
                review_cycles: row.get::<_, i64>(5)? as u32,
                failure_reason: row.get(6)?,
                claimed_by: row.get(7)?,
                lease_until: row.get(8)?,
            })
        },
    )?;

    let mut tasks = Vec::new();
    for row in rows {
        tasks.push(row?);
    }
    Ok(tasks)
}

pub(super) fn task_candidate_by_id(
    transaction: &Transaction<'_>,
    task_id: &str,
) -> Result<Option<ReadyTaskCandidate>> {
    let task = transaction
        .query_row(
            r#"
            SELECT id, path, status, paused_status, check_retries, review_cycles, failure_reason,
                   claimed_by, lease_until
            FROM tasks
            WHERE id = ?1
            LIMIT 1
            "#,
            [task_id],
            |row| {
                Ok(ReadyTaskCandidate {
                    id: row.get(0)?,
                    path: row.get(1)?,
                    status: row.get(2)?,
                    paused_status: row.get(3)?,
                    check_retries: row.get::<_, i64>(4)? as u32,
                    review_cycles: row.get::<_, i64>(5)? as u32,
                    failure_reason: row.get(6)?,
                    claimed_by: row.get(7)?,
                    lease_until: row.get(8)?,
                })
            },
        )
        .optional()?;
    Ok(task)
}

pub(super) fn claim_task_in_transaction(
    transaction: &Transaction<'_>,
    task_id: &str,
    agent_id: &str,
    lease_until: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<()> {
    let lease_until_text = lease_until.to_rfc3339_opts(SecondsFormat::Secs, true);
    let now_text = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    transaction.execute(
        "UPDATE tasks SET claimed_by = ?1, lease_until = ?2, last_heartbeat = ?3 WHERE id = ?4",
        params![agent_id, lease_until_text, now_text, task_id],
    )?;
    insert_event_in_transaction(
        transaction,
        None,
        "task_claimed",
        &serde_json::json!({
            "task_id": task_id,
            "claimed_by": agent_id,
            "lease_until": lease_until,
        }),
    )?;
    Ok(())
}

pub(super) fn parse_lease_until(value: Option<&str>) -> Option<DateTime<Utc>> {
    value
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

pub(super) fn clear_task_lease(connection: &Connection, task_id: &str) -> Result<()> {
    connection.execute(
        "UPDATE tasks SET claimed_by = NULL, lease_until = NULL, last_heartbeat = NULL WHERE id = ?1",
        [task_id],
    )?;
    Ok(())
}

pub(super) fn insert_event(
    connection: &Connection,
    run_id: Option<&str>,
    event_type: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    if let Some(run_id) = run_id {
        let exists = connection
            .query_row("SELECT 1 FROM runs WHERE id = ?1", [run_id], |_| Ok(()))
            .optional()?
            .is_some();
        if !exists {
            anyhow::bail!("Cannot insert event for unknown run id {run_id}");
        }
    }
    connection.execute(
        "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![
            run_id,
            event_type,
            serde_json::to_string(payload)?,
            timestamp()
        ],
    )?;
    Ok(())
}

pub(super) fn insert_event_in_transaction(
    transaction: &Transaction<'_>,
    run_id: Option<&str>,
    event_type: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    transaction.execute(
        "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![
            run_id,
            event_type,
            serde_json::to_string(payload)?,
            timestamp()
        ],
    )?;
    Ok(())
}

pub(super) fn ensure_column(
    connection: &Connection,
    table_name: &str,
    column_name: &str,
    column_type: &str,
) -> Result<()> {
    if column_exists(connection, table_name, column_name)? {
        return Ok(());
    }
    connection.execute(
        &format!("ALTER TABLE {table_name} ADD COLUMN {column_name} {column_type}"),
        [],
    )?;
    Ok(())
}

pub(super) fn column_exists(
    connection: &Connection,
    table_name: &str,
    column_name: &str,
) -> Result<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table_name})"))?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    for column in columns {
        if column? == column_name {
            return Ok(true);
        }
    }
    Ok(false)
}
