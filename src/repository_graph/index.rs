//! Incremental local indexing coordinator.
//!
//! The coordinator owns orchestration only: immutable source access, stateless
//! extractors and resolver, and a backend-neutral index store. Source bytes are
//! never placed in the fragment cache or sidecar.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

use serde::Serialize;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use super::{
    GRAPH_MODEL_VERSION,
    config::RepositoryGraphConfig,
    diagnostics::{GraphLifecycleEvent, LifecycleCounters, LifecycleEventKind, TracingEventSink},
    domain::{
        BuildId, BuildState, DiagnosticCode, DiagnosticLocation, DiagnosticSeverity, GraphBuild,
        GraphDiagnostic, GraphEdge, GraphNode, GraphSnapshot, PublishedViewName, SnapshotId,
    },
    extractors::{
        builtin_extractor_identities, cargo::CargoExtractor, generic::GenericExtractor,
        rust::RustSyntaxExtractor,
    },
    ports::{
        CachedFragment, CrossFileResolutionInput, CrossFileResolver, DynExtractor, EventSink,
        ExtractionContext, FileExtractionInput, FragmentCacheKey, GraphFragment, IndexBuildMetrics,
        IndexCommit, IndexStore, RepositorySource, ResolutionBudget, SourceFileDescriptor,
        SourceManifest,
    },
    resolution::ConservativeResolver,
    source::{SourceError, extractor_set_digest},
    store::{BuildFailure, PublicationOutcome, PublicationVersion, PublishRequest},
};

