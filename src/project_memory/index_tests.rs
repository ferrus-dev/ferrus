use std::{fs, path::Path, process::Command};

use rusqlite::{Connection, params};
use tempfile::TempDir;

use crate::{
    project_memory::{
        domain::{
            MemoryEntityData, MemoryRelationshipTarget, MemoryResolutionState, ProjectId,
            ProjectNamespace, ProjectRef,
        },
        policy::MemoryPolicy,
        ports::MemoryLinkStore,
        source::LocalMemorySource,
        sqlite::MemorySidecar,
    },
    repository_graph::{
        domain::{RepoPath, RepositoryId, RepositoryNamespace, RepositoryRef, SnapshotId},
        sqlite::{OpenSidecarResult, SIDECAR_FILE_NAME, open_for_build_at},
    },
};

use super::*;

fn project() -> ProjectRef {
    ProjectRef {
        namespace: ProjectNamespace::new("local:test").unwrap(),
        project_id: ProjectId::new("project-1").unwrap(),
    }
}

fn graph_repository() -> RepositoryRef {
    RepositoryRef {
        namespace: RepositoryNamespace::new("local:project-1").unwrap(),
        repository_id: RepositoryId::new("root").unwrap(),
    }
}

fn insert_graph_snapshot(
    data: &TempDir,
    snapshot_id: &str,
    identity: &str,
    files: &[&str],
    symbols: &[(&str, &str)],
    publish: bool,
) {
    let path = data.path().join(SIDECAR_FILE_NAME);
    let mut sidecar = match open_for_build_at(&path).unwrap() {
        OpenSidecarResult::Ready(sidecar) => sidecar,
        OpenSidecarResult::RequiresRebuild(_) => panic!("test graph sidecar requires rebuild"),
    };
    let repository = graph_repository();
    let build_id = format!("build-{snapshot_id}");
    sidecar
        .connection_mut()
        .execute(
            "INSERT INTO index_builds( \
                    id, repository_namespace, repository_id, source_revision_id, \
                    prospective_snapshot_id, state, started_at, finished_at \
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'published', ?6, ?6)",
            params![
                build_id,
                repository.namespace.as_str(),
                repository.repository_id.as_str(),
                format!("source-{snapshot_id}"),
                snapshot_id,
                "2026-08-03T00:00:00Z",
            ],
        )
        .unwrap();
    sidecar
        .connection_mut()
        .execute(
            "INSERT INTO snapshots( \
                    id, repository_namespace, repository_id, source_revision_id, \
                    source_manifest_algorithm, source_manifest_digest, graph_model_version, \
                    analysis_config_algorithm, analysis_config_digest, extractor_set_algorithm, \
                    extractor_set_digest, completed_by_build_id, created_at \
                 ) VALUES (?1, ?2, ?3, ?4, 'sha256', ?5, 1, 'sha256', ?5, 'sha256', ?5, ?6, ?7)",
            params![
                snapshot_id,
                repository.namespace.as_str(),
                repository.repository_id.as_str(),
                format!("source-{snapshot_id}"),
                identity,
                build_id,
                "2026-08-03T00:00:00Z",
            ],
        )
        .unwrap();
    for (index, file) in files.iter().enumerate() {
        sidecar
            .connection_mut()
            .execute(
                "INSERT INTO files( \
                        snapshot_id, path, content_algorithm, content_digest, byte_length \
                     ) VALUES (?1, ?2, 'sha256', ?3, 1)",
                params![snapshot_id, file, format!("{identity}{index:02x}")],
            )
            .unwrap();
    }
    for (index, (semantic_key, path)) in symbols.iter().enumerate() {
        sidecar
            .connection_mut()
            .execute(
                "INSERT INTO nodes( \
                        snapshot_id, id, kind, semantic_key, extractor_id, extractor_version, \
                        extractor_contract_version, resolution_state, confidence, evidence_path, \
                        evidence_content_algorithm, evidence_content_digest, properties_json \
                     ) VALUES (?1, ?2, 'function', ?3, 'test', '1', 1, 'resolved', 'exact', \
                        ?4, 'sha256', ?5, '{}')",
                params![
                    snapshot_id,
                    format!("node-{snapshot_id}-{index}"),
                    semantic_key,
                    path,
                    format!("{identity}{index:02x}"),
                ],
            )
            .unwrap();
    }
    if publish {
        sidecar
            .connection_mut()
            .execute(
                "INSERT INTO published_views( \
                        repository_namespace, repository_id, view_name, snapshot_id, build_id, \
                        generation, published_at \
                     ) VALUES (?1, ?2, 'canonical', ?3, ?4, 1, ?5) \
                     ON CONFLICT(repository_namespace, repository_id, view_name) DO UPDATE SET \
                        snapshot_id = excluded.snapshot_id, build_id = excluded.build_id, \
                        generation = published_views.generation + 1, \
                        published_at = excluded.published_at",
                params![
                    repository.namespace.as_str(),
                    repository.repository_id.as_str(),
                    snapshot_id,
                    build_id,
                    "2026-08-03T00:00:00Z",
                ],
            )
            .unwrap();
    }
}

