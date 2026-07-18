//! Machine-local repository graph runtime adapter shared by CLI and MCP reads.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU64,
    path::Path,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use crate::{project, repository_graph};
use anyhow::{Context, Result};
use repository_graph::{
    QUERY_WIRE_VERSION,
    config::RepositoryGraphConfig,
    domain::{
        Availability, DiagnosticCode, DiagnosticLocation, DiagnosticSeverity, Freshness,
        PublishedViewName, QueryBudget, RepositoryId, RepositoryNamespace, RepositoryRef,
    },
    index::active_extractor_identities,
    ports::{GraphQuery, SnapshotContent},
    query::{
        ContentRequest, ContextRequest, ContextResponse, ContextSnippet, DiagnosticSummary,
        DiagnosticsEnvelope, FreshnessEnvelope, PageInfo, QueryDiagnostic, QueryError,
        QueryErrorCode, RetrievalAction, SearchRequest, SearchResponse, SnapshotSelector,
        StatusData, StatusRequest, StatusResponse,
    },
    query_sqlite::{
        FreshnessComparison, SqliteGraphQuery, default_budget, snapshot_file_descriptors,
    },
    source::{LocalRepositorySource, LocalSnapshotContent, SourceDiscoveryContext},
    sqlite::{OpenQuerySidecarResult, SIDECAR_FILE_NAME, open_for_query_at},
};

pub(crate) const CANONICAL_VIEW: &str = "canonical";
const FRESHNESS_CACHE_TTL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct FreshnessCacheKey {
    root: std::path::PathBuf,
    analysis_config_digest: String,
}

#[derive(Default)]
struct FreshnessCacheEntry {
    comparison: Option<FreshnessComparison>,
    refreshed_at: Option<Instant>,
    refresh_in_flight: bool,
}

#[derive(Default)]
struct FreshnessCache {
    entries: BTreeMap<FreshnessCacheKey, FreshnessCacheEntry>,
}

impl FreshnessCache {
    fn cached_or_begin_refresh(
        &mut self,
        key: FreshnessCacheKey,
        now: Instant,
    ) -> (Option<FreshnessComparison>, bool) {
        let entry = self.entries.entry(key).or_default();
        let expired = entry.refreshed_at.is_none_or(|refreshed_at| {
            now.saturating_duration_since(refreshed_at) >= FRESHNESS_CACHE_TTL
        });
        if expired {
            if !entry.refresh_in_flight {
                entry.refresh_in_flight = true;
                return (None, true);
            }
            return (None, false);
        }
        (entry.comparison.clone(), false)
    }

    fn complete(
        &mut self,
        key: FreshnessCacheKey,
        comparison: Option<FreshnessComparison>,
        now: Instant,
    ) {
        let entry = self.entries.entry(key).or_default();
        entry.comparison = comparison;
        entry.refreshed_at = Some(now);
        entry.refresh_in_flight = false;
    }
}

fn freshness_cache() -> &'static Mutex<FreshnessCache> {
    static CACHE: OnceLock<Mutex<FreshnessCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(FreshnessCache::default()))
}

fn with_freshness_cache<T>(operation: impl FnOnce(&mut FreshnessCache) -> T) -> T {
    let mut cache = freshness_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    operation(&mut cache)
}

#[derive(Clone)]
pub(crate) struct LocalGraphContext {
    pub(crate) root: std::path::PathBuf,
    pub(crate) repository: RepositoryRef,
    pub(crate) config: RepositoryGraphConfig,
}

impl LocalGraphContext {
    pub(crate) async fn load(require_enabled: bool) -> Result<Self> {
        let root = project::canonical_project_root().await?;
        let contents = tokio::fs::read_to_string(root.join("ferrus.toml"))
            .await
            .context("ferrus.toml not found — run ferrus init first")?;
        let config = RepositoryGraphConfig::from_ferrus_toml(&contents)
            .context("Invalid [repository_graph] configuration")?;
        if require_enabled && !config.enabled {
            anyhow::bail!(
                "repository graph is disabled; set repository_graph.enabled = true in ferrus.toml"
            );
        }
        let project_id = project::current_project_id().await?;
        Ok(Self {
            root,
            repository: RepositoryRef {
                namespace: RepositoryNamespace::new(format!("local:{project_id}"))?,
                repository_id: RepositoryId::new("root")?,
            },
            config,
        })
    }