const GENERIC_EXTRACTOR_ID: &str = "builtin.generic-structure";
const RESOLVER_ID: &str = "builtin.rust-cargo-resolver";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRequest {
    pub build_id: BuildId,
    pub view_name: PublishedViewName,
    pub force_full: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexOutcome {
    pub build_id: BuildId,
    pub snapshot: GraphSnapshot,
    pub publication: PublicationOutcome,
    pub metrics: IndexBuildMetrics,
    pub reused_existing_snapshot: bool,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum IndexError {
    #[error("repository graph configuration is invalid")]
    Config,
    #[error("configured repository graph analyzer is unavailable")]
    AnalyzerUnavailable,
    #[error("source manifest extractor identity does not match the active indexer")]
    ExtractorSetMismatch,
    #[error("repository graph storage read failed")]
    StorageRead,
    #[error("repository graph build could not be started")]
    BuildStart,
    #[error("repository graph fragment cache failed")]
    Cache,
    #[error("verified repository source content could not be read")]
    SourceRead,
    #[error("repository graph extraction failed")]
    Extraction,
    #[error("repository graph fragments contain conflicting deterministic facts")]
    FragmentConflict,
    #[error("repository graph cross-file resolution failed")]
    Resolution,
    #[error("repository source could not be revalidated")]
    Revalidation,
    #[error("repository source changed during indexing")]
    SourceChanged,
    #[error("complete repository graph snapshot could not be persisted")]
    Commit,
    #[error("repository graph publication failed")]
    Publication,
}

impl IndexError {
    fn diagnostic_code(self) -> DiagnosticCode {
        let code = match self {
            Self::Config => "index.config_invalid",
            Self::AnalyzerUnavailable => "index.analyzer_unavailable",
            Self::ExtractorSetMismatch => "index.extractor_set_mismatch",
            Self::StorageRead => "index.storage_read_failed",
            Self::BuildStart => "index.build_start_failed",
            Self::Cache => "index.cache_failed",
            Self::SourceRead => "index.source_read_failed",
            Self::Extraction => "index.extraction_failed",
            Self::FragmentConflict => "index.fragment_conflict",
            Self::Resolution => "index.resolution_failed",
            Self::Revalidation => "index.revalidation_failed",
            Self::SourceChanged => "index.source_changed",
            Self::Commit => "index.commit_failed",
            Self::Publication => "index.publication_failed",
        };
        DiagnosticCode::new(code).expect("static index diagnostic code is canonical")
    }
}

pub struct IndexCoordinator<'a, Store, Sink = TracingEventSink> {
    store: &'a mut Store,
    sink: Sink,
    resolver: ConservativeResolver,
}

impl<'a, Store> IndexCoordinator<'a, Store, TracingEventSink> {
    pub fn new(store: &'a mut Store) -> Self {
        Self {
            store,
            sink: TracingEventSink,
            resolver: ConservativeResolver::new(),
        }
    }
}

impl<'a, Store, Sink> IndexCoordinator<'a, Store, Sink> {
    pub fn with_event_sink(store: &'a mut Store, sink: Sink) -> Self {
        Self {
            store,
            sink,
            resolver: ConservativeResolver::new(),
        }
    }
}

impl<Store, Sink> IndexCoordinator<'_, Store, Sink>
where
    Store: IndexStore,
    Sink: EventSink,
{
    pub fn index(
        &mut self,
        source: &dyn RepositorySource<Error = SourceError>,
        config: &RepositoryGraphConfig,
        request: IndexRequest,
    ) -> Result<IndexOutcome, IndexError> {
        let started = Instant::now();
        let manifest = source.manifest();
        let active = ActiveExtractors::new(config)?;
        if manifest.extractor_set_digest != extractor_set_digest(&active.identities) {
            return Err(IndexError::ExtractorSetMismatch);
        }
        let analysis_config_digest = config
            .analysis_config_digest()
            .map_err(|_| IndexError::Config)?;
        if manifest.revision.analysis_config_digest != analysis_config_digest {
            return Err(IndexError::Config);
        }
        let snapshot_id = snapshot_identity(manifest);
        let expected = self
            .store
            .published_view(&manifest.revision.repository, &request.view_name)
            .map_err(|_| IndexError::StorageRead)?
            .map(|view| PublicationVersion {
                snapshot_id: view.snapshot_id,
                generation: view.generation,
            });
        let reused_existing_snapshot = self
            .store
            .snapshot(&snapshot_id)
            .map_err(|_| IndexError::StorageRead)?
            .is_some();
        let build = GraphBuild {
            id: request.build_id.clone(),
            repository: manifest.revision.repository.clone(),
            source_revision_id: manifest.revision.id.clone(),
            prospective_snapshot_id: snapshot_id.clone(),
            state: BuildState::Building,
        };
        self.store
            .start_build(&build)
            .map_err(|_| IndexError::BuildStart)?;
        self.emit(
            LifecycleEventKind::BuildStarted,
            &build,
            None,
            &IndexBuildMetrics::default(),
        );

        let mut prepared = match self.prepare(
            source,
            config,
            manifest,
            &active,
            &build,
            request.force_full,
            started,
        ) {
            Ok(prepared) => prepared,
            Err((error, metrics)) => {
                self.fail_started_build(&build, error, &metrics);
                return Err(error);
            }
        };
        match source.revalidate() {
            Ok(true) => {}
            Ok(false) => {
                prepared.metrics.duration_ms = elapsed_ms(started);
                self.fail_started_build(&build, IndexError::SourceChanged, &prepared.metrics);
                return Err(IndexError::SourceChanged);
            }
            Err(_) => {
                prepared.metrics.duration_ms = elapsed_ms(started);
                self.fail_started_build(&build, IndexError::Revalidation, &prepared.metrics);
                return Err(IndexError::Revalidation);
            }
        }

        prepared.metrics.duration_ms = elapsed_ms(started);
        let requested_snapshot = GraphSnapshot {
            id: snapshot_id.clone(),
            repository: manifest.revision.repository.clone(),
            source_revision_id: manifest.revision.id.clone(),
            source_manifest_digest: manifest.revision.manifest_digest.clone(),
            graph_model_version: GRAPH_MODEL_VERSION,
            analysis_config_digest: manifest.revision.analysis_config_digest.clone(),
            extractor_set_digest: manifest.extractor_set_digest.clone(),
            completed_by: build.id.clone(),
        };
        let completed = self
            .store
            .complete_index(&IndexCommit {
                snapshot: requested_snapshot,
                files: manifest.files.clone(),
                graph: prepared.graph,
                cache_writes: prepared.cache_writes,
                metrics: prepared.metrics.clone(),
            })
            .map_err(|_| {
                self.fail_started_build(&build, IndexError::Commit, &prepared.metrics);
                IndexError::Commit
            })?;
        self.emit(
            LifecycleEventKind::SnapshotCompleted,
            &build,
            Some(&completed.id),
            &prepared.metrics,
        );

        match source.revalidate() {
            Ok(true) => {}
            state => {
                let _ = self.store.supersede_build(&build.id);
                self.emit(
                    LifecycleEventKind::BuildSuperseded,
                    &build,
                    Some(&completed.id),
                    &prepared.metrics,
                );
                return Err(if state.is_ok() {
                    IndexError::SourceChanged
                } else {
                    IndexError::Revalidation
                });
            }
        }

        let publication = self.store.publish(&PublishRequest {
            repository: manifest.revision.repository.clone(),
            view_name: request.view_name.clone(),
            build_id: build.id.clone(),
            expected: expected.clone(),
        });
        let (publication, build_needs_supersede) = match publication {
            Ok(PublicationOutcome::Published { view }) => {
                let build_needs_supersede = view.build_id != build.id;
                (
                    PublicationOutcome::Published { view },
                    build_needs_supersede,
                )
            }
            Ok(outcome @ PublicationOutcome::Superseded { .. }) => (outcome, false),
            Err(_) => {
                self.emit(
                    LifecycleEventKind::PublicationConflict,
                    &build,
                    Some(&completed.id),
                    &prepared.metrics,
                );
                let current = self
                    .store
                    .published_view(&manifest.revision.repository, &request.view_name)
                    .map_err(|_| IndexError::Publication)?;
                let Some(current) = current else {
                    return Err(IndexError::Publication);
                };
                let current_version = PublicationVersion {
                    snapshot_id: current.snapshot_id.clone(),
                    generation: current.generation,
                };
                if current.build_id == build.id {
                    (PublicationOutcome::Published { view: current }, false)
                } else if expected.as_ref() != Some(&current_version) {
                    (PublicationOutcome::Superseded { current }, true)
                } else {
                    return Err(IndexError::Publication);
                }
            }
        };
        let publication_is_other_build = match &publication {
            PublicationOutcome::Published { view } => view.build_id != build.id,
            PublicationOutcome::Superseded { .. } => true,
        };
        if publication_is_other_build {
            if build_needs_supersede {
                self.store
                    .supersede_build(&build.id)
                    .map_err(|_| IndexError::Publication)?;
            }
            self.emit(
                LifecycleEventKind::BuildSuperseded,
                &build,
                Some(&completed.id),
                &prepared.metrics,
            );
        } else {
            self.emit(
                LifecycleEventKind::SnapshotPublished,
                &build,
                Some(&completed.id),
                &prepared.metrics,
            );
        }
        Ok(IndexOutcome {
            build_id: build.id,
            snapshot: completed,
            publication,
            metrics: prepared.metrics,
            reused_existing_snapshot,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare(
        &mut self,
        source: &dyn RepositorySource<Error = SourceError>,
        config: &RepositoryGraphConfig,
        manifest: &SourceManifest,
        active: &ActiveExtractors,
        build: &GraphBuild,
        force_full: bool,
        started: Instant,
    ) -> Result<PreparedIndex, (IndexError, IndexBuildMetrics)> {
        let context = ExtractionContext {
            snapshot_id: build.prospective_snapshot_id.clone(),
            build_id: build.id.clone(),
            repository: build.repository.clone(),
            max_facts_per_file: config.index_limits.max_facts_per_file,
            max_parser_duration_ms: config.index_limits.max_parser_duration_ms,
            max_diagnostics: config.index_limits.max_diagnostics,
        };
        let mut metrics = IndexBuildMetrics {
            discovered_files: manifest.files.len() as u64,
            skipped_files: manifest.metrics.skipped,
            ..IndexBuildMetrics::default()
        };
        let mut merger = FragmentMerger::new();
        merger
            .merge(source_diagnostics(&context, manifest))
            .map_err(|error| (error, metrics.clone()))?;
        if active.generic_enabled {
            merger
                .merge(GenericExtractor::new().repository_fragment(&context, manifest))
                .map_err(|error| (error, metrics.clone()))?;
        }
        let mut cache_writes = Vec::new();

        let generic = GenericExtractor::new();
        let cargo = CargoExtractor::new();
        let rust = RustSyntaxExtractor::new();
        let available: [&dyn DynExtractor; 3] = [&generic, &cargo, &rust];
        for file in &manifest.files {
            let extractors = available
                .iter()
                .copied()
                .filter(|extractor| active.file_ids.contains(extractor.identity().id.as_str()))
                .filter(|extractor| extractor.supports(file))
                .collect::<Vec<_>>();
            if extractors.is_empty() {
                metrics.skipped_files = metrics.skipped_files.saturating_add(1);
                continue;
            }
            let mut fragments = Vec::new();
            let mut missing = Vec::new();
            for extractor in extractors {
                let key = cache_key(
                    &context,
                    file,
                    extractor.identity(),
                    &manifest.revision.analysis_config_digest,
                );
                let cached = if force_full {
                    None
                } else {
                    self.store
                        .load_cached_fragment(&key)
                        .map_err(|_| (IndexError::Cache, metrics.clone()))?
                };
                if let Some(cached) = cached
                    && cache_fragment_is_valid(&cached, &key)
                {
                    fragments.push(rebase_fragment(cached, &context));
                } else {
                    missing.push((extractor, key));
                }
            }

            if missing.is_empty() {
                metrics.reused_files = metrics.reused_files.saturating_add(1);
            } else {
                metrics.parsed_files = metrics.parsed_files.saturating_add(1);
                let content = source.read_verified(file).map_err(|_| {
                    metrics.failed_files = metrics.failed_files.saturating_add(1);
                    metrics.duration_ms = elapsed_ms(started);
                    (IndexError::SourceRead, metrics.clone())
                })?;
                metrics.processed_bytes = metrics
                    .processed_bytes
                    .saturating_add(content.bytes.len() as u64);
                for (extractor, key) in missing {
                    let fragment = extractor
                        .extract(FileExtractionInput {
                            context: &context,
                            file,
                            content: &content.bytes,
                        })
                        .map_err(|_| {
                            metrics.failed_files = metrics.failed_files.saturating_add(1);
                            metrics.duration_ms = elapsed_ms(started);
                            (IndexError::Extraction, metrics.clone())
                        })?;
                    cache_writes.push(CachedFragment {
                        key,
                        fragment: fragment.clone(),
                    });
                    fragments.push(fragment);
                }
            }
            for fragment in fragments {
                merger
                    .merge(fragment)
                    .map_err(|error| (error, metrics.clone()))?;
            }
        }

        let unresolved = merger.finish(&context);
        let graph = if active.resolver_enabled {
            self.resolver
                .resolve(CrossFileResolutionInput {
                    context: &context,
                    manifest,
                    fragment: unresolved,
                    budget: ResolutionBudget {
                        max_relationships: config.index_limits.max_resolved_relationships,
                        max_added_relationships: config.index_limits.max_resolved_relationships,
                        max_duration_ms: config.index_limits.max_resolver_duration_ms,
                        max_diagnostics: config.index_limits.max_diagnostics,
                    },
                })
                .map_err(|_| (IndexError::Resolution, metrics.clone()))?
        } else {
            unresolved
        };
        metrics.nodes = graph.nodes.len() as u64;
        metrics.edges = graph.edges.len() as u64;
        metrics.diagnostics = graph.diagnostics.len() as u64;
        metrics.duration_ms = elapsed_ms(started);
        Ok(PreparedIndex {
            graph,
            cache_writes,
            metrics,
        })
    }

    fn fail_started_build(
        &mut self,
        build: &GraphBuild,
        error: IndexError,
        metrics: &IndexBuildMetrics,
    ) {
        let _ = self.store.record_build_metrics(&build.id, metrics);
        let _ = self.store.fail_build(&BuildFailure {
            build_id: build.id.clone(),
            code: error.diagnostic_code(),
        });
        self.emit(LifecycleEventKind::BuildFailed, build, None, metrics);
    }

    fn emit(
        &self,
        kind: LifecycleEventKind,
        build: &GraphBuild,
        snapshot_id: Option<&SnapshotId>,
        metrics: &IndexBuildMetrics,
    ) {
        let _ = self.sink.emit(GraphLifecycleEvent {
            kind,
            repository: &build.repository,
            build_id: &build.id,
            snapshot_id,
            counters: LifecycleCounters {
                files: metrics.discovered_files,
                nodes: metrics.nodes,
                edges: metrics.edges,
                diagnostics: metrics.diagnostics,
            },
            duration_ms: Some(metrics.duration_ms),
        });
    }
}

struct PreparedIndex {
    graph: GraphFragment,
    cache_writes: Vec<CachedFragment>,
    metrics: IndexBuildMetrics,
}

struct ActiveExtractors {
    identities: Vec<super::domain::ExtractorIdentity>,
    file_ids: BTreeSet<String>,
    generic_enabled: bool,
    resolver_enabled: bool,
}

impl ActiveExtractors {
    fn new(config: &RepositoryGraphConfig) -> Result<Self, IndexError> {
        let builtins = builtin_extractor_identities();
        let known = builtins
            .iter()
            .map(|identity| identity.id.as_str())
            .collect::<BTreeSet<_>>();
        if config
            .analyzers
            .enabled
            .iter()
            .any(|enabled| !known.contains(enabled.as_str()))
        {
            return Err(IndexError::AnalyzerUnavailable);
        }
        let identities = if config.analyzers.enabled.is_empty() {
            builtins
        } else {
            builtins
                .into_iter()
                .filter(|identity| config.analyzers.enabled.contains(identity.id.as_str()))
                .collect()
        };
        let file_ids = identities
            .iter()
            .filter(|identity| identity.id.as_str() != RESOLVER_ID)
            .map(|identity| identity.id.as_str().to_string())
            .collect::<BTreeSet<_>>();
        Ok(Self {
            generic_enabled: file_ids.contains(GENERIC_EXTRACTOR_ID),
            resolver_enabled: identities
                .iter()
                .any(|identity| identity.id.as_str() == RESOLVER_ID),
            identities,
            file_ids,
        })
    }
}

pub fn active_extractor_identities(
    config: &RepositoryGraphConfig,
) -> Result<Vec<super::domain::ExtractorIdentity>, IndexError> {
    Ok(ActiveExtractors::new(config)?.identities)
}

fn cache_key(
    context: &ExtractionContext,
    file: &SourceFileDescriptor,
    extractor: super::domain::ExtractorIdentity,
    analysis_config_digest: &super::domain::Digest,
) -> FragmentCacheKey {
    FragmentCacheKey {
        repository: context.repository.clone(),
        path: file.path.clone(),
        content_identity: file.content_identity.clone(),
        byte_len: file.byte_len,
        file_mode: file.file_mode,
        analysis_config_digest: analysis_config_digest.clone(),
        extractor,
    }
}

fn cache_fragment_is_valid(fragment: &GraphFragment, key: &FragmentCacheKey) -> bool {
    fragment
        .nodes
        .iter()
        .all(|node| fact_provenance_is_valid(&node.provenance, key))
        && fragment
            .edges
            .iter()
            .all(|edge| fact_provenance_is_valid(&edge.provenance, key))
}

fn fact_provenance_is_valid(
    provenance: &super::domain::FactProvenance,
    key: &FragmentCacheKey,
) -> bool {
    provenance.extractor == key.extractor
        && provenance.evidence.as_ref().is_none_or(|evidence| {
            evidence.path == key.path && evidence.content_identity == key.content_identity
        })
}

fn rebase_fragment(mut fragment: GraphFragment, context: &ExtractionContext) -> GraphFragment {
    // Fact IDs are canonical extractor-local identities. Snapshot scope lives in
    // the separate `snapshot_id` field (and in SQLite's composite keys), so a
    // cached fragment must only rebind that scope and its build diagnostics.
    for node in &mut fragment.nodes {
        node.snapshot_id = context.snapshot_id.clone();
    }
    for edge in &mut fragment.edges {
        edge.snapshot_id = context.snapshot_id.clone();
    }
    for diagnostic in &mut fragment.diagnostics {
        diagnostic.build_id = context.build_id.clone();
        diagnostic.snapshot_id = Some(context.snapshot_id.clone());
    }
    fragment
}

struct FragmentMerger {
    nodes: BTreeMap<super::domain::NodeId, GraphNode>,
    edges: BTreeMap<super::domain::EdgeId, GraphEdge>,
    diagnostics: Vec<GraphDiagnostic>,
}

impl FragmentMerger {
    fn new() -> Self {
        Self {
            nodes: BTreeMap::new(),
            edges: BTreeMap::new(),
            diagnostics: Vec::new(),
        }
    }

    fn merge(&mut self, fragment: GraphFragment) -> Result<(), IndexError> {
        for node in fragment.nodes {
            if let Some(existing) = self.nodes.get(&node.id)
                && existing != &node
            {
                return Err(IndexError::FragmentConflict);
            }
            self.nodes.insert(node.id.clone(), node);
        }
        for edge in fragment.edges {
            if let Some(existing) = self.edges.get(&edge.id)
                && existing != &edge
            {
                return Err(IndexError::FragmentConflict);
            }
            self.edges.insert(edge.id.clone(), edge);
        }
        self.diagnostics.extend(fragment.diagnostics);
        Ok(())
    }

    fn finish(mut self, context: &ExtractionContext) -> GraphFragment {
        self.diagnostics.sort_by(|left, right| {
            left.code
                .cmp(&right.code)
                .then_with(|| diagnostic_location_key(left).cmp(&diagnostic_location_key(right)))
        });
        self.diagnostics.dedup();
        for diagnostic in &mut self.diagnostics {
            diagnostic.build_id = context.build_id.clone();
            diagnostic.snapshot_id = Some(context.snapshot_id.clone());
        }
        GraphFragment {
            nodes: self.nodes.into_values().collect(),
            edges: self.edges.into_values().collect(),
            diagnostics: self.diagnostics,
        }
    }
}

fn source_diagnostics(context: &ExtractionContext, manifest: &SourceManifest) -> GraphFragment {
    GraphFragment {
        diagnostics: manifest
            .diagnostics
            .iter()
            .map(|diagnostic| GraphDiagnostic {
                build_id: context.build_id.clone(),
                snapshot_id: Some(context.snapshot_id.clone()),
                severity: DiagnosticSeverity::Warning,
                code: diagnostic.code.clone(),
                location: diagnostic
                    .path
                    .clone()
                    .map(|path| DiagnosticLocation { path, span: None }),
                metrics: BTreeMap::new(),
            })
            .collect(),
        ..GraphFragment::default()
    }
}

fn diagnostic_location_key(diagnostic: &GraphDiagnostic) -> (Option<&str>, Option<(u64, u64)>) {
    let location = diagnostic.location.as_ref();
    (
        location.map(|location| location.path.as_str()),
        location
            .and_then(|location| location.span.as_ref())
            .map(|span| (span.start.byte_offset, span.end.byte_offset)),
    )
}

#[derive(Serialize)]
struct SnapshotIdentity<'a> {
    version: u32,
    repository: &'a super::domain::RepositoryRef,
    source_manifest_digest: &'a super::domain::Digest,
    graph_model_version: u32,
    analysis_config_digest: &'a super::domain::Digest,
    extractor_set_digest: &'a super::domain::Digest,
}

pub fn snapshot_identity(manifest: &SourceManifest) -> SnapshotId {
    snapshot_identity_from_revision(&manifest.revision, &manifest.extractor_set_digest)
}

pub fn snapshot_identity_from_revision(
    revision: &super::domain::SourceRevision,
    extractor_set_digest: &super::domain::Digest,
) -> SnapshotId {
    let identity = SnapshotIdentity {
        version: 1,
        repository: &revision.repository,
        source_manifest_digest: &revision.manifest_digest,
        graph_model_version: GRAPH_MODEL_VERSION,
        analysis_config_digest: &revision.analysis_config_digest,
        extractor_set_digest,
    };
    let bytes = serde_json::to_vec(&identity)
        .expect("canonical snapshot identity serialization cannot fail");
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    SnapshotId::new(format!("snapshot:{encoded}"))
        .expect("prefixed sha256 snapshot identity is non-empty")
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "index_tests.rs"]
mod tests;