fn init(root: &Path) {
    assert!(
        Command::new("git")
            .arg("init")
            .arg(root)
            .status()
            .unwrap()
            .success()
    );
    fs::create_dir_all(root.join("docs/specs")).unwrap();
}

fn write_spec(root: &Path, outcome: Option<&str>) {
    let outcome = outcome
        .map(|outcome| format!("\n## Outcome\n\n{outcome}\n\n### Decisions\n\nUse SQLite.\n"))
        .unwrap_or_default();
    fs::write(
        root.join("docs/specs/example.md"),
        format!("# Example\n\n- [x] #1.0 Done\n\nID: one\nDepends on: none\n{outcome}"),
    )
    .unwrap();
    assert!(
        Command::new("git")
            .current_dir(root)
            .args(["add", "--", "docs/specs/example.md"])
            .status()
            .unwrap()
            .success()
    );
}

fn source(root: &TempDir, data: &TempDir) -> LocalMemorySource {
    let spec_path = "docs/specs/example.md";
    if let Ok(spec_content) = fs::read_to_string(root.path().join(spec_path)) {
        crate::project_memory::source::record_approved_outcome_for_test(
            data.path(),
            spec_path,
            &spec_content,
        );
    }
    LocalMemorySource::discover_at(
        root.path().to_path_buf(),
        data.path().to_path_buf(),
        project(),
        RepoPath::new("docs/specs").unwrap(),
        MemoryPolicy::default(),
    )
    .unwrap()
}

fn normalize_index_timestamps(
    entities: &mut [MemoryEntity],
    relationships: &mut [MemoryRelationship],
) {
    let epoch = chrono::DateTime::<Utc>::from(std::time::UNIX_EPOCH);
    for entity in entities {
        entity.provenance.timestamps.source_observed_at = epoch;
        entity.provenance.timestamps.indexed_at = epoch;
    }
    for relationship in relationships {
        relationship.provenance.timestamps.source_observed_at = epoch;
        relationship.provenance.timestamps.indexed_at = epoch;
    }
}

#[test]
fn equivalent_authorized_inputs_produce_equivalent_records() {
    let root = TempDir::new().unwrap();
    let first_data = TempDir::new().unwrap();
    let second_data = TempDir::new().unwrap();
    init(root.path());
    write_spec(root.path(), Some("Delivered deterministically."));
    let first_source = source(&root, &first_data);
    let second_source = source(&root, &second_data);
    let mut first_store = MemorySidecar::open_at(first_data.path()).unwrap();
    let mut second_store = MemorySidecar::open_at(second_data.path()).unwrap();
    let first = MemoryIndexer::new(&first_source, &mut first_store)
        .unwrap()
        .index(MemoryIndexOptions::default())
        .unwrap();
    let second = MemoryIndexer::new(&second_source, &mut second_store)
        .unwrap()
        .index(MemoryIndexOptions::default())
        .unwrap();
    assert_eq!(first.revision.id, second.revision.id);

    let mut first_entities = first_store
        .entities_for_revision(&first.revision.id)
        .unwrap();
    let mut second_entities = second_store
        .entities_for_revision(&second.revision.id)
        .unwrap();
    let mut first_relationships = first_store
        .relationships_for_revision(&first.revision.id)
        .unwrap();
    let mut second_relationships = second_store
        .relationships_for_revision(&second.revision.id)
        .unwrap();
    normalize_index_timestamps(&mut first_entities, &mut first_relationships);
    normalize_index_timestamps(&mut second_entities, &mut second_relationships);
    assert_eq!(first_entities, second_entities);
    assert_eq!(first_relationships, second_relationships);
}

