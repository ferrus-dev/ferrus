//! Tests for snapshots.

use super::*;

#[test]
fn completed_snapshot_reuse_needs_no_fragments_and_full_still_reads_every_file() {
    let directory = tempfile::tempdir().unwrap();
    fixture_repository(directory.path());
    let config = RepositoryGraphConfig::default();
    let (_sidecar_dir, mut sidecar) = sidecar();
    let first = run(
        &mut sidecar,
        &discover(directory.path(), &config),
        &config,
        "cold",
        false,
    )
    .unwrap();
    let facts = fact_identities(&sidecar, &first.snapshot.id);
    sidecar
        .connection()
        .execute("DELETE FROM fragment_cache", [])
        .unwrap();
    let source = FailingReadSource(discover(directory.path(), &config));
    let reused = run(&mut sidecar, &source, &config, "reused", false).unwrap();
    assert_eq!(reused.snapshot, first.snapshot);
    assert_eq!(reused.metrics.reused_files, 3);
    assert_eq!(reused.metrics.processed_bytes, 0);
    assert_eq!(reused.metrics.nodes, first.metrics.nodes);
    assert_eq!(reused.metrics.edges, first.metrics.edges);
    assert_eq!(fact_identities(&sidecar, &first.snapshot.id), facts);
    assert_eq!(
        sidecar
            .connection()
            .query_row("SELECT COUNT(*) FROM fragment_cache", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        run(&mut sidecar, &source, &config, "full-failed", true).unwrap_err(),
        IndexError::SourceRead
    );
    let full = run(
        &mut sidecar,
        &discover(directory.path(), &config),
        &config,
        "full",
        true,
    )
    .unwrap();
    assert_eq!(full.metrics.parsed_files, 3);
    assert_eq!(full.metrics.reused_files, 0);
    assert_eq!(fact_identities(&sidecar, &full.snapshot.id), facts);
}

#[test]
fn analyzer_diagnostics_prevent_completed_snapshot_reuse() {
    let directory = tempfile::tempdir().unwrap();
    fixture_repository(directory.path());
    write(&directory.path().join("src/lib.rs"), b"pub fn broken( {\n");
    let config = RepositoryGraphConfig::default();
    let (_sidecar_dir, mut sidecar) = sidecar();
    let first = run(
        &mut sidecar,
        &discover(directory.path(), &config),
        &config,
        "cold",
        false,
    )
    .unwrap();
    assert!(
        sidecar
            .diagnostics_for_build(&first.build_id)
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic.code.as_str().starts_with("rust."))
    );
    sidecar
        .connection()
        .execute("DELETE FROM fragment_cache", [])
        .unwrap();
    let second = run(
        &mut sidecar,
        &discover(directory.path(), &config),
        &config,
        "retry",
        false,
    )
    .unwrap();
    assert_eq!(second.metrics.parsed_files, 3);
    assert_eq!(second.metrics.reused_files, 0);
    assert!(
        sidecar
            .diagnostics_for_build(&second.build_id)
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic.code.as_str().starts_with("rust."))
    );
}

#[test]
fn cache_hits_are_touched_only_when_snapshot_completion_commits() {
    let directory = tempfile::tempdir().unwrap();
    fixture_repository(directory.path());
    let config = RepositoryGraphConfig::default();
    let (_sidecar_dir, mut sidecar) = sidecar();
    run(
        &mut sidecar,
        &discover(directory.path(), &config),
        &config,
        "cold",
        false,
    )
    .unwrap();
    sidecar
        .connection()
        .execute("UPDATE fragment_cache SET last_used_at = 'old'", [])
        .unwrap();

    // A changed manifest exercises fragment reuse rather than snapshot reuse.
    write(&directory.path().join("README.md"), b"# Added\n");
    let source = CacheObservedSource {
        inner: discover(directory.path(), &config),
        sidecar_path: sidecar.path().to_path_buf(),
        revalidations: Cell::new(0),
    };
    let outcome = run(&mut sidecar, &source, &config, "cached", false).unwrap();
    assert_eq!(outcome.metrics.reused_files, 3);
    assert_eq!(source.revalidations.get(), 2);
    let (distinct, unchanged): (i64, i64) = sidecar
        .connection()
        .query_row(
            "SELECT COUNT(DISTINCT last_used_at), SUM(last_used_at = 'old') FROM fragment_cache \
             WHERE path != 'README.md'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((distinct, unchanged), (1, 0));
}

#[test]
fn equivalent_snapshot_refreshes_diagnostics_without_discarding_build_history() {
    let repository_dir = tempfile::tempdir().unwrap();
    fixture_repository(repository_dir.path());
    let config = RepositoryGraphConfig::default();
    let (_sidecar_dir, mut sidecar) = sidecar();

    let first_source = discover(repository_dir.path(), &config);
    let first = run(&mut sidecar, &first_source, &config, "build-1", false).unwrap();
    let first_warning_count = status(&sidecar, &config).diagnostics.summary.warning;

    write(
        &repository_dir.path().join(".ferrus/project.toml"),
        b"project_id='local-only'\n",
    );
    let second_source = discover(repository_dir.path(), &config);
    let second = run(&mut sidecar, &second_source, &config, "build-2", false).unwrap();
    assert_eq!(second.snapshot.id, first.snapshot.id);
    assert!(second.reused_existing_snapshot);
    assert_eq!(
        status(&sidecar, &config).diagnostics.summary.warning,
        first_warning_count + 1
    );
    assert!(
        sidecar
            .diagnostics_for_build(&BuildId::new("build-2").unwrap())
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic.code.as_str() == "runtime_path_excluded")
    );

    fs::remove_dir_all(repository_dir.path().join(".ferrus")).unwrap();
    let third_source = discover(repository_dir.path(), &config);
    let third = run(&mut sidecar, &third_source, &config, "build-3", false).unwrap();
    assert_eq!(third.snapshot.id, first.snapshot.id);
    assert_eq!(
        status(&sidecar, &config).diagnostics.summary.warning,
        first_warning_count
    );
    assert!(
        sidecar
            .diagnostics_for_build(&BuildId::new("build-2").unwrap())
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic.code.as_str() == "runtime_path_excluded")
    );
}

#[test]
fn snapshot_collected_during_revalidation_is_not_recreated_without_facts() {
    let directory = tempfile::tempdir().unwrap();
    fixture_repository(directory.path());
    let config = RepositoryGraphConfig::default();
    let (_sidecar_dir, mut sidecar) = sidecar();
    let first = run(
        &mut sidecar,
        &discover(directory.path(), &config),
        &config,
        "cold",
        false,
    )
    .unwrap();
    sidecar
        .connection()
        .execute("DELETE FROM published_views", [])
        .unwrap();
    let source = CollectedSnapshotSource {
        inner: discover(directory.path(), &config),
        sidecar_path: sidecar.path().to_path_buf(),
    };
    assert_eq!(
        run(&mut sidecar, &source, &config, "collected", false).unwrap_err(),
        IndexError::Commit
    );
    assert!(sidecar.snapshot(&first.snapshot.id).unwrap().is_none());
    assert_eq!(
        sidecar
            .build(&BuildId::new("collected").unwrap())
            .unwrap()
            .unwrap()
            .state,
        BuildState::Failed
    );
}
