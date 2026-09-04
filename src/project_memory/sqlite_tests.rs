//! Memory sidecar tests for immutable revisions, publication, migration, and stored facts.

use super::*;

#[test]
fn version_one_sidecars_migrate_to_repository_link_sets() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(MEMORY_SIDECAR_FILE_NAME);
    drop(MemorySidecar::open(path.clone()).unwrap());
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "DROP TABLE memory_repository_link_diagnostics; \
             DROP TABLE memory_repository_links; \
             DROP TABLE memory_repository_link_sets; \
             PRAGMA user_version = 1;",
        )
        .unwrap();
    drop(connection);

    let sidecar = MemorySidecar::open(path).unwrap();
    let version: u32 = sidecar
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, MEMORY_SIDECAR_SCHEMA_VERSION);
    let tables: i64 = sidecar
        .connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
             AND name LIKE 'memory_repository_link_%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 3);
}

#[test]
fn query_open_reports_legacy_schema_without_migrating_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(MEMORY_SIDECAR_FILE_NAME);
    drop(MemorySidecar::open(path.clone()).unwrap());
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "DROP TABLE memory_repository_link_diagnostics; \
             DROP TABLE memory_repository_links; \
             DROP TABLE memory_repository_link_sets; \
             PRAGMA user_version = 1;",
        )
        .unwrap();
    drop(connection);

    assert!(matches!(
        open_for_query_at(&path).unwrap(),
        OpenMemoryQuerySidecarResult::NeedsMigration {
            found_schema_version: 1
        }
    ));
    let connection = Connection::open(&path).unwrap();
    let version: u32 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 1);
    let tables: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
             AND name LIKE 'memory_repository_link_%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0);
}