#[test]
fn full_rebuild_removes_an_incompatible_sidecar_file_set() {
    let data = TempDir::new().unwrap();
    let path = data
        .path()
        .join(super::super::sqlite::MEMORY_SIDECAR_FILE_NAME);
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch("PRAGMA application_id = 1; PRAGMA user_version = 999;")
        .unwrap();
    drop(connection);
    fs::write(path.with_extension("db-wal"), "stale wal").unwrap();
    fs::write(path.with_extension("db-shm"), "stale shm").unwrap();
    assert!(matches!(
        MemorySidecar::open_at(data.path()),
        Err(MemoryStoreError::RequiresRebuild)
    ));

    remove_memory_sidecar_file_set(data.path()).unwrap();
    let rebuilt = MemorySidecar::open_at(data.path()).unwrap();
    let version: u32 = rebuilt
        .connection()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, super::super::sqlite::MEMORY_SIDECAR_SCHEMA_VERSION);
}

#[test]
fn cold_no_op_incremental_and_deletion_builds_publish_atomically() {
    let root = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    init(root.path());
    write_spec(root.path(), Some("Delivered."));
    let first_source = source(&root, &data);
    let mut store = MemorySidecar::open_at(data.path()).unwrap();
    let first = MemoryIndexer::new(&first_source, &mut store)
        .unwrap()
        .index(MemoryIndexOptions::default())
        .unwrap();
    assert_eq!(first.metrics.extracted_sources, 3);
    assert_eq!(first.metrics.reused_sources, 0);
    let first_view = published_revision(&store, &first_source).unwrap().unwrap();
    assert_eq!(first_view.generation, 1);
    let first_entities = store.entities_for_revision(&first.revision.id).unwrap();
    assert!(
        first_entities
            .iter()
            .any(|entity| { matches!(entity.data, MemoryEntityData::Specification { .. }) })
    );
    assert!(
        first_entities
            .iter()
            .any(|entity| matches!(entity.data, MemoryEntityData::Outcome { .. }))
    );

    let no_op = MemoryIndexer::new(&first_source, &mut store)
        .unwrap()
        .index(MemoryIndexOptions::default())
        .unwrap();
    assert_eq!(no_op.revision.id, first.revision.id);
    assert_eq!(no_op.metrics.reused_sources, 3);
    assert_eq!(
        published_revision(&store, &first_source)
            .unwrap()
            .unwrap()
            .generation,
        1
    );

    write_spec(root.path(), Some("Delivered differently."));
    let changed_source = source(&root, &data);
    let changed = MemoryIndexer::new(&changed_source, &mut store)
        .unwrap()
        .index(MemoryIndexOptions::default())
        .unwrap();
    assert_ne!(changed.revision.id, first.revision.id);
    assert_eq!(changed.metrics.reused_sources, 2);
    assert_eq!(changed.metrics.extracted_sources, 2);
    assert_eq!(
        published_revision(&store, &changed_source)
            .unwrap()
            .unwrap()
            .generation,
        2
    );

    write_spec(root.path(), None);
    let removed_source = source(&root, &data);
    let removed = MemoryIndexer::new(&removed_source, &mut store)
        .unwrap()
        .index(MemoryIndexOptions::default())
        .unwrap();
    let entities = store.entities_for_revision(&removed.revision.id).unwrap();
    assert!(
        !entities
            .iter()
            .any(|entity| matches!(entity.data, MemoryEntityData::Outcome { .. }))
    );
    let entity_ids = entities
        .iter()
        .map(|entity| entity.id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        store
            .relationships_for_revision(&removed.revision.id)
            .unwrap()
            .iter()
            .all(|relationship| {
                entity_ids.contains(&relationship.source)
                    && match &relationship.target {
                        MemoryRelationshipTarget::MemoryEntity { entity_id } => {
                            entity_ids.contains(entity_id)
                        }
                        _ => true,
                    }
            })
    );
    assert_eq!(removed.metrics.reused_sources, 3);

    assert!(
        Command::new("git")
            .current_dir(root.path())
            .args(["rm", "-f", "--", "docs/specs/example.md"])
            .status()
            .unwrap()
            .success()
    );
    let deleted_source = source(&root, &data);
    let deleted = MemoryIndexer::new(&deleted_source, &mut store)
        .unwrap()
        .index(MemoryIndexOptions::default())
        .unwrap();
    let deleted_entities = store.entities_for_revision(&deleted.revision.id).unwrap();
    assert!(
        deleted_entities
            .iter()
            .all(|entity| matches!(entity.data, MemoryEntityData::ArchiveReference { .. }))
    );
    assert!(
        store
            .relationships_for_revision(&deleted.revision.id)
            .unwrap()
            .is_empty()
    );
    let deleted_entity_ids = deleted_entities
        .iter()
        .map(|entity| &entity.id)
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        store
            .repository_links(&deleted.repository_link_set.id)
            .unwrap()
            .iter()
            .all(|relationship| deleted_entity_ids.contains(&relationship.source))
    );
    assert_eq!(
        published_revision(&store, &deleted_source)
            .unwrap()
            .unwrap()
            .generation,
        4
    );
}

