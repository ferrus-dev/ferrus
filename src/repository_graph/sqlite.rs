//! Versioned local SQLite sidecar for rebuildable repository graph data.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};

pub const SIDECAR_FILE_NAME: &str = "repo-graph.db";
pub const SIDECAR_SCHEMA_VERSION: u32 = 2;
const SIDECAR_APPLICATION_ID: u32 = 0x4652_4731; // "FRG1"

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarStatus {
    Absent,
    Ready { schema_version: u32 },
    RequiresRebuild(RebuildRequired),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuildRequired {
    pub found_schema_version: u32,
    pub supported_schema_version: u32,
    pub reason: String,
}

pub enum OpenSidecarResult {
    Ready(Sidecar),
    RequiresRebuild(RebuildRequired),
}

pub struct Sidecar {
    path: PathBuf,
    connection: Connection,
}

impl Sidecar {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }

    pub(crate) fn connection_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }
}

struct Migration {
    version: u32,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: r#"
        CREATE TABLE schema_migrations (
            version INTEGER PRIMARY KEY CHECK (version > 0),
            applied_at TEXT NOT NULL
        ) STRICT;

        CREATE TABLE index_builds (
            id TEXT PRIMARY KEY NOT NULL,
            repository_namespace TEXT NOT NULL,
            repository_id TEXT NOT NULL,
            source_revision_id TEXT NOT NULL,
            prospective_snapshot_id TEXT NOT NULL,
            state TEXT NOT NULL CHECK (state IN ('building', 'published', 'failed', 'superseded')),
            started_at TEXT NOT NULL,
            finished_at TEXT,
            failure_code TEXT,
            failure_message TEXT
        ) STRICT;

        CREATE INDEX index_builds_repository_state_idx
            ON index_builds(repository_namespace, repository_id, state, started_at);
        CREATE INDEX index_builds_snapshot_idx ON index_builds(prospective_snapshot_id);

        CREATE TABLE snapshots (
            id TEXT PRIMARY KEY NOT NULL,
            repository_namespace TEXT NOT NULL,
            repository_id TEXT NOT NULL,
            source_revision_id TEXT NOT NULL,
            source_manifest_algorithm TEXT NOT NULL,
            source_manifest_digest TEXT NOT NULL,
            graph_model_version INTEGER NOT NULL CHECK (graph_model_version > 0),
            analysis_config_algorithm TEXT NOT NULL,
            analysis_config_digest TEXT NOT NULL,
            extractor_set_algorithm TEXT NOT NULL,
            extractor_set_digest TEXT NOT NULL,
            completed_by_build_id TEXT NOT NULL REFERENCES index_builds(id),
            created_at TEXT NOT NULL
        ) STRICT;

        CREATE INDEX snapshots_repository_created_idx
            ON snapshots(repository_namespace, repository_id, created_at);
        CREATE UNIQUE INDEX snapshots_identity_idx ON snapshots(
            repository_namespace,
            repository_id,
            source_manifest_algorithm,
            source_manifest_digest,
            graph_model_version,
            analysis_config_algorithm,
            analysis_config_digest,
            extractor_set_algorithm,
            extractor_set_digest
        );

        CREATE TABLE published_views (
            repository_namespace TEXT NOT NULL,
            repository_id TEXT NOT NULL,
            view_name TEXT NOT NULL,
            snapshot_id TEXT NOT NULL REFERENCES snapshots(id),
            build_id TEXT NOT NULL REFERENCES index_builds(id),
            generation INTEGER NOT NULL CHECK (generation > 0),
            published_at TEXT NOT NULL,
            PRIMARY KEY (repository_namespace, repository_id, view_name)
        ) STRICT;

        CREATE INDEX published_views_snapshot_idx ON published_views(snapshot_id);

