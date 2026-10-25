//! Tests for publication.

use super::*;

#[test]
fn snapshot_reuse_revalidates_before_completion_and_before_publication() {
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
    for (initial_calls, build, state) in [
        (1, "before-completion", BuildState::Failed),
        (0, "before-publication", BuildState::Superseded),
    ] {
        let source = SequencedSource {
            inner: discover(directory.path(), &config),
            revalidations: Cell::new(initial_calls),
        };
        assert_eq!(
            run(&mut sidecar, &source, &config, build, false).unwrap_err(),
            IndexError::SourceChanged
        );
        assert_eq!(
            sidecar
                .build(&BuildId::new(build).unwrap())
                .unwrap()
                .unwrap()
                .state,
            state
        );
        assert_eq!(
            sidecar
                .published_view(&repository(), &PublishedViewName::new("canonical").unwrap())
                .unwrap()
                .unwrap()
                .snapshot_id,
            first.snapshot.id
        );
        assert_eq!(source.revalidations.get(), 2);
    }
}

#[test]
fn overlapping_publication_supersedes_the_completed_loser() {
    let repository_dir = tempfile::tempdir().unwrap();
    fixture_repository(repository_dir.path());
    let config = RepositoryGraphConfig::default();
    let (_sidecar_dir, mut sidecar) = sidecar();
    let source = PublishingRaceSource {
        inner: discover(repository_dir.path(), &config),
        sidecar_path: sidecar.path().to_path_buf(),
        revalidations: Cell::new(0),
    };

    let outcome = run(&mut sidecar, &source, &config, "build-loser", false).unwrap();
    assert!(matches!(
        outcome.publication,
        PublicationOutcome::Superseded { ref current }
            if current.build_id.as_str() == "build-winner"
    ));
    assert_eq!(
        sidecar
            .build(&BuildId::new("build-loser").unwrap())
            .unwrap()
            .unwrap()
            .state,
        BuildState::Superseded
    );
    assert_eq!(
        sidecar
            .published_view(&repository(), &PublishedViewName::new("canonical").unwrap())
            .unwrap()
            .unwrap()
            .build_id
            .as_str(),
        "build-winner"
    );
}

#[test]
fn completed_snapshot_reuse_cannot_overwrite_a_concurrent_publication() {
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
    let source = PublishingRaceSource {
        inner: discover(directory.path(), &config),
        sidecar_path: sidecar.path().to_path_buf(),
        revalidations: Cell::new(0),
    };
    let outcome = run(&mut sidecar, &source, &config, "reuse-loser", false).unwrap();
    assert!(outcome.reused_existing_snapshot);
    assert_eq!(outcome.metrics.parsed_files, 0);
    assert!(matches!(outcome.publication,
        PublicationOutcome::Superseded { ref current } if current.build_id.as_str() == "build-winner"));
    assert_eq!(
        sidecar.build(&outcome.build_id).unwrap().unwrap().state,
        BuildState::Superseded
    );
}

#[test]
fn skipped_path_diagnostics_publish_without_file_rows() {
    let repository_dir = tempfile::tempdir().unwrap();
    fixture_repository(repository_dir.path());
    write(
        &repository_dir.path().join(".ferrus/project.toml"),
        b"project_id='local-only'\n",
    );
    let config = RepositoryGraphConfig::default();
    let source = discover(repository_dir.path(), &config);
    assert!(
        source
            .manifest()
            .files
            .iter()
            .all(|file| file.path.as_str() != ".ferrus")
    );
    assert!(source.manifest().diagnostics.iter().any(|diagnostic| {
        diagnostic.code.as_str() == "runtime_path_excluded"
            && diagnostic.path.as_ref().map(RepoPath::as_str) == Some(".ferrus")
    }));

    let (_sidecar_dir, mut sidecar) = sidecar();
    let outcome = run(&mut sidecar, &source, &config, "build-skipped", false).unwrap();
    let persisted_path: Option<String> = sidecar
        .connection()
        .query_row(
            "SELECT path FROM diagnostics WHERE snapshot_id = ?1 \
             AND code = 'runtime_path_excluded'",
            [outcome.snapshot.id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(persisted_path.as_deref(), Some(".ferrus"));
    let omitted_file_rows: i64 = sidecar
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM files WHERE snapshot_id = ?1 AND path = '.ferrus'",
            [outcome.snapshot.id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(omitted_file_rows, 0);
    assert_eq!(
        sidecar
            .published_snapshot(&repository(), &PublishedViewName::new("canonical").unwrap())
            .unwrap()
            .unwrap()
            .id,
        outcome.snapshot.id
    );
}

#[test]
fn verified_read_failure_keeps_previous_publication() {
    let repository_dir = tempfile::tempdir().unwrap();
    fixture_repository(repository_dir.path());
    let config = RepositoryGraphConfig::default();
    let (_sidecar_dir, mut sidecar) = sidecar();
    let first_source = discover(repository_dir.path(), &config);
    run(&mut sidecar, &first_source, &config, "build-1", false).unwrap();
    let original_view = sidecar
        .published_view(&repository(), &PublishedViewName::new("canonical").unwrap())
        .unwrap()
        .unwrap();

    write(
        &repository_dir.path().join("src/lib.rs"),
        b"pub struct Changed;\n",
    );
    let failing = FailingReadSource(discover(repository_dir.path(), &config));
    let error = run(&mut sidecar, &failing, &config, "build-2", false).unwrap_err();
    assert_eq!(error, IndexError::SourceRead);
    assert_eq!(
        sidecar
            .build(&BuildId::new("build-2").unwrap())
            .unwrap()
            .unwrap()
            .state,
        BuildState::Failed
    );
    assert_eq!(
        sidecar
            .published_view(&repository(), &PublishedViewName::new("canonical").unwrap())
            .unwrap(),
        Some(original_view)
    );
    assert_eq!(
        sidecar
            .index_build_metrics(&BuildId::new("build-2").unwrap())
            .unwrap()
            .unwrap()
            .failed_files,
        1
    );
}

#[test]
fn source_change_after_commit_never_replaces_published_snapshot() {
    let repository_dir = tempfile::tempdir().unwrap();
    fixture_repository(repository_dir.path());
    let config = RepositoryGraphConfig::default();
    let (_sidecar_dir, mut sidecar) = sidecar();
    let first_source = discover(repository_dir.path(), &config);
    let first = run(&mut sidecar, &first_source, &config, "build-1", false).unwrap();
    let original_view = sidecar
        .published_view(&repository(), &PublishedViewName::new("canonical").unwrap())
        .unwrap()
        .unwrap();

    write(
        &repository_dir.path().join("src/lib.rs"),
        b"pub struct Changed;\n",
    );
    let source = SequencedSource {
        inner: discover(repository_dir.path(), &config),
        revalidations: Cell::new(0),
    };
    let error = run(&mut sidecar, &source, &config, "build-2", false).unwrap_err();
    assert_eq!(error, IndexError::SourceChanged);
    assert_eq!(
        sidecar
            .published_view(&repository(), &PublishedViewName::new("canonical").unwrap())
            .unwrap(),
        Some(original_view)
    );
    assert_eq!(
        sidecar
            .build(&BuildId::new("build-2").unwrap())
            .unwrap()
            .unwrap()
            .state,
        BuildState::Superseded
    );
    assert!(
        sidecar
            .snapshot(&snapshot_identity(source.manifest()).unwrap())
            .unwrap()
            .is_some()
    );
    assert!(sidecar.snapshot(&first.snapshot.id).unwrap().is_some());
}
