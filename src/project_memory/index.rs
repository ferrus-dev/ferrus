//! Deterministic incremental project-memory indexing coordinator.

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use chrono::Utc;
use serde::Serialize;
use thiserror::Error;

use super::{
    diagnostics::{MemoryDiagnostic, MemoryDiagnosticCode, MemoryDiagnosticSeverity},
    domain::{
        CachedMemoryFragment, MemoryBuild, MemoryBuildId, MemoryBuildMetrics, MemoryBuildState,
        MemoryCommit, MemoryEntity, MemoryEntityId, MemoryFragment, MemoryFragmentCacheKey,
        MemoryPublicationOutcome, MemoryPublicationVersion, MemoryPublishRequest,
        MemoryRelationship, MemoryRelationshipId, MemoryRepositoryLinkSet, MemoryResolutionState,
        MemoryRevision, MemorySourceCategory, MemoryViewName, PublishedMemoryRevision,
    },
    extractors::built_in_extractors,
    links::LocalRepositoryLinkResolver,
    ports::{
        MemoryBuildFailure, MemoryExtractionContext, MemoryExtractionInput, MemoryLinkStore,
        MemorySource, MemoryStore,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryIndexOutcome {
    pub build_id: MemoryBuildId,
    pub revision: MemoryRevision,
    pub publication: MemoryPublicationOutcome,
    pub repository_link_set: MemoryRepositoryLinkSet,
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
    #[error("project-memory repository link resolution failed")]
    Links(#[source] anyhow::Error),
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
        let mut store = match MemorySidecar::open_at(&data_dir) {
            Ok(store) => store,
            Err(MemoryStoreError::RequiresRebuild) if options.full => {
                remove_memory_sidecar_file_set(&data_dir)?;
                MemorySidecar::open_at(&data_dir)?
            }
            Err(error) => return Err(error.into()),
        };
        MemoryIndexer::new(&source, &mut store)
            .expect("static memory view name is valid")
            .index(options)
    })
    .await
    .map_err(|error| MemoryIndexError::Source(anyhow::Error::from(error)))?
}

fn remove_memory_sidecar_file_set(data_dir: &std::path::Path) -> Result<(), MemoryStoreError> {
    let path = data_dir.join(super::sqlite::MEMORY_SIDECAR_FILE_NAME);
    let suffixed = |suffix: &str| {
        let mut value = path.as_os_str().to_os_string();
        value.push(suffix);
        std::path::PathBuf::from(value)
    };
    for candidate in [path.clone(), suffixed("-wal"), suffixed("-shm")] {
        match std::fs::remove_file(candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(MemoryStoreError::Database(
                    rusqlite::Error::ToSqlConversionFailure(Box::new(error)),
                ));
            }
        }
    }
    Ok(())
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
        let entities = entities.into_values().collect::<Vec<_>>();
        let relationships = relationships.into_values().collect::<Vec<_>>();
        let resolver = LocalRepositoryLinkResolver::open(self.source.data_dir(), &revision.project)
            .map_err(MemoryIndexError::Links)?;
        let current_link_set_id = resolver
            .link_set_id(&revision, &entities)
            .map_err(MemoryIndexError::Links)?;
        let previous_link_set = match self.store.repository_link_set(&current_link_set_id)? {
            Some(link_set) => Some(link_set),
            None => self
                .store
                .latest_repository_link_set(&revision.id, resolver.repository())?,
        };
        let previous_links = previous_link_set
            .as_ref()
            .map(|set| self.store.repository_links(&set.id))
            .transpose()?;
        let repository_links = resolver
            .resolve(
                &revision,
                &entities,
                previous_link_set.as_ref().zip(previous_links.as_deref()),
            )
            .map_err(MemoryIndexError::Links)?;
        metrics.entities = entities.len() as u64;
        metrics.relationships =
            relationships.len() as u64 + repository_links.relationships.len() as u64;
        metrics.stale_links = repository_links
            .relationships
            .iter()
            .filter(|relationship| {
                relationship.provenance.resolution == MemoryResolutionState::Stale
            })
            .count() as u64;
        diagnostics.extend(
            repository_links
                .diagnostics
                .iter()
                .take(MAX_DIAGNOSTICS.saturating_sub(diagnostics.len()))
                .cloned(),
        );
        metrics.diagnostics = diagnostics.len() as u64;
        metrics.duration_ms = started.elapsed().as_millis() as u64;
        Ok(MemoryCommit {
            revision,
            entities,
            relationships,
            cache_writes,
            diagnostics,
            repository_links: vec![repository_links],
            metrics,
        })
    }

    fn finish(
        &mut self,
        commit: MemoryCommit,
        expected: Option<MemoryPublicationVersion>,
    ) -> Result<MemoryIndexOutcome, MemoryIndexError> {
        let candidate_revision = commit.revision.clone();
        let repository_link_set = commit
            .repository_links
            .first()
            .expect("local memory builds always resolve one repository link set")
            .link_set
            .clone();
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
            repository_link_set,
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
#[path = "index_tests.rs"]
mod tests;