        CREATE TABLE files (
            snapshot_id TEXT NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
            path TEXT NOT NULL CHECK (
                length(path) > 0
                AND substr(path, 1, 1) <> '/'
                AND instr(path, char(0)) = 0
                AND instr(path, '\') = 0
                AND instr(path, '//') = 0
                AND instr('/' || path || '/', '/../') = 0
                AND instr('/' || path || '/', '/./') = 0
                AND path NOT GLOB '[A-Za-z]:*'
            ),
            content_algorithm TEXT NOT NULL,
            content_digest TEXT NOT NULL,
            byte_length INTEGER NOT NULL CHECK (byte_length >= 0),
            file_mode INTEGER,
            language TEXT,
            PRIMARY KEY (snapshot_id, path)
        ) STRICT;

        CREATE TABLE nodes (
            snapshot_id TEXT NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
            id TEXT NOT NULL,
            kind TEXT NOT NULL,
            semantic_key TEXT,
            extractor_id TEXT NOT NULL,
            extractor_version TEXT NOT NULL,
            extractor_contract_version INTEGER NOT NULL CHECK (extractor_contract_version > 0),
            resolution_state TEXT NOT NULL CHECK (resolution_state IN ('resolved', 'unresolved', 'external')),
            confidence TEXT NOT NULL CHECK (confidence IN ('exact', 'high', 'medium', 'low')),
            evidence_path TEXT,
            evidence_content_algorithm TEXT,
            evidence_content_digest TEXT,
            span_start_byte INTEGER CHECK (span_start_byte IS NULL OR span_start_byte >= 0),
            span_end_byte INTEGER CHECK (span_end_byte IS NULL OR span_end_byte >= span_start_byte),
            properties_json TEXT NOT NULL DEFAULT '{}',
            PRIMARY KEY (snapshot_id, id),
            FOREIGN KEY (snapshot_id, evidence_path) REFERENCES files(snapshot_id, path)
        ) STRICT;

        CREATE INDEX nodes_kind_idx ON nodes(snapshot_id, kind);
        CREATE INDEX nodes_semantic_key_idx ON nodes(snapshot_id, semantic_key)
            WHERE semantic_key IS NOT NULL;
        CREATE INDEX nodes_evidence_path_idx ON nodes(snapshot_id, evidence_path)
            WHERE evidence_path IS NOT NULL;

        CREATE TABLE edges (
            snapshot_id TEXT NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
            id TEXT NOT NULL,
            kind TEXT NOT NULL,
            source_node_id TEXT NOT NULL,
            target_node_id TEXT,
            external_target TEXT,
            extractor_id TEXT NOT NULL,
            extractor_version TEXT NOT NULL,
            extractor_contract_version INTEGER NOT NULL CHECK (extractor_contract_version > 0),
            resolution_state TEXT NOT NULL CHECK (resolution_state IN ('resolved', 'unresolved', 'external')),
            confidence TEXT NOT NULL CHECK (confidence IN ('exact', 'high', 'medium', 'low')),
            evidence_path TEXT,
            evidence_content_algorithm TEXT,
            evidence_content_digest TEXT,
            span_start_byte INTEGER CHECK (span_start_byte IS NULL OR span_start_byte >= 0),
            span_end_byte INTEGER CHECK (span_end_byte IS NULL OR span_end_byte >= span_start_byte),
            properties_json TEXT NOT NULL DEFAULT '{}',
            CHECK ((target_node_id IS NOT NULL) <> (external_target IS NOT NULL)),
            PRIMARY KEY (snapshot_id, id),
            FOREIGN KEY (snapshot_id, source_node_id) REFERENCES nodes(snapshot_id, id),
            FOREIGN KEY (snapshot_id, target_node_id) REFERENCES nodes(snapshot_id, id),
            FOREIGN KEY (snapshot_id, evidence_path) REFERENCES files(snapshot_id, path)
        ) STRICT;

        CREATE INDEX edges_source_idx ON edges(snapshot_id, source_node_id, kind);
        CREATE INDEX edges_target_idx ON edges(snapshot_id, target_node_id, kind)
            WHERE target_node_id IS NOT NULL;
        CREATE INDEX edges_evidence_path_idx ON edges(snapshot_id, evidence_path)
            WHERE evidence_path IS NOT NULL;

        CREATE TABLE diagnostics (
            id INTEGER PRIMARY KEY,
            build_id TEXT NOT NULL REFERENCES index_builds(id) ON DELETE CASCADE,
            snapshot_id TEXT REFERENCES snapshots(id) ON DELETE CASCADE,
            severity TEXT NOT NULL CHECK (severity IN ('info', 'warning', 'error')),
            code TEXT NOT NULL,
            message TEXT NOT NULL,
            path TEXT,
            span_start_byte INTEGER CHECK (span_start_byte IS NULL OR span_start_byte >= 0),
            span_end_byte INTEGER CHECK (span_end_byte IS NULL OR span_end_byte >= span_start_byte),
            metadata_json TEXT NOT NULL DEFAULT '{}',
            created_at TEXT NOT NULL,
            FOREIGN KEY (snapshot_id, path) REFERENCES files(snapshot_id, path)
        ) STRICT;

        CREATE INDEX diagnostics_build_idx ON diagnostics(build_id, severity, id);
        CREATE INDEX diagnostics_snapshot_idx ON diagnostics(snapshot_id, id)
            WHERE snapshot_id IS NOT NULL;
    "#,
    },
    Migration {
        version: 2,
        sql: r#"
        ALTER TABLE diagnostics ADD COLUMN span_start_line INTEGER
            CHECK (span_start_line IS NULL OR span_start_line >= 0);
        ALTER TABLE diagnostics ADD COLUMN span_start_column INTEGER
            CHECK (span_start_column IS NULL OR span_start_column >= 0);
        ALTER TABLE diagnostics ADD COLUMN span_end_line INTEGER
            CHECK (span_end_line IS NULL OR span_end_line >= 0);
        ALTER TABLE diagnostics ADD COLUMN span_end_column INTEGER
            CHECK (span_end_column IS NULL OR span_end_column >= 0);
    "#,
    },
];

