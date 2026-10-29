//! Runtime schema migrations and validation.

use super::*;

pub(in super::super) fn initialize_schema(connection: &mut Connection) -> Result<()> {
    connection.execute_batch("PRAGMA foreign_keys = ON;")?;
    migrate_runtime_schema(connection)?;
    // Some pre-migration installations wrote this compatibility table lazily.
    // Keep importing it idempotently even after the schema baseline is adopted.
    migrate_legacy_runtime_metadata(connection)
}

pub(super) struct RuntimeMigration {
    pub(super) version: u32,
    pub(super) name: &'static str,
    pub(super) apply: fn(&Transaction<'_>) -> Result<()>,
}

pub(super) const RUNTIME_MIGRATIONS: &[RuntimeMigration] = &[
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

pub(in super::super) fn migrate_runtime_schema(connection: &mut Connection) -> Result<()> {
    loop {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_runtime_migration_history(&transaction)?;
        let current_version = runtime_schema_version(&transaction)?;
        if current_version == RUNTIME_SCHEMA_VERSION {
            transaction.commit()?;
            break;
        }

        let migration = &RUNTIME_MIGRATIONS[current_version as usize];
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

pub(in super::super) fn validate_runtime_migration_history(connection: &Connection) -> Result<()> {
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

pub(in super::super) fn runtime_schema_version(connection: &Connection) -> Result<u32> {
    Ok(connection.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

pub(in super::super) fn adopt_legacy_runtime_schema(connection: &Transaction<'_>) -> Result<()> {
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

pub(in super::super) fn add_repository_view_references(connection: &Transaction<'_>) -> Result<()> {
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

pub(in super::super) fn add_frozen_repository_views(connection: &Transaction<'_>) -> Result<()> {
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

pub(in super::super) fn add_canonical_graph_state(connection: &Transaction<'_>) -> Result<()> {
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
