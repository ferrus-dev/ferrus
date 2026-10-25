//! Graph indexing tests for incremental reuse, source changes, and publication consistency.

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

struct CacheObservedSource {
    inner: FilesystemRepositorySource,
    sidecar_path: PathBuf,
    revalidations: Cell<u32>,
}

impl RepositorySource for CacheObservedSource {
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
        let connection = rusqlite::Connection::open(&self.sidecar_path).unwrap();
        connection.busy_timeout(std::time::Duration::ZERO).unwrap();
        connection
            .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
            .unwrap();
        let touched: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM fragment_cache WHERE last_used_at != 'old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        if self.revalidations.get() == 0 {
            assert_eq!(touched, 0);
        } else {
            assert!(touched > 0);
        }

        self.revalidations.set(self.revalidations.get() + 1);
        self.inner.revalidate()
    }
}

struct SequencedSource {
    inner: FilesystemRepositorySource,
    revalidations: Cell<u32>,
}

struct CollectedSnapshotSource {
    inner: FilesystemRepositorySource,
    sidecar_path: PathBuf,
}

impl RepositorySource for CollectedSnapshotSource {
    type Error = SourceError;

    fn repository(&self) -> &RepositoryRef {
        self.inner.repository()
    }

    fn manifest(&self) -> &SourceManifest {
        self.inner.manifest()
    }

    fn read_verified(&self, _file: &SourceFileDescriptor) -> Result<SourceContent, Self::Error> {
        panic!("a reusable snapshot must not read individual source files");
    }

    fn revalidate(&self) -> Result<bool, Self::Error> {
        let OpenSidecarResult::Ready(sidecar) = open_for_build_at(&self.sidecar_path).unwrap()
        else {
            panic!("sidecar is current");
        };
        sidecar
            .connection()
            .execute(
                "DELETE FROM snapshots WHERE id = ?1",
                [snapshot_identity(self.inner.manifest()).unwrap().as_str()],
            )
            .unwrap();
        self.inner.revalidate()
    }
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
            let expected = sidecar
                .published_view(
                    &winner.repository,
                    &PublishedViewName::new("canonical").unwrap(),
                )
                .unwrap()
                .map(|view| PublicationVersion {
                    snapshot_id: view.snapshot_id,
                    generation: view.generation,
                });
            sidecar
                .publish(&PublishRequest {
                    repository: winner.repository,
                    view_name: PublishedViewName::new("canonical").unwrap(),
                    build_id: winner.id,
                    expected,
                })
                .unwrap();
        }

        Ok(true)
    }
}

#[path = "index_tests/snapshots.rs"]
mod snapshots_tests;

#[path = "index_tests/publication.rs"]
mod publication_tests;

#[path = "index_tests/behavior.rs"]
mod behavior_tests;

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
fn failed_builds_do_not_leave_partial_cache_touches() {
    let directory = tempfile::tempdir().unwrap();
    fixture_repository(directory.path());
    let config = RepositoryGraphConfig::default();
    let (_sidecar_dir, mut sidecar) = sidecar();
    let initial = run(
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
    write(&directory.path().join("README.md"), b"# Added\n");
    let changed = SequencedSource {
        inner: discover(directory.path(), &config),
        revalidations: Cell::new(1),
    };
    assert_eq!(
        run(&mut sidecar, &changed, &config, "source-changed", false).unwrap_err(),
        IndexError::SourceChanged
    );
    let touched = || {
        sidecar
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM fragment_cache WHERE last_used_at != 'old'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
    };
    assert_eq!(touched(), 0);
    // Abort a touch after the new fragment write to detect partial transactions.
    sidecar
        .connection()
        .execute_batch(
            "CREATE TRIGGER fail_cache_touch BEFORE UPDATE OF last_used_at ON fragment_cache \
         WHEN EXISTS (SELECT 1 FROM fragment_cache WHERE last_used_at != 'old') \
         BEGIN SELECT RAISE(ABORT, 'injected cache touch failure'); END;",
        )
        .unwrap();
    assert_eq!(
        run(
            &mut sidecar,
            &discover(directory.path(), &config),
            &config,
            "commit-failed",
            false
        )
        .unwrap_err(),
        IndexError::Commit
    );
    let touched: i64 = sidecar
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM fragment_cache WHERE last_used_at != 'old'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(touched, 0);
    assert_eq!(
        sidecar
            .published_view(&repository(), &PublishedViewName::new("canonical").unwrap())
            .unwrap()
            .unwrap()
            .snapshot_id,
        initial.snapshot.id
    );
    sidecar
        .connection()
        .execute_batch("DROP TRIGGER fail_cache_touch")
        .unwrap();
    let retry = run(
        &mut sidecar,
        &discover(directory.path(), &config),
        &config,
        "retry",
        false,
    )
    .unwrap();
    assert_eq!(retry.metrics.reused_files, 3);
}