    pub(crate) fn discover(&self) -> Result<LocalRepositorySource> {
        let identities = active_extractor_identities(&self.config)?;
        let context = SourceDiscoveryContext::from_config(
            self.repository.clone(),
            &self.config,
            &identities,
        )?;
        LocalRepositorySource::discover(&self.root, context)
            .context("Failed to discover the canonical repository source")
    }

    pub(crate) fn freshness_comparison(&self) -> Result<Option<FreshnessComparison>> {
        if !self.config.enabled {
            return Ok(None);
        }
        let source = self.discover()?;
        Ok(Some(FreshnessComparison::from_manifest(source.manifest())))
    }

    fn cached_freshness_comparison(&self) -> Option<FreshnessComparison> {
        if !self.config.enabled {
            return None;
        }
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        let key = FreshnessCacheKey {
            root: self.root.clone(),
            analysis_config_digest: self
                .config
                .analysis_config_digest()
                .ok()?
                .value()
                .to_string(),
        };
        let (comparison, refresh) = with_freshness_cache(|cache| {
            cache.cached_or_begin_refresh(key.clone(), Instant::now())
        });
        if refresh {
            let context = self.clone();
            runtime.spawn_blocking(move || {
                let comparison = context
                    .discover()
                    .ok()
                    .map(|source| FreshnessComparison::from_manifest(source.manifest()));
                with_freshness_cache(|cache| cache.complete(key, comparison, Instant::now()));
            });
        }
        comparison
    }

    pub(crate) fn scope(&self, budget: QueryBudget) -> Result<repository_graph::query::QueryScope> {
        Ok(repository_graph::query::QueryScope::current(
            self.repository.clone(),
            SnapshotSelector::Published(PublishedViewName::new(CANONICAL_VIEW)?),
            budget,
        ))
    }

    pub(crate) async fn status(&self) -> Result<StatusResponse> {
        let path = sidecar_path().await?;
        let comparison = self.cached_freshness_comparison();
        status_response_at(self, &path, comparison)
    }

    pub(crate) async fn search(
        &self,
        request: &SearchRequest,
    ) -> Result<Result<SearchResponse, QueryError>> {
        if !self.config.enabled {
            return Ok(Err(query_error(
                QueryErrorCode::InvalidRequest,
                "repository graph is disabled; enable it before searching",
                false,
                None,
            )));
        }
        let path = sidecar_path().await?;
        let comparison = self.cached_freshness_comparison();
        Ok(search_response_at(self, &path, comparison, request))
    }

    pub(crate) async fn context(
        &self,
        request: &ContextRequest,
    ) -> Result<Result<ContextResponse, QueryError>> {
        if !self.config.enabled {
            return Ok(Err(query_error(
                QueryErrorCode::InvalidRequest,
                "repository graph is disabled; enable it before assembling context",
                false,
                None,
            )));
        }
        let path = sidecar_path().await?;
        let comparison = self.cached_freshness_comparison();
        Ok(context_response_at(self, &path, comparison, request))
    }

    pub(crate) async fn context_with_snippets(
        &self,
        request: &ContextRequest,
        requested_snippet_bytes: NonZeroU64,
    ) -> Result<Result<ContextResponse, QueryError>> {
        let response = match self.context(request).await? {
            Ok(response) => response,
            Err(error) => return Ok(Err(error)),
        };
        let path = sidecar_path().await?;
        Ok(attach_snippets_at(
            self,
            &path,
            request,
            response,
            requested_snippet_bytes,
        ))
    }
}

pub(crate) async fn sidecar_path() -> Result<std::path::PathBuf> {
    Ok(project::current_project_data_dir()
        .await?
        .join(SIDECAR_FILE_NAME))
}

pub(crate) fn status_response_at(
    context: &LocalGraphContext,
    sidecar_path: &Path,
    freshness_comparison: Option<FreshnessComparison>,
) -> Result<StatusResponse> {
    match open_for_query_at(sidecar_path) {
        Ok(OpenQuerySidecarResult::Ready(sidecar)) => {
            let query = SqliteGraphQuery::new(
                &sidecar,
                context.config.query_limits.clone(),
                freshness_comparison,
            );
            Ok(query.status(&StatusRequest {
                scope: context.scope(default_budget(&context.config.query_limits)?)?,
            })?)
        }
        Ok(OpenQuerySidecarResult::Absent) => unavailable_status(
            context.repository.clone(),
            Availability::NotBuilt,
            "not_built",
            RetrievalAction::Index,
        ),
        Ok(OpenQuerySidecarResult::NeedsMigration {
            found_schema_version,
        }) => unavailable_status(
            context.repository.clone(),
            Availability::Incompatible,
            &format!("schema_{found_schema_version}_needs_migration"),
            RetrievalAction::Index,
        ),
        Ok(OpenQuerySidecarResult::RequiresRebuild(_)) => unavailable_status(
            context.repository.clone(),
            Availability::Incompatible,
            "incompatible_schema",
            RetrievalAction::Rebuild,
        ),
        Err(_) => unavailable_status(
            context.repository.clone(),
            Availability::Incompatible,
            "sidecar_unreadable",
            RetrievalAction::Rebuild,
        ),
    }
}