#[test]
fn source_change_during_build_keeps_the_previous_publication() {
    let root = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    init(root.path());
    write_spec(root.path(), Some("First."));
    let first_source = source(&root, &data);
    let mut store = MemorySidecar::open_at(data.path()).unwrap();
    let first = MemoryIndexer::new(&first_source, &mut store)
        .unwrap()
        .index(MemoryIndexOptions::default())
        .unwrap();
    let stale_source = source(&root, &data);
    write_spec(root.path(), Some("Changed after discovery."));
    let error = MemoryIndexer::new(&stale_source, &mut store)
        .unwrap()
        .index(MemoryIndexOptions::default())
        .unwrap_err();
    assert!(matches!(error, MemoryIndexError::Source(_)));
    assert_eq!(
        published_revision(&store, &stale_source)
            .unwrap()
            .unwrap()
            .revision_id,
        first.revision.id
    );
}

#[test]
fn repository_links_preserve_resolved_stale_and_unresolved_evidence() {
    let root = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    init(root.path());
    write_spec(
        root.path(),
        Some(
            "Touched `path:src/old.rs`, `path:src/missing.rs`, and \
                 `symbol:rust:function:src/old.rs:run`; ambiguous evidence is \
                 `symbol:rust:function:src/old.rs:ambiguous`.",
        ),
    );
    insert_graph_snapshot(
        &data,
        "snapshot-old",
        "11",
        &["docs/specs/example.md", "src/old.rs"],
        &[
            ("rust:function:src/old.rs:run", "src/old.rs"),
            ("rust:function:src/old.rs:ambiguous", "src/old.rs"),
            ("rust:function:src/old.rs:ambiguous", "src/old.rs"),
        ],
        true,
    );
    let initial_source = source(&root, &data);
    let mut store = MemorySidecar::open_at(data.path()).unwrap();
    let first = MemoryIndexer::new(&initial_source, &mut store)
        .unwrap()
        .index(MemoryIndexOptions::default())
        .unwrap();
    let first_set = store
        .latest_repository_link_set(&first.revision.id, &graph_repository())
        .unwrap()
        .unwrap();
    let first_links = store.repository_links(&first_set.id).unwrap();
    assert!(first_links.iter().any(|relationship| {
        relationship.provenance.resolution == MemoryResolutionState::Resolved
            && matches!(
                &relationship.target,
                MemoryRelationshipTarget::RepositoryPath { path, snapshot_id: Some(id), .. }
                    if path.as_str() == "src/old.rs" && id.as_str() == "snapshot-old"
            )
    }));
    assert!(first_links.iter().any(|relationship| {
        relationship.provenance.resolution == MemoryResolutionState::Unresolved
            && matches!(
                &relationship.target,
                MemoryRelationshipTarget::RepositorySymbol {
                    semantic_key,
                    snapshot_id: Some(id),
                    ..
                } if semantic_key.as_str() == "rust:function:src/old.rs:ambiguous"
                    && id.as_str() == "snapshot-old"
            )
    }));
    assert!(first_links.iter().any(|relationship| {
        relationship.provenance.resolution == MemoryResolutionState::Resolved
            && matches!(
                &relationship.target,
                MemoryRelationshipTarget::RepositoryNode {
                    snapshot_id,
                    semantic_key,
                    ..
                } if snapshot_id.as_str() == "snapshot-old"
                    && semantic_key.as_ref().is_some_and(|key| {
                        key.as_str() == "rust:function:src/old.rs:run"
                    })
            )
    }));
    assert!(first_links.iter().any(|relationship| {
        relationship.provenance.resolution == MemoryResolutionState::Unresolved
            && matches!(
                &relationship.target,
                MemoryRelationshipTarget::RepositoryPath { path, snapshot_id: None, .. }
                    if path.as_str() == "src/missing.rs"
            )
    }));

    insert_graph_snapshot(
        &data,
        "snapshot-new",
        "22",
        &["docs/specs/example.md", "src/new.rs"],
        &[("rust:function:src/new.rs:run", "src/new.rs")],
        true,
    );
    let spec_path = root.path().join("docs/specs/example.md");
    let updated = fs::read_to_string(&spec_path)
        .unwrap()
        .replace("Use SQLite.", "Use SQLite carefully.");
    fs::write(&spec_path, updated).unwrap();
    assert!(
        Command::new("git")
            .current_dir(root.path())
            .args(["add", "--", "docs/specs/example.md"])
            .status()
            .unwrap()
            .success()
    );
    let updated_source = source(&root, &data);
    let second = MemoryIndexer::new(&updated_source, &mut store)
        .unwrap()
        .index(MemoryIndexOptions::default())
        .unwrap();
    assert_ne!(second.revision.id, first.revision.id);
    assert!(second.metrics.stale_links > 0);
    let second_set = store
        .latest_repository_link_set(&second.revision.id, &graph_repository())
        .unwrap()
        .unwrap();
    assert_ne!(second_set.id, first_set.id);
    assert_eq!(
        store
            .repository_link_set_for_snapshot(
                &first.revision.id,
                &graph_repository(),
                Some(&SnapshotId::new("snapshot-old").unwrap()),
            )
            .unwrap()
            .unwrap()
            .id,
        first_set.id
    );
    let second_links = store.repository_links(&second_set.id).unwrap();
    assert!(second_links.iter().any(|relationship| {
        relationship.provenance.resolution == MemoryResolutionState::Stale
            && matches!(
                &relationship.target,
                MemoryRelationshipTarget::RepositoryPath { path, snapshot_id: Some(id), .. }
                    if path.as_str() == "src/old.rs" && id.as_str() == "snapshot-old"
            )
    }));
    assert!(second_links.iter().any(|relationship| {
        relationship.provenance.resolution == MemoryResolutionState::Stale
            && matches!(
                &relationship.target,
                MemoryRelationshipTarget::RepositoryNode {
                    snapshot_id,
                    semantic_key,
                    ..
                } if snapshot_id.as_str() == "snapshot-old"
                    && semantic_key.as_ref().is_some_and(|key| {
                        key.as_str() == "rust:function:src/old.rs:run"
                    })
            )
    }));
}

