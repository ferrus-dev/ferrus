use std::{
    cell::Cell,
    fs,
    path::{Path, PathBuf},
};

use super::*;
use crate::repository_graph::{
    config::{AnalyzerSettings, ConfigScalar},
    domain::{
        Digest, RepoPath, RepositoryId, RepositoryNamespace, RepositoryRef, SnapshotId,
        SourceRevisionId,
    },
    ports::{GraphQuery, RepositorySource, SourceContent},
    query::{QueryScope, SnapshotSelector, StatusRequest, StatusResponse},
    query_sqlite::{SqliteGraphQuery, default_budget},
    source::{FilesystemRepositorySource, SourceDiscoveryContext},
    sqlite::{OpenSidecarResult, Sidecar, open_for_build_at},
};

fn repository() -> RepositoryRef {
    RepositoryRef {
        namespace: RepositoryNamespace::new("local:test").unwrap(),
        repository_id: RepositoryId::new("incremental").unwrap(),
    }
}

fn write(path: &Path, contents: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn fixture_repository(root: &Path) {
    write(
        &root.join("Cargo.toml"),
        b"[package]\nname='fixture'\nversion='0.1.0'\n",
    );
    write(
        &root.join("src/lib.rs"),
        b"mod api;\npub use crate::api::Api;\n",
    );
    write(&root.join("src/api.rs"), b"pub struct Api;\n");
}

fn discover(root: &Path, config: &RepositoryGraphConfig) -> FilesystemRepositorySource {
    let identities = active_extractor_identities(config).unwrap();
    let context = SourceDiscoveryContext::from_config(repository(), config, &identities).unwrap();
    FilesystemRepositorySource::discover(root, context).unwrap()
}

fn sidecar() -> (tempfile::TempDir, Sidecar) {
    let directory = tempfile::tempdir().unwrap();
    let OpenSidecarResult::Ready(sidecar) =
        open_for_build_at(&directory.path().join("repo-graph.db")).unwrap()
    else {
        panic!("new sidecar unexpectedly requires rebuild");
    };
    (directory, sidecar)
}

fn run(
    sidecar: &mut Sidecar,
    source: &dyn RepositorySource<Error = SourceError>,
    config: &RepositoryGraphConfig,
    build: &str,
    force_full: bool,
) -> Result<IndexOutcome, IndexError> {
    IndexCoordinator::new(sidecar).index(
        source,
        config,
        IndexRequest {
            build_id: BuildId::new(build).unwrap(),
            view_name: PublishedViewName::new("canonical").unwrap(),
            force_full,
        },
    )
}

fn status(sidecar: &Sidecar, config: &RepositoryGraphConfig) -> StatusResponse {
    let query = SqliteGraphQuery::new(sidecar, config.query_limits.clone(), None);
    query
        .status(&StatusRequest {
            scope: QueryScope::current(
                repository(),
                SnapshotSelector::Published(PublishedViewName::new("canonical").unwrap()),
                default_budget(&config.query_limits).unwrap(),
            ),
        })
        .unwrap()
}

type NodeIdentity = (String, String);
type EdgeIdentity = (String, String, String, Option<String>, Option<String>);

fn fact_identities(
    sidecar: &Sidecar,
    snapshot: &SnapshotId,
) -> (Vec<NodeIdentity>, Vec<EdgeIdentity>) {
    let mut nodes = sidecar
        .connection()
        .prepare("SELECT id, kind FROM nodes WHERE snapshot_id = ?1 ORDER BY id")
        .unwrap();
    let nodes = nodes
        .query_map([snapshot.as_str()], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let mut edges = sidecar
        .connection()
        .prepare(
            "SELECT id, kind, source_node_id, target_node_id, external_target \
             FROM edges WHERE snapshot_id = ?1 ORDER BY id",
        )
        .unwrap();
    let edges = edges
        .query_map([snapshot.as_str()], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    (nodes, edges)
}

#[test]
fn cold_build_persists_complete_graph_and_no_op_reuses_every_file() {
    let repository_dir = tempfile::tempdir().unwrap();
    fixture_repository(repository_dir.path());
    let config = RepositoryGraphConfig::default();
    let first_source = discover(repository_dir.path(), &config);
    let (_sidecar_dir, mut sidecar) = sidecar();

    let first = run(&mut sidecar, &first_source, &config, "build-1", false).unwrap();
    assert_eq!(first.metrics.discovered_files, 3);
    assert_eq!(first.metrics.parsed_files, 3);
    assert_eq!(first.metrics.reused_files, 0);
    assert!(first.metrics.nodes > 0);
    assert!(first.metrics.edges > 0);
    assert_eq!(
        sidecar.snapshot_fact_counts(&first.snapshot.id).unwrap(),
        (
            first.metrics.discovered_files,
            first.metrics.nodes,
            first.metrics.edges
        )
    );
    let first_view = sidecar
        .published_view(&repository(), &PublishedViewName::new("canonical").unwrap())
        .unwrap()
        .unwrap();

    let second_source = discover(repository_dir.path(), &config);
    let second = run(&mut sidecar, &second_source, &config, "build-2", false).unwrap();
    assert_eq!(second.snapshot.id, first.snapshot.id);
    assert!(second.reused_existing_snapshot);
    assert_eq!(second.metrics.reused_files, 3);
    assert_eq!(second.metrics.parsed_files, 0);
    assert_eq!(second.metrics.processed_bytes, 0);
    assert_eq!(
        sidecar
            .build(&BuildId::new("build-2").unwrap())
            .unwrap()
            .unwrap()
            .state,
        BuildState::Superseded
    );
    let second_view = sidecar
        .published_view(&repository(), &PublishedViewName::new("canonical").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(second_view, first_view);
    assert_eq!(
        sidecar
            .index_build_metrics(&BuildId::new("build-2").unwrap())
            .unwrap(),
        Some(second.metrics)
    );
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
fn analysis_config_change_invalidates_all_cached_fragments() {
    let repository_dir = tempfile::tempdir().unwrap();
    fixture_repository(repository_dir.path());
    let first_config = RepositoryGraphConfig::default();
    let (_sidecar_dir, mut sidecar) = sidecar();
    let first_source = discover(repository_dir.path(), &first_config);
    run(&mut sidecar, &first_source, &first_config, "build-1", false).unwrap();

    let mut second_config = first_config.clone();
    second_config.analyzers.settings.insert(
        "builtin.rust-syntax".to_string(),
        AnalyzerSettings {
            options: BTreeMap::from([("fixture_mode".to_string(), ConfigScalar::Boolean(true))]),
        },
    );
    let second_source = discover(repository_dir.path(), &second_config);
    let second = run(
        &mut sidecar,
        &second_source,
        &second_config,
        "build-2",
        false,
    )
    .unwrap();
    assert_eq!(second.metrics.reused_files, 0);
    assert_eq!(second.metrics.parsed_files, 3);
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
            .snapshot(&snapshot_identity(source.manifest()))
            .unwrap()
            .is_some()
    );
    assert!(sidecar.snapshot(&first.snapshot.id).unwrap().is_some());
}

struct SequencedSource {
    inner: FilesystemRepositorySource,
    revalidations: Cell<u32>,
}

struct PublishingRaceSource {
    inner: FilesystemRepositorySource,
    sidecar_path: PathBuf,
    revalidations: Cell<u32>,
}

struct FailingReadSource(FilesystemRepositorySource);

impl RepositorySource for FailingReadSource {
    type Error = SourceError;

    fn repository(&self) -> &RepositoryRef {
        self.0.repository()
    }

    fn manifest(&self) -> &SourceManifest {
        self.0.manifest()
    }

    fn read_verified(&self, _file: &SourceFileDescriptor) -> Result<SourceContent, Self::Error> {
        Err(SourceError::ContentChanged)
    }

    fn revalidate(&self) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

impl RepositorySource for SequencedSource {
    type Error = SourceError;

    fn repository(&self) -> &RepositoryRef {
        self.inner.repository()
    }

    fn manifest(&self) -> &SourceManifest {
        self.inner.manifest()
    }

    fn read_verified(&self, file: &SourceFileDescriptor) -> Result<SourceContent, Self::Error> {
        self.inner.read_verified(file)
    }

    fn revalidate(&self) -> Result<bool, Self::Error> {
        let call = self.revalidations.get();
        self.revalidations.set(call + 1);
        Ok(call == 0)
    }
}

impl RepositorySource for PublishingRaceSource {
    type Error = SourceError;

    fn repository(&self) -> &RepositoryRef {
        self.inner.repository()
    }

    fn manifest(&self) -> &SourceManifest {
        self.inner.manifest()
    }

    fn read_verified(&self, file: &SourceFileDescriptor) -> Result<SourceContent, Self::Error> {
        self.inner.read_verified(file)
    }

    fn revalidate(&self) -> Result<bool, Self::Error> {
        let call = self.revalidations.get();
        self.revalidations.set(call + 1);
        if call == 1 {
            let OpenSidecarResult::Ready(mut sidecar) =
                open_for_build_at(&self.sidecar_path).unwrap()
            else {
                panic!("current sidecar unexpectedly requires rebuild");
            };
            let winner = GraphBuild {
                id: BuildId::new("build-winner").unwrap(),
                repository: self.inner.manifest().revision.repository.clone(),
                source_revision_id: SourceRevisionId::new("revision-winner").unwrap(),
                prospective_snapshot_id: SnapshotId::new("snapshot:winner").unwrap(),
                state: BuildState::Building,
            };
            sidecar.start_build(&winner).unwrap();
            sidecar
                .complete_build(&GraphSnapshot {
                    id: winner.prospective_snapshot_id.clone(),
                    repository: winner.repository.clone(),
                    source_revision_id: winner.source_revision_id.clone(),
                    source_manifest_digest: Digest::new("sha256", "00").unwrap(),
                    graph_model_version: GRAPH_MODEL_VERSION,
                    analysis_config_digest: self
                        .inner
                        .manifest()
                        .revision
                        .analysis_config_digest
                        .clone(),
                    extractor_set_digest: self.inner.manifest().extractor_set_digest.clone(),
                    completed_by: winner.id.clone(),
                })
                .unwrap();
            sidecar
                .publish(&PublishRequest {
                    repository: winner.repository,
                    view_name: PublishedViewName::new("canonical").unwrap(),
                    build_id: winner.id,
                    expected: None,
                })
                .unwrap();
        }
        Ok(true)
    }
}