fn search_response_at(
    context: &LocalGraphContext,
    sidecar_path: &Path,
    freshness_comparison: Option<FreshnessComparison>,
    request: &SearchRequest,
) -> Result<SearchResponse, QueryError> {
    match open_for_query_at(sidecar_path) {
        Ok(OpenQuerySidecarResult::Ready(sidecar)) => SqliteGraphQuery::new(
            &sidecar,
            context.config.query_limits.clone(),
            freshness_comparison,
        )
        .search(request),
        Ok(OpenQuerySidecarResult::Absent) => Err(query_error(
            QueryErrorCode::NotBuilt,
            "repository graph is not built; run `ferrus graph index`",
            false,
            Some(RetrievalAction::Index),
        )),
        Ok(OpenQuerySidecarResult::NeedsMigration { .. }) => Err(query_error(
            QueryErrorCode::Incompatible,
            "repository graph storage needs migration; run `ferrus graph index`",
            false,
            Some(RetrievalAction::Index),
        )),
        Ok(OpenQuerySidecarResult::RequiresRebuild(_)) => Err(query_error(
            QueryErrorCode::Incompatible,
            "repository graph storage is incompatible; rebuild the derived index",
            false,
            Some(RetrievalAction::Rebuild),
        )),
        Err(_) => Err(query_error(
            QueryErrorCode::BackendUnavailable,
            "repository graph storage is unavailable or inconsistent",
            true,
            Some(RetrievalAction::Rebuild),
        )),
    }
}

fn context_response_at(
    context: &LocalGraphContext,
    sidecar_path: &Path,
    freshness_comparison: Option<FreshnessComparison>,
    request: &ContextRequest,
) -> Result<ContextResponse, QueryError> {
    match open_for_query_at(sidecar_path) {
        Ok(OpenQuerySidecarResult::Ready(sidecar)) => SqliteGraphQuery::new(
            &sidecar,
            context.config.query_limits.clone(),
            freshness_comparison,
        )
        .context(request),
        Ok(OpenQuerySidecarResult::Absent) => Err(query_error(
            QueryErrorCode::NotBuilt,
            "repository graph is not built; run `ferrus graph index`",
            false,
            Some(RetrievalAction::Index),
        )),
        Ok(OpenQuerySidecarResult::NeedsMigration { .. }) => Err(query_error(
            QueryErrorCode::Incompatible,
            "repository graph storage needs migration; run `ferrus graph index`",
            false,
            Some(RetrievalAction::Index),
        )),
        Ok(OpenQuerySidecarResult::RequiresRebuild(_)) => Err(query_error(
            QueryErrorCode::Incompatible,
            "repository graph storage is incompatible; rebuild the derived index",
            false,
            Some(RetrievalAction::Rebuild),
        )),
        Err(_) => Err(query_error(
            QueryErrorCode::BackendUnavailable,
            "repository graph storage is unavailable or inconsistent",
            true,
            Some(RetrievalAction::Rebuild),
        )),
    }
}

