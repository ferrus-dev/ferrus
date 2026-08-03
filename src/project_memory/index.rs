//! Deterministic incremental project-memory indexing coordinator.

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use chrono::Utc;
use thiserror::Error;

use super::{
    diagnostics::{MemoryDiagnostic, MemoryDiagnosticCode, MemoryDiagnosticSeverity},
    domain::{
        CachedMemoryFragment, MemoryBuild, MemoryBuildId, MemoryBuildMetrics, MemoryBuildState,
        MemoryCommit, MemoryEntity, MemoryEntityId, MemoryFragment, MemoryFragmentCacheKey,
        MemoryPublicationOutcome, MemoryPublicationVersion, MemoryPublishRequest,
        MemoryRelationship, MemoryRelationshipId, MemoryRevision, MemorySourceCategory,
        MemoryViewName, PublishedMemoryRevision,
    },
    extractors::built_in_extractors,
    ports::{
        MemoryBuildFailure, MemoryExtractionContext, MemoryExtractionInput, MemorySource,
        MemoryStore,
    },
    source::LocalMemorySource,
    sqlite::{MemorySidecar, MemoryStoreError},
};

const MAX_ENTITIES_PER_SOURCE: u64 = 10_000;
const MAX_RELATIONSHIPS_PER_SOURCE: u64 = 20_000;
const MAX_DIAGNOSTICS: usize = 1_000;
const MAX_PARSER_DURATION_MS: u64 = 5_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryIndexOptions {
    pub full: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryIndexOutcome {
    pub build_id: MemoryBuildId,
    pub revision: MemoryRevision,
    pub publication: MemoryPublicationOutcome,
    pub metrics: MemoryBuildMetrics,
}

#[derive(Debug, Error)]
pub enum MemoryIndexError {
    #[error("project-memory source discovery or verification failed")]
    Source(#[source] anyhow::Error),
    #[error("project-memory storage failed")]
    Store(#[from] MemoryStoreError),
    #[error("project-memory identity validation failed")]
    Identity(#[from] super::domain::AuthorizedSourceManifestError),
    #[error("project-memory facts contain conflicting deterministic identities")]
    FactCollision,
}

pub struct MemoryIndexer<'a> {
    source: &'a LocalMemorySource,
    store: &'a mut MemorySidecar,
    view_name: MemoryViewName,
}

pub async fn index_current_project(
    options: MemoryIndexOptions,
) -> Result<MemoryIndexOutcome, MemoryIndexError> {
    let source = LocalMemorySource::discover_current()
        .await
        .map_err(MemoryIndexError::Source)?;
    let data_dir = source.data_dir().to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut store = MemorySidecar::open_at(&data_dir)?;
        MemoryIndexer::new(&source, &mut store)
            .expect("static memory view name is valid")
            .index(options)
    })
    .await
    .map_err(|error| MemoryIndexError::Source(anyhow::Error::from(error)))?
}

impl<'a> MemoryIndexer<'a> {
    pub fn new(
        source: &'a LocalMemorySource,
        store: &'a mut MemorySidecar,
    ) -> Result<Self, super::domain::MemoryValueError> {
        Ok(Self {
            source,
            store,
            view_name: MemoryViewName::new("project")?,
        })
    }

    pub fn index(
        &mut self,
        options: MemoryIndexOptions,
    ) -> Result<MemoryIndexOutcome, MemoryIndexError> {
        let started = Instant::now();
        let manifest = self.source.manifest().map_err(MemoryIndexError::Source)?;
        let build_id = next_build_id();
        let revision = MemoryRevision::from_manifest(&manifest, build_id.clone())?;
        let expected = self
            .store
            .published_view(&manifest.project, &self.view_name)?
            .map(|view| MemoryPublicationVersion {
                revision_id: view.revision_id,
                generation: view.generation,
            });
        let build = MemoryBuild {
            id: build_id.clone(),
            project: manifest.project.clone(),
            prospective_revision_id: revision.id.clone(),
            state: MemoryBuildState::Building,
        };
        self.store.start_build(&build)?;
        match self.prepare_commit(&manifest, revision.clone(), options, started) {
            Ok(commit) => self.finish(commit, expected),
            Err(error) => {
                let _ = self.store.fail_build(
                    &build_id,
                    &MemoryBuildFailure {
                        code: diagnostic_code("build.failed"),
                    },
                );
                Err(error)
            }
        }
    }

    fn prepare_commit(
        &self,
        manifest: &super::domain::AuthorizedSourceManifest,
        revision: MemoryRevision,
        options: MemoryIndexOptions,
        started: Instant,
    ) -> Result<MemoryCommit, MemoryIndexError> {
        let indexed_at = Utc::now();
        let context = MemoryExtractionContext {
            project: manifest.project.clone(),
            revision_id: revision.id.clone(),
            build_id: revision.completed_by.clone(),
            indexed_at,
            max_entities_per_source: MAX_ENTITIES_PER_SOURCE,
            max_relationships_per_source: MAX_RELATIONSHIPS_PER_SOURCE,
            max_parser_duration_ms: MAX_PARSER_DURATION_MS,
            max_diagnostics: MAX_DIAGNOSTICS as u64,
        };
        let extractors = built_in_extractors();
        let mut metrics = MemoryBuildMetrics {
            discovered_sources: manifest.sources.len() as u64,
            ..MemoryBuildMetrics::default()
        };
        let mut entities = BTreeMap::<MemoryEntityId, MemoryEntity>::new();
        let mut relationships = BTreeMap::<MemoryRelationshipId, MemoryRelationship>::new();
        let mut cache_writes = Vec::new();
        let mut diagnostics = Vec::new();

        for source in &manifest.sources {
            let Some(extractor) = extractors
                .iter()
                .find(|extractor| extractor.supports(source.category))
            else {
                metrics.skipped_sources += 1;
                push_diagnostic(
                    &mut diagnostics,
                    &revision,
                    source.category,
                    "extractor.unavailable",
                );
                continue;
            };
            let key = MemoryFragmentCacheKey {
                project: manifest.project.clone(),
                category: source.category,
                locator: source.locator.clone(),
                source_fingerprint: source.fingerprint.clone(),
                policy_digest: manifest.policy_digest.clone(),
                extractor: extractor.identity(),
            };
            let fragment = if !options.full {
                self.store.load_cached_fragment(&key)?
            } else {
                None
            };
            let fragment = match fragment {
                Some(mut fragment) => {
                    rebase_fragment(&mut fragment, &revision);
                    metrics.reused_sources += 1;
                    fragment
                }
                None => {
                    let content = self
                        .source
                        .read_verified(source)
                        .map_err(MemoryIndexError::Source)?;
                    metrics.processed_bytes += content.bytes.len() as u64;
                    match extractor.extract(MemoryExtractionInput {
                        context: &context,
                        source,
                        content: &content.bytes,
                    }) {
                        Ok(fragment) => {
                            if fragment.entities.len() as u64 > context.max_entities_per_source
                                || fragment.relationships.len() as u64
                                    > context.max_relationships_per_source
                            {
                                metrics.failed_sources += 1;
                                push_diagnostic(
                                    &mut diagnostics,
                                    &revision,
                                    source.category,
                                    "extractor.fact_limit",
                                );
                                continue;
                            }
                            metrics.extracted_sources += 1;
                            cache_writes.push(CachedMemoryFragment {
                                key,
                                fragment: fragment.clone(),
                            });
                            fragment
                        }
                        Err(failure) => {
                            metrics.failed_sources += 1;
                            push_diagnostic_with_code(
                                &mut diagnostics,
                                &revision,
                                source.category,
                                failure.code,
                            );
                            continue;
                        }
                    }
                }
            };
            merge_fragment(&mut entities, &mut relationships, fragment)?;
        }
        self.source
            .revalidate(manifest)
            .map_err(MemoryIndexError::Source)?;
        metrics.entities = entities.len() as u64;
        metrics.relationships = relationships.len() as u64;
        metrics.diagnostics = diagnostics.len() as u64;
        metrics.duration_ms = started.elapsed().as_millis() as u64;
        Ok(MemoryCommit {
            revision,
            entities: entities.into_values().collect(),
            relationships: relationships.into_values().collect(),
            cache_writes,
            diagnostics,
            metrics,
        })
    }

    fn finish(
        &mut self,
        commit: MemoryCommit,
        expected: Option<MemoryPublicationVersion>,
    ) -> Result<MemoryIndexOutcome, MemoryIndexError> {
        let candidate_revision = commit.revision.clone();
        let build_id = candidate_revision.completed_by.clone();
        let metrics = commit.metrics.clone();
        if let Err(error) = self.store.complete_build(&commit) {
            let _ = self.store.fail_build(
                &candidate_revision.completed_by,
                &MemoryBuildFailure {
                    code: diagnostic_code("build.commit_failed"),
                },
            );
            return Err(error.into());
        }
        let revision = self
            .store
            .revision(&candidate_revision.id)?
            .ok_or(MemoryStoreError::RevisionNotFound)?;
        let request = MemoryPublishRequest {
            project: candidate_revision.project.clone(),
            view_name: self.view_name.clone(),
            build_id: candidate_revision.completed_by.clone(),
            expected,
        };
        let publication = match self.store.publish(&request) {
            Ok(outcome) => outcome,
            Err(MemoryStoreError::PublicationConflict) => {
                self.store
                    .supersede_build(&candidate_revision.completed_by)?;
                let current = self
                    .store
                    .published_view(&revision.project, &self.view_name)?
                    .ok_or(MemoryStoreError::PublicationConflict)?;
                MemoryPublicationOutcome::Superseded { current }
            }
            Err(error) => {
                let _ = self.store.supersede_build(&candidate_revision.completed_by);
                return Err(error.into());
            }
        };
        Ok(MemoryIndexOutcome {
            build_id,
            revision,
            publication,
            metrics,
        })
    }
}

fn merge_fragment(
    entities: &mut BTreeMap<MemoryEntityId, MemoryEntity>,
    relationships: &mut BTreeMap<MemoryRelationshipId, MemoryRelationship>,
    fragment: MemoryFragment,
) -> Result<(), MemoryIndexError> {
    for entity in fragment.entities {
        if let Some(existing) = entities.insert(entity.id.clone(), entity.clone())
            && existing != entity
        {
            return Err(MemoryIndexError::FactCollision);
        }
    }
    for relationship in fragment.relationships {
        if let Some(existing) = relationships.insert(relationship.id.clone(), relationship.clone())
            && existing != relationship
        {
            return Err(MemoryIndexError::FactCollision);
        }
    }
    Ok(())
}

fn rebase_fragment(fragment: &mut MemoryFragment, revision: &MemoryRevision) {
    for entity in &mut fragment.entities {
        entity.memory_revision_id = revision.id.clone();
        entity.project = revision.project.clone();
    }
    for relationship in &mut fragment.relationships {
        relationship.memory_revision_id = revision.id.clone();
        relationship.project = revision.project.clone();
    }
}

fn push_diagnostic(
    diagnostics: &mut Vec<MemoryDiagnostic>,
    revision: &MemoryRevision,
    category: MemorySourceCategory,
    code: &str,
) {
    push_diagnostic_with_code(diagnostics, revision, category, diagnostic_code(code));
}

fn push_diagnostic_with_code(
    diagnostics: &mut Vec<MemoryDiagnostic>,
    revision: &MemoryRevision,
    category: MemorySourceCategory,
    code: MemoryDiagnosticCode,
) {
    if diagnostics.len() >= MAX_DIAGNOSTICS {
        return;
    }
    diagnostics.push(MemoryDiagnostic {
        build_id: revision.completed_by.clone(),
        revision_id: revision.id.clone(),
        severity: MemoryDiagnosticSeverity::Warning,
        code,
        source_category: Some(category),
        entity_id: None,
        relationship_id: None,
        metrics: BTreeMap::new(),
    });
}

fn diagnostic_code(value: &str) -> MemoryDiagnosticCode {
    MemoryDiagnosticCode::new(value).expect("static memory diagnostic code is valid")
}

fn next_build_id() -> MemoryBuildId {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let timestamp = Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let digest = super::extractors::canonical_digest(&(timestamp, sequence));
    MemoryBuildId::new(format!("memory-build:{}", digest.value()))
        .expect("sha256 memory build id is bounded")
}

pub fn published_revision(
    store: &MemorySidecar,
    source: &LocalMemorySource,
) -> Result<Option<PublishedMemoryRevision>, MemoryStoreError> {
    let view = MemoryViewName::new("project").expect("static memory view name is valid");
    store.published_view(source.project(), &view)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, process::Command};

    use rusqlite::{Connection, params};
    use tempfile::TempDir;

    use crate::{
        project_memory::{
            domain::{MemoryEntityData, ProjectId, ProjectNamespace, ProjectRef},
            policy::MemoryPolicy,
            source::LocalMemorySource,
            sqlite::MemorySidecar,
        },
        repository_graph::domain::RepoPath,
    };

    use super::*;

    fn project() -> ProjectRef {
        ProjectRef {
            namespace: ProjectNamespace::new("local:test").unwrap(),
            project_id: ProjectId::new("project-1").unwrap(),
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
        LocalMemorySource::discover_at(
            root.path().to_path_buf(),
            data.path().to_path_buf(),
            project(),
            RepoPath::new("docs/specs").unwrap(),
            MemoryPolicy::default(),
        )
        .unwrap()
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
        assert_eq!(first.metrics.extracted_sources, 2);
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
        assert_eq!(no_op.metrics.reused_sources, 2);
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
        assert_eq!(changed.metrics.reused_sources, 1);
        assert_eq!(changed.metrics.extracted_sources, 1);
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
        assert_eq!(removed.metrics.reused_sources, 1);

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
        assert!(
            store
                .entities_for_revision(&deleted.revision.id)
                .unwrap()
                .is_empty()
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
                "CREATE TABLE tasks(id TEXT, milestone_id TEXT, status TEXT, spec_path TEXT); \
                 CREATE TABLE runs(id TEXT, task_id TEXT, status TEXT); \
                 CREATE TABLE events(id INTEGER, run_id TEXT, type TEXT, payload_json TEXT);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO tasks VALUES ('t-1', 'one', 'complete', 'docs/specs/example.md')",
                [],
            )
            .unwrap();
        connection
            .execute("INSERT INTO runs VALUES ('run-1', 't-1', 'completed')", [])
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
            "/private/task.md",
            "must not persist",
        ] {
            assert!(!serialized.contains(forbidden));
        }
    }
}