#[test]
fn task_changed_paths_link_task_and_milestone_to_current_snapshot() {
    let root = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    init(root.path());
    write_spec(root.path(), None);
    insert_graph_snapshot(
        &data,
        "snapshot-baseline",
        "31",
        &["docs/specs/example.md", "src/lib.rs"],
        &[],
        false,
    );
    insert_graph_snapshot(
        &data,
        "snapshot-task",
        "32",
        &["docs/specs/example.md", "src/lib.rs"],
        &[],
        false,
    );
    insert_graph_snapshot(
        &data,
        "snapshot-current",
        "33",
        &["docs/specs/example.md", "src/lib.rs"],
        &[],
        true,
    );
    let connection = Connection::open(data.path().join("ferrus.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE tasks( \
                    id TEXT, milestone_id TEXT, status TEXT, spec_path TEXT, \
                    baseline_snapshot_id TEXT, repository_view_snapshot_id TEXT \
                 ); \
                 CREATE TABLE runs( \
                    id TEXT, task_id TEXT, status TEXT, baseline_snapshot_id TEXT, \
                    repository_view_snapshot_id TEXT \
                 ); \
                 CREATE TABLE events(id INTEGER, run_id TEXT, type TEXT, payload_json TEXT);",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO tasks VALUES ( \
                    't-1', 'one', 'complete', 'docs/specs/example.md', \
                    'snapshot-baseline', 'snapshot-task' \
                 )",
            [],
        )
        .unwrap();
    let source = source(&root, &data);
    let mut store = MemorySidecar::open_at(data.path()).unwrap();
    let outcome = MemoryIndexer::new(&source, &mut store)
        .unwrap()
        .index(MemoryIndexOptions::default())
        .unwrap();
    let entities = store.entities_for_revision(&outcome.revision.id).unwrap();
    let task = entities
        .iter()
        .find(|entity| matches!(entity.data, MemoryEntityData::TaskReference { .. }))
        .unwrap();
    let milestone = entities
        .iter()
        .find(|entity| matches!(entity.data, MemoryEntityData::Milestone { .. }))
        .unwrap();
    let link_set = store
        .latest_repository_link_set(&outcome.revision.id, &graph_repository())
        .unwrap()
        .unwrap();
    let links = store.repository_links(&link_set.id).unwrap();
    for source in [&task.id, &milestone.id] {
        assert!(links.iter().any(|relationship| {
            &relationship.source == source
                && relationship.provenance.resolution == MemoryResolutionState::Resolved
                && matches!(
                    &relationship.target,
                    MemoryRelationshipTarget::RepositoryPath {
                        path,
                        snapshot_id: Some(snapshot),
                        ..
                    } if path.as_str() == "src/lib.rs"
                        && snapshot.as_str() == "snapshot-current"
                )
        }));
    }
}

#[test]
fn archive_and_runtime_adapters_create_only_provenance_entities() {
    let root = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    init(root.path());
    let archive = data.path().join("archive/specs/example-20260803");
    fs::create_dir_all(archive.join("tasks")).unwrap();
    fs::create_dir_all(archive.join("runs/t-1")).unwrap();
    fs::write(archive.join("tasks/t-1.md"), "raw task body secret").unwrap();
    fs::write(
        archive.join("runs/t-1/SUBMISSION.md"),
        "raw submission body secret",
    )
    .unwrap();
    for (path, body) in [
        ("runs/t-1/REVIEW.md", "raw review body secret"),
        ("runs/t-1/PATCH.diff", "raw patch body secret"),
        ("runs/t-1/QUESTION.md", "raw question body secret"),
        ("runs/t-1/ANSWER.md", "raw answer body secret"),
        (
            "runs/t-1/CONSULT_REQUEST.md",
            "raw consultation request secret",
        ),
        (
            "runs/t-1/CONSULT_RESPONSE.md",
            "raw consultation response secret",
        ),
        (
            "runs/t-1/INTEGRATION_ERROR.md",
            "raw integration error secret",
        ),
        ("runs/t-1/check.log", "raw check log secret"),
    ] {
        fs::write(archive.join(path), body).unwrap();
    }
    fs::write(
        archive.join("manifest.toml"),
        r#"spec_path = "docs/specs/example.md"
archived_at = "2026-08-03T00:00:00Z"

[[tasks]]
id = "t-1"
status = "complete"
milestone_id = "one"
original_task_path = "/private/task.md"
archived_task_path = "tasks/t-1.md"
original_run_dir = "/private/run"
archived_run_dir = "runs/t-1"
"#,
    )
    .unwrap();

    let connection = Connection::open(data.path().join("ferrus.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE tasks(id TEXT, milestone_id TEXT, status TEXT, spec_path TEXT, \
                    baseline_snapshot_id TEXT, repository_view_snapshot_id TEXT); \
                 CREATE TABLE runs(id TEXT, task_id TEXT, status TEXT, baseline_snapshot_id TEXT, \
                    repository_view_snapshot_id TEXT); \
                 CREATE TABLE events(id INTEGER, run_id TEXT, type TEXT, payload_json TEXT); \
                 CREATE TABLE spec_archives(\
                    id INTEGER PRIMARY KEY, spec_path TEXT, archive_dir TEXT, closed_at TEXT, \
                    task_count INTEGER, run_count INTEGER, outcome TEXT\
                 );",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO spec_archives VALUES (\
                1, 'docs/specs/example.md', ?1, '2026-08-03T00:00:00Z', 1, 1, ''\
             )",
            [archive.to_string_lossy().as_ref()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO tasks VALUES (\
                    't-1', 'one', 'complete', 'docs/specs/example.md', NULL, NULL\
                 )",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO runs VALUES ('run-1', 't-1', 'completed', NULL, NULL)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO events VALUES (1, 'run-1', 'check_passed', ?1)",
            params![r#"{"secret":"must not persist"}"#],
        )
        .unwrap();

    let source = source(&root, &data);
    let mut store = MemorySidecar::open_at(data.path()).unwrap();
    let outcome = MemoryIndexer::new(&source, &mut store)
        .unwrap()
        .index(MemoryIndexOptions::default())
        .unwrap();
    let entities = store.entities_for_revision(&outcome.revision.id).unwrap();
    let archive_entity = entities
        .iter()
        .find(|entity| matches!(entity.data, MemoryEntityData::ArchiveReference { .. }))
        .unwrap();
    assert!(
        entities
            .iter()
            .any(|entity| matches!(entity.data, MemoryEntityData::ArchiveReference { .. }))
    );
    assert!(
        entities
            .iter()
            .any(|entity| matches!(entity.data, MemoryEntityData::TaskReference { .. }))
    );
    assert!(
        entities
            .iter()
            .any(|entity| matches!(entity.data, MemoryEntityData::RunReference { .. }))
    );
    assert!(entities.iter().any(|entity| {
        matches!(
            entity.data,
            MemoryEntityData::ValidationEvidence { text: None, .. }
        )
    }));
    let serialized = serde_json::to_string(&entities).unwrap();
    for forbidden in [
        "raw task body secret",
        "raw submission body secret",
        "raw review body secret",
        "raw patch body secret",
        "raw check log secret",
        "raw question body secret",
        "raw answer body secret",
        "raw consultation request secret",
        "raw consultation response secret",
        "raw integration error secret",
        "/private/task.md",
        "must not persist",
    ] {
        assert!(!serialized.contains(forbidden));
        assert!(
            !fs::read(store.path())
                .unwrap()
                .windows(forbidden.len())
                .any(|window| window == forbidden.as_bytes()),
            "forbidden source body was persisted: {forbidden}"
        );
    }
    let links = store
        .repository_links(&outcome.repository_link_set.id)
        .unwrap();
    assert!(links.iter().any(|relationship| {
        relationship.source == archive_entity.id
            && relationship.provenance.resolution == MemoryResolutionState::Unresolved
            && matches!(
                &relationship.target,
                MemoryRelationshipTarget::RepositoryPath { path, snapshot_id: None, .. }
                    if path.as_str() == "docs/specs/example.md"
            )
    }));
}