fn attach_snippets_at(
    context: &LocalGraphContext,
    sidecar_path: &Path,
    request: &ContextRequest,
    mut response: ContextResponse,
    requested_snippet_bytes: NonZeroU64,
) -> Result<ContextResponse, QueryError> {
    let hard_limit =
        NonZeroU64::new(context.config.query_limits.max_snippet_bytes).ok_or_else(|| {
            query_error(
                QueryErrorCode::InvalidRequest,
                "repository_graph.query_limits.max_snippet_bytes must be greater than zero",
                false,
                None,
            )
        })?;
    let total_limit = requested_snippet_bytes.get().min(hard_limit.get());
    let max_diagnostics = request
        .scope
        .budget
        .max_diagnostics
        .get()
        .min(context.config.query_limits.max_diagnostics) as usize;
    let sidecar = match open_for_query_at(sidecar_path) {
        Ok(OpenQuerySidecarResult::Ready(sidecar)) => sidecar,
        _ => {
            return Err(query_error(
                QueryErrorCode::ContentUnavailable,
                "repository content metadata became unavailable after context assembly",
                true,
                None,
            ));
        }
    };

    let paths = response
        .data
        .items
        .iter()
        .map(|item| item.path.clone())
        .collect::<BTreeSet<_>>();
    let files = snapshot_file_descriptors(&sidecar, &response.snapshot_id, &paths)?;

    let content = LocalSnapshotContent::new(
        &context.root,
        context.repository.clone(),
        response.snapshot_id.clone(),
        &context.config.source,
        files,
        hard_limit,
    )
    .map_err(|_| {
        query_error(
            QueryErrorCode::ContentUnavailable,
            "repository content boundary could not be initialized",
            true,
            None,
        )
    })?;

    let evidence = response
        .data
        .items
        .iter()
        .map(|item| {
            (
                item.path.clone(),
                item.span.clone(),
                item.content_identity.clone(),
            )
        })
        .collect::<Vec<_>>();
    let mut seen = BTreeSet::new();
    let mut remaining = total_limit;
    let mut omitted_for_budget = false;
    for (path, span, content_identity) in evidence {
        let key = serde_json::to_string(&(path.clone(), span.clone(), content_identity.clone()))
            .expect("context evidence is always serializable");
        if !seen.insert(key) {
            continue;
        }
        let Some(max_bytes) = NonZeroU64::new(remaining) else {
            omitted_for_budget = true;
            break;
        };
        match content.read_verified(&ContentRequest {
            wire_version: QUERY_WIRE_VERSION,
            repository: response.repository.clone(),
            snapshot_id: response.snapshot_id.clone(),
            path: path.clone(),
            expected_content_identity: content_identity,
            span: span.clone(),
            max_bytes,
        }) {
            Ok(snippet) => match String::from_utf8(snippet.bytes) {
                Ok(text) => {
                    remaining = remaining.saturating_sub(text.len() as u64);
                    response.data.snippets.push(ContextSnippet {
                        path,
                        span,
                        verified_content_identity: snippet.verified_content_identity,
                        text,
                        truncated: snippet.truncated,
                    });
                    if snippet.truncated {
                        omitted_for_budget = true;
                    }
                }
                Err(_) => add_content_diagnostic(
                    &mut response,
                    max_diagnostics,
                    "content.non_utf8",
                    path,
                    span,
                ),
            },
            Err(error) => add_content_diagnostic(
                &mut response,
                max_diagnostics,
                match error.code {
                    QueryErrorCode::ContentChanged => "content.changed",
                    _ => "content.unavailable",
                },
                path,
                span,
            ),
        }
    }
    if omitted_for_budget {
        add_content_diagnostic_without_location(
            &mut response,
            max_diagnostics,
            "content.snippets_truncated",
        );
    }
    Ok(response)
}

fn add_content_diagnostic(
    response: &mut ContextResponse,
    max_diagnostics: usize,
    code: &str,
    path: repository_graph::domain::RepoPath,
    span: Option<repository_graph::domain::SourceSpan>,
) {
    add_bounded_content_diagnostic(
        response,
        max_diagnostics,
        code,
        Some(DiagnosticLocation { path, span }),
    );
}

fn add_content_diagnostic_without_location(
    response: &mut ContextResponse,
    max_diagnostics: usize,
    code: &str,
) {
    add_bounded_content_diagnostic(response, max_diagnostics, code, None);
}

fn add_bounded_content_diagnostic(
    response: &mut ContextResponse,
    max_diagnostics: usize,
    code: &str,
    location: Option<DiagnosticLocation>,
) {
    response.diagnostics.summary.warning += 1;
    if response.diagnostics.items.len() < max_diagnostics {
        response.diagnostics.items.push(QueryDiagnostic {
            severity: DiagnosticSeverity::Warning,
            code: DiagnosticCode::new(code).expect("static content diagnostic code is canonical"),
            location,
        });
    } else {
        response.diagnostics.truncated = true;
    }
}