/// Resolves the sidecar beside `ferrus.db` through the registered project.
/// This function is read-only and does not create the sidecar or its directory.
pub async fn current_sidecar_path() -> Result<PathBuf> {
    Ok(crate::project::current_project_data_dir()
        .await?
        .join(SIDECAR_FILE_NAME))
}

/// Inspects optional graph storage without creating or migrating it.
pub async fn inspect_current() -> Result<SidecarStatus> {
    inspect_at(&current_sidecar_path().await?)
}

/// Explicit write/build entrypoint. This is the only operation in this phase
/// that creates and migrates an absent sidecar.
pub async fn open_current_for_build() -> Result<OpenSidecarResult> {
    open_for_build_at(&current_sidecar_path().await?)
}

pub fn inspect_at(path: &Path) -> Result<SidecarStatus> {
    if !path.exists() {
        return Ok(SidecarStatus::Absent);
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| {
        format!(
            "Failed to inspect repository graph sidecar {}",
            path.display()
        )
    })?;
    let schema_version = pragma_u32(&connection, "user_version")?;
    let application_id = pragma_u32(&connection, "application_id")?;
    if application_id != SIDECAR_APPLICATION_ID {
        return Ok(SidecarStatus::RequiresRebuild(RebuildRequired {
            found_schema_version: schema_version,
            supported_schema_version: SIDECAR_SCHEMA_VERSION,
            reason: "file is not a Ferrus repository graph sidecar".to_string(),
        }));
    }
    let supported_version = MIGRATIONS
        .iter()
        .any(|migration| migration.version == schema_version);
    if schema_version > SIDECAR_SCHEMA_VERSION || !supported_version {
        return Ok(SidecarStatus::RequiresRebuild(RebuildRequired {
            found_schema_version: schema_version,
            supported_schema_version: SIDECAR_SCHEMA_VERSION,
            reason: format!(
                "sidecar schema version {schema_version} is incompatible; delete or rebuild the derived index"
            ),
        }));
    }
    let migration_version = match connection
        .query_row(
            "SELECT version FROM schema_migrations ORDER BY version DESC LIMIT 1",
            [],
            |row| row.get::<_, u32>(0),
        )
        .optional()
    {
        Ok(version) => version,
        Err(_) => {
            return Ok(SidecarStatus::RequiresRebuild(RebuildRequired {
                found_schema_version: schema_version,
                supported_schema_version: SIDECAR_SCHEMA_VERSION,
                reason: "sidecar migration metadata is missing or unreadable".to_string(),
            }));
        }
    };
    if migration_version != Some(schema_version) {
        return Ok(SidecarStatus::RequiresRebuild(RebuildRequired {
            found_schema_version: schema_version,
            supported_schema_version: SIDECAR_SCHEMA_VERSION,
            reason: "sidecar migration metadata is incomplete".to_string(),
        }));
    }
    Ok(SidecarStatus::Ready { schema_version })
}

