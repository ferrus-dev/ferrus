//! Tests for behavior.

use super::*;

#[test]
fn add_change_delete_and_rename_rebuild_only_affected_fragments() {
    let repository_dir = tempfile::tempdir().unwrap();
    fixture_repository(repository_dir.path());
    let config = RepositoryGraphConfig::default();
    let (_sidecar_dir, mut sidecar) = sidecar();
    let first_source = discover(repository_dir.path(), &config);
    let first = run(&mut sidecar, &first_source, &config, "build-1", false).unwrap();

    fs::rename(
        repository_dir.path().join("src/api.rs"),
        repository_dir.path().join("src/model.rs"),
    )
    .unwrap();
    write(
        &repository_dir.path().join("src/lib.rs"),
        b"mod model;\npub use crate::model::Model;\n",
    );
    write(
        &repository_dir.path().join("src/model.rs"),
        b"pub struct Model;\n",
    );
    write(&repository_dir.path().join("README.md"), b"# Fixture\n");

    let second_source = discover(repository_dir.path(), &config);
    let second = run(&mut sidecar, &second_source, &config, "build-2", false).unwrap();
    assert_ne!(second.snapshot.id, first.snapshot.id);
    assert_eq!(second.metrics.reused_files, 1);
    assert_eq!(second.metrics.parsed_files, 3);
    let stale_nodes: i64 = sidecar
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM nodes WHERE snapshot_id = ?1 AND evidence_path = 'src/api.rs'",
            [second.snapshot.id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stale_nodes, 0);
    let old_snapshot_nodes: i64 = sidecar
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM nodes WHERE snapshot_id = ?1 AND evidence_path = 'src/api.rs'",
            [first.snapshot.id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(old_snapshot_nodes > 0);
    let model_imports: i64 = sidecar
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE snapshot_id = ?1 AND kind = 're_exports' \
             AND resolution_state = 'resolved'",
            [second.snapshot.id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(model_imports, 1);

    let (_full_sidecar_dir, mut full_sidecar) = self::sidecar();
    let full_source = discover(repository_dir.path(), &config);
    let full = run(&mut full_sidecar, &full_source, &config, "build-full", true).unwrap();
    assert_eq!(full.snapshot.id, second.snapshot.id);
    assert_eq!(
        fact_identities(&sidecar, &second.snapshot.id),
        fact_identities(&full_sidecar, &full.snapshot.id)
    );
}

#[test]
fn corrupt_fragment_cache_is_treated_as_rebuildable_miss() {
    let repository_dir = tempfile::tempdir().unwrap();
    fixture_repository(repository_dir.path());
    let config = RepositoryGraphConfig::default();
    let (_sidecar_dir, mut sidecar) = sidecar();
    let first_source = discover(repository_dir.path(), &config);
    run(&mut sidecar, &first_source, &config, "build-1", false).unwrap();
    sidecar
        .connection_mut()
        .execute("UPDATE fragment_cache SET fragment_json = '{invalid'", [])
        .unwrap();

    // New discovery warnings require normal indexing even for equivalent facts.
    write(
        &repository_dir.path().join(".ferrus/project.toml"),
        b"local=true\n",
    );

    let second_source = discover(repository_dir.path(), &config);
    let second = run(&mut sidecar, &second_source, &config, "build-2", false).unwrap();
    assert_eq!(second.metrics.reused_files, 0);
    assert_eq!(second.metrics.parsed_files, 3);
    let invalid_rows: i64 = sidecar
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM fragment_cache WHERE fragment_json = '{invalid'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(invalid_rows, 0);
}