fn unavailable_status(
    repository: RepositoryRef,
    availability: Availability,
    reason: &str,
    action: RetrievalAction,
) -> Result<StatusResponse> {
    Ok(StatusResponse {
        wire_version: QUERY_WIRE_VERSION,
        repository,
        snapshot_id: None,
        source_revision: None,
        freshness: FreshnessEnvelope {
            freshness: Freshness::NotApplicable,
            compared_manifest: None,
            reason_codes: vec![reason.to_string()],
        },
        diagnostics: DiagnosticsEnvelope {
            summary: DiagnosticSummary::default(),
            items: vec![],
            truncated: false,
        },
        page: PageInfo {
            next_cursor: None,
            truncation: None,
        },
        data: StatusData {
            availability,
            build_state: None,
            build_id: None,
            published_view: Some(PublishedViewName::new(CANONICAL_VIEW)?),
            graph_model_version: None,
            statistics: None,
            recommended_action: Some(action),
        },
    })
}

fn query_error(
    code: QueryErrorCode,
    message: &str,
    retryable: bool,
    recommended_action: Option<RetrievalAction>,
) -> QueryError {
    QueryError {
        wire_version: QUERY_WIRE_VERSION,
        code,
        message: message.to_string(),
        retryable,
        recommended_action,
        details: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn indexed_context(root: &Path, sidecar_path: &Path) -> (LocalGraphContext, ContextRequest) {
        let mut context = context(root);
        context.config.enabled = true;
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), b"pub struct RuntimeTaskContext;\n").unwrap();
        let source = context.discover().unwrap();
        let mut sidecar = match repository_graph::sqlite::open_for_build_at(sidecar_path).unwrap() {
            repository_graph::sqlite::OpenSidecarResult::Ready(sidecar) => sidecar,
            repository_graph::sqlite::OpenSidecarResult::RequiresRebuild(_) => {
                panic!("new sidecar unexpectedly requires rebuild")
            }
        };
        repository_graph::index::IndexCoordinator::new(&mut sidecar)
            .index(
                &source,
                &context.config,
                repository_graph::index::IndexRequest {
                    build_id: repository_graph::domain::BuildId::new("build-1").unwrap(),
                    view_name: PublishedViewName::new(CANONICAL_VIEW).unwrap(),
                    force_full: false,
                },
            )
            .unwrap();
        drop(sidecar);
        let request = ContextRequest {
            scope: context
                .scope(default_budget(&context.config.query_limits).unwrap())
                .unwrap(),
            seeds: vec![repository_graph::query::ContextSeed::Path(
                repository_graph::domain::RepoPath::new("src/lib.rs").unwrap(),
            )],
            policy: repository_graph::query::ContextPolicy {
                direction: repository_graph::query::EdgeDirection::Both,
                edge_kinds: vec![],
                include_unresolved: false,
                include_external: false,
            },
            page: repository_graph::query::PageRequest { cursor: None },
        };
        (context, request)
    }

    fn context(root: &Path) -> LocalGraphContext {
        LocalGraphContext {
            root: root.to_path_buf(),
            repository: RepositoryRef {
                namespace: RepositoryNamespace::new("local:test").unwrap(),
                repository_id: RepositoryId::new("root").unwrap(),
            },
            config: RepositoryGraphConfig::default(),
        }
    }

    #[test]
    fn freshness_cache_never_returns_an_expired_comparison() {
        let directory = tempfile::tempdir().unwrap();
        let context = context(directory.path());
        let source = context.discover().unwrap();
        let comparison = FreshnessComparison::from_manifest(source.manifest());
        let key = FreshnessCacheKey {
            root: directory.path().to_path_buf(),
            analysis_config_digest: "test-config".to_string(),
        };
        let now = Instant::now();
        let mut cache = FreshnessCache::default();

        let (initial, starts_refresh) = cache.cached_or_begin_refresh(key.clone(), now);
        assert!(initial.is_none());
        assert!(starts_refresh);
        let (while_refreshing, starts_refresh) = cache.cached_or_begin_refresh(key.clone(), now);
        assert!(while_refreshing.is_none());
        assert!(!starts_refresh);

        cache.complete(key.clone(), Some(comparison.clone()), now);
        let (cached, starts_refresh) = cache.cached_or_begin_refresh(key.clone(), now);
        assert_eq!(cached, Some(comparison));
        assert!(!starts_refresh);

        let (expired, starts_refresh) =
            cache.cached_or_begin_refresh(key, now + FRESHNESS_CACHE_TTL);
        assert!(expired.is_none());
        assert!(starts_refresh);
    }

    #[test]
    fn absent_status_search_and_context_are_read_only_and_actionable() {
        let directory = tempfile::tempdir().unwrap();
        let sidecar = directory.path().join(SIDECAR_FILE_NAME);
        let context = context(directory.path());

        let status = status_response_at(&context, &sidecar, None).unwrap();
        let search = search_response_at(
            &context,
            &sidecar,
            None,
            &SearchRequest {
                scope: context
                    .scope(default_budget(&context.config.query_limits).unwrap())
                    .unwrap(),
                text: "RuntimeTaskContext".to_string(),
                node_kinds: vec![],
                paths: vec![],
                page: repository_graph::query::PageRequest { cursor: None },
            },
        )
        .unwrap_err();
        let context_response = context_response_at(
            &context,
            &sidecar,
            None,
            &ContextRequest {
                scope: context
                    .scope(default_budget(&context.config.query_limits).unwrap())
                    .unwrap(),
                seeds: vec![repository_graph::query::ContextSeed::Path(
                    repository_graph::domain::RepoPath::new("src/lib.rs").unwrap(),
                )],
                policy: repository_graph::query::ContextPolicy {
                    direction: repository_graph::query::EdgeDirection::Both,
                    edge_kinds: vec![],
                    include_unresolved: false,
                    include_external: false,
                },
                page: repository_graph::query::PageRequest { cursor: None },
            },
        )
        .unwrap_err();

        assert_eq!(status.data.availability, Availability::NotBuilt);
        assert_eq!(status.data.recommended_action, Some(RetrievalAction::Index));
        assert_eq!(search.code, QueryErrorCode::NotBuilt);
        assert_eq!(search.recommended_action, Some(RetrievalAction::Index));
        assert_eq!(context_response.code, QueryErrorCode::NotBuilt);
        assert_eq!(
            context_response.recommended_action,
            Some(RetrievalAction::Index)
        );
        assert!(!sidecar.exists());
    }

    #[test]
    fn unreadable_sidecar_is_distinct_from_a_missing_index() {
        let directory = tempfile::tempdir().unwrap();
        let sidecar = directory.path().join(SIDECAR_FILE_NAME);
        std::fs::write(&sidecar, b"not a sqlite database").unwrap();
        let context = context(directory.path());

        let status = status_response_at(&context, &sidecar, None).unwrap();

        assert_eq!(status.data.availability, Availability::Incompatible);
        assert_eq!(status.freshness.reason_codes, ["sidecar_unreadable"]);
        assert_eq!(
            status.data.recommended_action,
            Some(RetrievalAction::Rebuild)
        );
    }

    #[test]
    fn context_snippets_are_deduplicated_hash_verified_and_stale_safe() {
        let directory = tempfile::tempdir().unwrap();
        let sidecar = directory.path().join(SIDECAR_FILE_NAME);
        let (mut context, request) = indexed_context(directory.path(), &sidecar);
        let response = context_response_at(&context, &sidecar, None, &request).unwrap();

        let enriched = attach_snippets_at(
            &context,
            &sidecar,
            &request,
            response.clone(),
            NonZeroU64::new(1024).unwrap(),
        )
        .unwrap();
        assert!(!enriched.data.snippets.is_empty());
        assert_eq!(enriched.data.items, response.data.items);
        assert_eq!(enriched.page, response.page);
        assert!(
            enriched
                .data
                .snippets
                .iter()
                .all(|snippet| snippet.text.contains("RuntimeTaskContext"))
        );
        let unique = enriched
            .data
            .snippets
            .iter()
            .map(|snippet| serde_json::to_string(&(snippet.path.clone(), snippet.span.clone())))
            .collect::<Result<BTreeSet<_>, _>>()
            .unwrap();
        assert_eq!(unique.len(), enriched.data.snippets.len());

        std::fs::write(
            directory.path().join("src/lib.rs"),
            b"pub struct Changed;\n",
        )
        .unwrap();
        context.config.query_limits.max_diagnostics = 1;
        let stale = attach_snippets_at(
            &context,
            &sidecar,
            &request,
            response,
            NonZeroU64::new(1024).unwrap(),
        )
        .unwrap();
        assert!(stale.data.snippets.is_empty());
        assert_eq!(stale.diagnostics.items.len(), 1);
        assert!(stale.diagnostics.summary.warning > 1);
        assert!(stale.diagnostics.truncated);
        assert!(
            stale
                .diagnostics
                .items
                .iter()
                .any(|diagnostic| diagnostic.code.as_str() == "content.changed")
        );
    }
}