pub fn open_for_build_at(path: &Path) -> Result<OpenSidecarResult> {
    let existed = path.exists();
    let schema_version = if existed {
        match inspect_at(path)? {
            SidecarStatus::Ready { schema_version } => schema_version,
            SidecarStatus::RequiresRebuild(reason) => {
                return Ok(OpenSidecarResult::RequiresRebuild(reason));
            }
            SidecarStatus::Absent => {
                anyhow::bail!(
                    "repository graph sidecar {} disappeared while opening",
                    path.display()
                );
            }
        }
    } else {
        0
    };
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("repository graph sidecar path has no parent"))?;
    if !parent.exists() {
        anyhow::bail!(
            "registered project directory {} does not exist",
            parent.display()
        );
    }
    let flags = if existed {
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
    };
    let mut connection = Connection::open_with_flags(path, flags)
        .with_context(|| format!("Failed to open repository graph sidecar {}", path.display()))?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    if schema_version < SIDECAR_SCHEMA_VERSION {
        migrate_sidecar(&mut connection, schema_version)?;
    }
    Ok(OpenSidecarResult::Ready(Sidecar {
        path: path.to_path_buf(),
        connection,
    }))
}

fn migrate_sidecar(connection: &mut Connection, schema_version: u32) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.pragma_update(None, "application_id", SIDECAR_APPLICATION_ID)?;
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version > schema_version)
    {
        transaction.execute_batch(migration.sql).with_context(|| {
            format!(
                "Failed to apply repository graph migration {}",
                migration.version
            )
        })?;
        transaction.execute(
            "INSERT INTO schema_migrations(version, applied_at) VALUES (?1, ?2)",
            params![migration.version, timestamp()],
        )?;
        transaction.pragma_update(None, "user_version", migration.version)?;
    }
    transaction.commit()?;
    Ok(())
}

fn pragma_u32(connection: &Connection, name: &str) -> Result<u32> {
    connection
        .pragma_query_value(None, name, |row| row.get(0))
        .with_context(|| format!("Failed to read repository graph PRAGMA {name}"))
}

fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_sidecar_at_version(path: &Path, schema_version: u32) {
        let mut connection = Connection::open(path).unwrap();
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        transaction
            .pragma_update(None, "application_id", SIDECAR_APPLICATION_ID)
            .unwrap();
        for migration in MIGRATIONS
            .iter()
            .filter(|migration| migration.version <= schema_version)
        {
            transaction.execute_batch(migration.sql).unwrap();
            transaction
                .execute(
                    "INSERT INTO schema_migrations(version, applied_at) VALUES (?1, ?2)",
                    params![migration.version, timestamp()],
                )
                .unwrap();
            transaction
                .pragma_update(None, "user_version", migration.version)
                .unwrap();
        }
        transaction.commit().unwrap();
    }

    #[test]
    fn inspection_does_not_create_an_absent_sidecar() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(SIDECAR_FILE_NAME);
        assert_eq!(inspect_at(&path).unwrap(), SidecarStatus::Absent);
        assert!(!path.exists());
    }

    #[test]
    fn explicit_build_open_creates_versioned_initial_schema() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(SIDECAR_FILE_NAME);
        let OpenSidecarResult::Ready(sidecar) = open_for_build_at(&path).unwrap() else {
            panic!("new sidecar unexpectedly requires rebuild");
        };
        assert_eq!(sidecar.path(), path);
        assert_eq!(
            inspect_at(&path).unwrap(),
            SidecarStatus::Ready {
                schema_version: SIDECAR_SCHEMA_VERSION
            }
        );
        let table_count: u32 = sidecar
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name IN (\
                 'schema_migrations', 'index_builds', 'snapshots', 'published_views', \
                 'files', 'nodes', 'edges', 'diagnostics')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 8);
    }

    #[test]
    fn supported_older_sidecar_is_migrated_before_build_open() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(SIDECAR_FILE_NAME);
        create_sidecar_at_version(&path, 1);

        assert_eq!(
            inspect_at(&path).unwrap(),
            SidecarStatus::Ready { schema_version: 1 }
        );
        let before = Connection::open(&path).unwrap();
        assert_eq!(pragma_u32(&before, "user_version").unwrap(), 1);
        drop(before);

        let OpenSidecarResult::Ready(sidecar) = open_for_build_at(&path).unwrap() else {
            panic!("supported older sidecar unexpectedly requires rebuild");
        };
        assert_eq!(
            inspect_at(&path).unwrap(),
            SidecarStatus::Ready {
                schema_version: SIDECAR_SCHEMA_VERSION
            }
        );
        let migration_count: u32 = sidecar
            .connection()
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(migration_count, SIDECAR_SCHEMA_VERSION);
        let span_column_count: u32 = sidecar
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('diagnostics') \
                 WHERE name IN ('span_start_line', 'span_start_column', \
                                'span_end_line', 'span_end_column')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(span_column_count, 4);
    }

    #[test]
    fn unsupported_version_reports_requires_rebuild_without_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(SIDECAR_FILE_NAME);
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "application_id", SIDECAR_APPLICATION_ID)
            .unwrap();
        connection.pragma_update(None, "user_version", 99).unwrap();
        drop(connection);

        let status = inspect_at(&path).unwrap();
        let SidecarStatus::RequiresRebuild(reason) = status else {
            panic!("unsupported version was not rejected");
        };
        assert_eq!(reason.found_schema_version, 99);
        assert!(reason.reason.contains("delete or rebuild"));
        assert!(matches!(
            open_for_build_at(&path).unwrap(),
            OpenSidecarResult::RequiresRebuild(_)
        ));
        let connection = Connection::open(&path).unwrap();
        assert_eq!(pragma_u32(&connection, "user_version").unwrap(), 99);
    }

    #[test]
    fn sidecar_creation_does_not_modify_sibling_runtime_database() {
        let directory = tempfile::tempdir().unwrap();
        let runtime_path = directory.path().join("ferrus.db");
        std::fs::write(&runtime_path, b"runtime sentinel").unwrap();
        let before = std::fs::read(&runtime_path).unwrap();

        let graph_path = directory.path().join(SIDECAR_FILE_NAME);
        assert!(matches!(
            open_for_build_at(&graph_path).unwrap(),
            OpenSidecarResult::Ready(_)
        ));
        assert_eq!(std::fs::read(runtime_path).unwrap(), before);
    }

    #[test]
    fn supported_schema_fixture_reopens_idempotently() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(SIDECAR_FILE_NAME);
        let OpenSidecarResult::Ready(sidecar) = open_for_build_at(&path).unwrap() else {
            panic!("new sidecar unexpectedly requires rebuild");
        };
        drop(sidecar);

        let OpenSidecarResult::Ready(sidecar) = open_for_build_at(&path).unwrap() else {
            panic!("supported sidecar unexpectedly requires rebuild");
        };
        let migration_count: u32 = sidecar
            .connection()
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(migration_count, SIDECAR_SCHEMA_VERSION);

        let mut statement = sidecar
            .connection()
            .prepare(
                "SELECT type || ':' || name FROM sqlite_schema \
                 WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
            )
            .unwrap();
        let actual = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("\n");
        assert_eq!(
            actual,
            include_str!("fixtures/schema_v2_objects.txt")
                .trim()
                .replace("\r\n", "\n")
        );
    }
}
