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

        let publication = self
            .store
            .publish(&PublishRequest {
                repository: manifest.revision.repository.clone(),
                view_name: request.view_name,
                build_id: build.id.clone(),
                expected,
            })
            .map_err(|_| {
                self.emit(
                    LifecycleEventKind::PublicationConflict,
                    &build,
                    Some(&completed.id),
                    &prepared.metrics,
                );
                IndexError::Publication
            })?;
        let publication_is_other_build = match &publication {
            PublicationOutcome::Published { view } => view.build_id != build.id,
            PublicationOutcome::Superseded { .. } => true,
        };
        if publication_is_other_build {
            let _ = self.store.supersede_build(&build.id);
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
    let identity = SnapshotIdentity {
        version: 1,
        repository: &manifest.revision.repository,
        source_manifest_digest: &manifest.revision.manifest_digest,
        graph_model_version: GRAPH_MODEL_VERSION,
        analysis_config_digest: &manifest.revision.analysis_config_digest,
        extractor_set_digest: &manifest.extractor_set_digest,
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
mod tests {
    use std::{cell::Cell, fs, path::Path};

    use super::*;
    use crate::repository_graph::{
        config::{AnalyzerSettings, ConfigScalar},
        domain::{RepoPath, RepositoryId, RepositoryNamespace, RepositoryRef, SnapshotId},
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
        let context =
            SourceDiscoveryContext::from_config(repository(), config, &identities).unwrap();
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
                scope: QueryScope::v1(
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
        let first_warning_count = status(&sidecar, &config).diagnostics.warning;

        write(
            &repository_dir.path().join(".ferrus/project.toml"),
            b"project_id='local-only'\n",
        );
        let second_source = discover(repository_dir.path(), &config);
        let second = run(&mut sidecar, &second_source, &config, "build-2", false).unwrap();
        assert_eq!(second.snapshot.id, first.snapshot.id);
        assert!(second.reused_existing_snapshot);
        assert_eq!(
            status(&sidecar, &config).diagnostics.warning,
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
            status(&sidecar, &config).diagnostics.warning,
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
                options: BTreeMap::from([(
                    "fixture_mode".to_string(),
                    ConfigScalar::Boolean(true),
                )]),
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

    struct FailingReadSource(FilesystemRepositorySource);

    impl RepositorySource for FailingReadSource {
        type Error = SourceError;

        fn repository(&self) -> &RepositoryRef {
            self.0.repository()
        }

        fn manifest(&self) -> &SourceManifest {
            self.0.manifest()
        }

        fn read_verified(
            &self,
            _file: &SourceFileDescriptor,
        ) -> Result<SourceContent, Self::Error> {
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
}
