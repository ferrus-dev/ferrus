//! Machine-local repository graph runtime adapter shared by CLI and MCP reads.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU64,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{agent_id::ENV_AGENT_ID, project, repository_graph};
use anyhow::{Context, Result};
use repository_graph::{
    QUERY_WIRE_VERSION,
    config::RepositoryGraphConfig,
    domain::{
        Availability, BuildId, DiagnosticCode, DiagnosticLocation, DiagnosticSeverity, Digest,
        Freshness, PublishedViewName, QueryBudget, RepositoryId, RepositoryNamespace,
        RepositoryRef,
    },
    index::{IndexCoordinator, IndexRequest, active_extractor_identities},
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
    source::{
        LocalRepositorySource, LocalSnapshotContent, SourceDiscoveryContext, TaskBaselineSource,
    },
    sqlite::{
        OpenQuerySidecarResult, OpenSidecarResult, SIDECAR_FILE_NAME, open_for_build_at,
        open_for_query_at,
    },
};

pub(crate) const CANONICAL_VIEW: &str = "canonical";
static TASK_BASELINE_BUILD_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) struct LocalGraphContext {
    pub(crate) root: std::path::PathBuf,
    pub(crate) repository: RepositoryRef,
    pub(crate) config: RepositoryGraphConfig,
    pub(crate) repository_view: Option<project::RepositoryViewReference>,
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
        let repository_view = match std::env::var(ENV_AGENT_ID) {
            Ok(agent_id) if !agent_id.trim().is_empty() => {
                project::runtime_task_context_for_agent(agent_id.trim())
                    .await?
                    .map(|context| context.repository_view)
            }
            _ => None,
        };
        Ok(Self {
            root,
            repository: RepositoryRef {
                namespace: RepositoryNamespace::new(format!("local:{project_id}"))?,
                repository_id: RepositoryId::new("root")?,
            },
            config,
            repository_view,
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

    pub(crate) fn scope(&self, budget: QueryBudget) -> Result<repository_graph::query::QueryScope> {
        let snapshot = self
            .repository_view
            .as_ref()
            .and_then(|view| view.baseline_snapshot_id.clone())
            .map(SnapshotSelector::Snapshot)
            .unwrap_or_else(|| {
                SnapshotSelector::Published(
                    PublishedViewName::new(CANONICAL_VIEW)
                        .expect("canonical published view name is non-empty"),
                )
            });
        Ok(repository_graph::query::QueryScope::current(
            self.repository.clone(),
            snapshot,
            budget,
        ))
    }

    pub(crate) async fn status(&self) -> Result<StatusResponse> {
        if let Some(status) = self.unavailable_task_view_status() {
            return unavailable_status(
                self.repository.clone(),
                Availability::NotBuilt,
                status.as_str(),
                RetrievalAction::Index,
            );
        }
        let path = sidecar_path().await?;
        // Discovering the current manifest can walk and hash the repository.
        // MCP retrieval must stay latency-bounded, so without a reliable source
        // mutation token it reports freshness as unknown rather than stale data
        // as fresh. The local CLI uses freshness_comparison() for exact checks.
        status_response_at(self, &path, None)
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
        if let Some(status) = self.unavailable_task_view_status() {
            return Ok(Err(unavailable_task_view_error(status)));
        }
        let path = sidecar_path().await?;
        Ok(search_response_at(self, &path, None, request))
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
        if let Some(status) = self.unavailable_task_view_status() {
            return Ok(Err(unavailable_task_view_error(status)));
        }
        let path = sidecar_path().await?;
        Ok(context_response_at(self, &path, None, request))
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

    fn unavailable_task_view_status(&self) -> Option<project::RepositoryViewStatus> {
        self.repository_view
            .as_ref()
            .and_then(|view| view.baseline_snapshot_id.is_none().then_some(view.status))
    }
}

pub(crate) async fn sidecar_path() -> Result<std::path::PathBuf> {
    Ok(project::current_project_data_dir()
        .await?
        .join(SIDECAR_FILE_NAME))
}

pub(crate) async fn schedule_task_baseline_pin(
    task_id: &str,
    workspace_root: &Path,
    baseline_tree: Option<&str>,
) {
    let existing = project::task_repository_view(task_id).await.ok().flatten();
    let prepared = match (
        LocalGraphContext::load(false).await,
        project::current_project_data_dir().await,
    ) {
        (Ok(context), Ok(data_dir)) => Some((
            context,
            data_dir.join(SIDECAR_FILE_NAME),
            data_dir.join("ferrus.db"),
        )),
        (Err(error), _) | (_, Err(error)) => {
            let repository_view = resolved_repository_view(task_id, existing.clone(), Err(error));
            if let Err(error) =
                project::record_task_repository_view(task_id, &repository_view).await
            {
                tracing::warn!(
                    task_id,
                    error = ?error,
                    "failed to persist unavailable task repository graph baseline"
                );
            }
            None
        }
    };
    let Some((context, sidecar_path, database_path)) = prepared else {
        return;
    };
    let task_id = task_id.to_string();
    let workspace_root = workspace_root.to_path_buf();
    let baseline_tree = baseline_tree.map(str::to_string);
    tokio::spawn(async move {
        let resolved = resolve_task_baseline(
            context,
            sidecar_path,
            &task_id,
            &workspace_root,
            baseline_tree.as_deref(),
            existing.as_ref(),
        )
        .await;
        let repository_view = resolved_repository_view(&task_id, existing, resolved);
        if let Err(error) =
            project::record_task_repository_view_at(&database_path, &task_id, &repository_view)
                .await
        {
            tracing::warn!(
                task_id,
                error = ?error,
                "failed to persist task repository graph baseline; dispatch already continued"
            );
        }
    });
}

fn resolved_repository_view(
    task_id: &str,
    existing: Option<project::RepositoryViewReference>,
    resolved: Result<project::RepositoryViewReference>,
) -> project::RepositoryViewReference {
    match resolved {
        Ok(repository_view) => repository_view,
        Err(error) => {
            tracing::warn!(
                task_id,
                error = ?error,
                "failed to pin task repository graph baseline; dispatch will continue"
            );
            match existing.and_then(|view| view.baseline_snapshot_id) {
                Some(snapshot_id) => project::RepositoryViewReference::new(
                    Some(snapshot_id),
                    None,
                    project::RepositoryViewStatus::Stale,
                )
                .expect("a retained baseline snapshot is a valid stale view"),
                None => project::RepositoryViewReference::new(
                    None,
                    None,
                    project::RepositoryViewStatus::Failed,
                )
                .expect("a failed unavailable view has no overlay"),
            }
        }
    }
}

async fn resolve_task_baseline(
    context: LocalGraphContext,
    sidecar_path: std::path::PathBuf,
    task_id: &str,
    workspace_root: &Path,
    baseline_tree: Option<&str>,
    existing: Option<&project::RepositoryViewReference>,
) -> Result<project::RepositoryViewReference> {
    if let Some(existing) = existing {
        if let Some(snapshot_id) = existing.baseline_snapshot_id.as_ref() {
            let retained = match open_for_query_at(&sidecar_path) {
                Ok(OpenQuerySidecarResult::Ready(sidecar)) => sidecar
                    .snapshot(snapshot_id)
                    .context("Failed to inspect the pinned baseline snapshot")?
                    .is_some(),
                _ => false,
            };
            return project::RepositoryViewReference::new(
                Some(snapshot_id.clone()),
                existing.overlay_revision_id.clone(),
                if retained {
                    existing.status
                } else {
                    project::RepositoryViewStatus::Stale
                },
            );
        }
        if existing.status != project::RepositoryViewStatus::NotBuilt {
            return Ok(existing.clone());
        }
    }
    if !context.config.enabled || baseline_tree.is_none() {
        return project::RepositoryViewReference::new(
            None,
            None,
            project::RepositoryViewStatus::Unavailable,
        );
    }

    let baseline_tree = git_tree_digest(baseline_tree.expect("checked above"))?;
    let workspace_root = workspace_root.to_path_buf();
    let config = context.config.clone();
    let repository = context.repository.clone();
    let task_id = task_id.to_string();
    let snapshot = tokio::task::spawn_blocking(move || -> Result<_> {
        let identities = active_extractor_identities(&config)?;
        let discovery = SourceDiscoveryContext::from_config(repository, &config, &identities)?;
        let source =
            TaskBaselineSource::discover(&workspace_root, discovery, baseline_tree.clone())?;
        if workspace_tree_identity(
            &workspace_root,
            baseline_tree.value(),
            sidecar_path
                .parent()
                .context("Repository graph sidecar has no parent directory")?,
        )? != baseline_tree.value()
        {
            anyhow::bail!("managed worktree no longer matches its pinned baseline tree");
        }
        let mut sidecar = match open_for_build_at(&sidecar_path)? {
            OpenSidecarResult::Ready(sidecar) => sidecar,
            OpenSidecarResult::RequiresRebuild(reason) => anyhow::bail!(
                "repository graph sidecar schema {} is incompatible with {}",
                reason.found_schema_version,
                reason.supported_schema_version
            ),
        };
        let outcome = IndexCoordinator::new(&mut sidecar).index(
            &source,
            &config,
            IndexRequest {
                build_id: next_task_baseline_build_id(&task_id)?,
                view_name: PublishedViewName::new(format!("task-baseline:{task_id}"))?,
                force_full: false,
            },
        )?;
        Ok(outcome.snapshot.id)
    })
    .await??;

    project::RepositoryViewReference::new(
        Some(snapshot),
        None,
        project::RepositoryViewStatus::Available,
    )
}

struct TemporaryGitIndex {
    path: PathBuf,
}

impl Drop for TemporaryGitIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn workspace_tree_identity(root: &Path, baseline_tree: &str, data_dir: &Path) -> Result<String> {
    let sequence = TASK_BASELINE_BUILD_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let index_dir = data_dir.join("baseline-indexes");
    std::fs::create_dir_all(&index_dir).context("Failed to create baseline index workspace")?;
    let index = TemporaryGitIndex {
        path: index_dir.join(format!("{nanos:x}-{sequence:x}.index")),
    };
    run_git_with_index(root, &index.path, &["read-tree", baseline_tree])?;
    run_git_with_index(
        root,
        &index.path,
        &[
            "add",
            "-A",
            "--",
            ".",
            ":(exclude).ferrus",
            ":(exclude).ferrus/**",
        ],
    )?;
    let output = run_git_with_index(root, &index.path, &["write-tree"])?;
    let identity = std::str::from_utf8(&output)
        .context("Git returned a non-UTF-8 tree identity")?
        .trim();
    git_tree_digest(identity)?;
    Ok(identity.to_string())
}

fn run_git_with_index(root: &Path, index: &Path, arguments: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .env("GIT_INDEX_FILE", index)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .context("Failed to inspect the managed worktree baseline")?;
    if !output.status.success() {
        anyhow::bail!("Git could not verify the managed worktree baseline");
    }
    Ok(output.stdout)
}

fn git_tree_digest(value: &str) -> Result<Digest> {
    let value = value.trim();
    let algorithm = match value.len() {
        40 => "git-tree-sha1",
        64 => "git-tree-sha256",
        _ => anyhow::bail!("Pinned baseline tree has an unsupported identity"),
    };
    Digest::new(algorithm, value).context("Pinned baseline tree is not canonical hexadecimal")
}

fn next_task_baseline_build_id(task_id: &str) -> Result<BuildId> {
    let sequence = TASK_BASELINE_BUILD_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(BuildId::new(format!(
        "task-baseline:{task_id}:{nanos:x}:{sequence:x}"
    ))?)
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

fn unavailable_task_view_error(status: project::RepositoryViewStatus) -> QueryError {
    query_error(
        QueryErrorCode::NotBuilt,
        "repository graph is unavailable for the current task baseline; inspect source directly",
        matches!(status, project::RepositoryViewStatus::Stale),
        Some(RetrievalAction::Index),
    )
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
            repository_view: None,
        }
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
    fn mcp_runtime_does_not_label_changed_source_as_fresh_without_revalidation() {
        let directory = tempfile::tempdir().unwrap();
        let sidecar = directory.path().join(SIDECAR_FILE_NAME);
        let (context, request) = indexed_context(directory.path(), &sidecar);
        std::fs::write(
            directory.path().join("src/lib.rs"),
            b"pub struct ChangedAfterIndex;\n",
        )
        .unwrap();

        let response = context_response_at(&context, &sidecar, None, &request).unwrap();

        assert_eq!(response.freshness.freshness, Freshness::Unknown);
        assert_eq!(response.freshness.reason_codes, ["source_not_compared"]);
    }

    #[test]
    fn task_scope_remains_pinned_when_canonical_publication_advances() {
        let directory = tempfile::tempdir().unwrap();
        let sidecar_path = directory.path().join(SIDECAR_FILE_NAME);
        let source_root = directory.path().join("repository");
        std::fs::create_dir(&source_root).unwrap();
        let (mut context, _) = indexed_context(&source_root, &sidecar_path);
        let sidecar = match open_for_query_at(&sidecar_path).unwrap() {
            OpenQuerySidecarResult::Ready(sidecar) => sidecar,
            _ => panic!("indexed sidecar must be queryable"),
        };
        let baseline = sidecar
            .published_view(
                &context.repository,
                &PublishedViewName::new(CANONICAL_VIEW).unwrap(),
            )
            .unwrap()
            .unwrap()
            .snapshot_id;
        drop(sidecar);
        context.repository_view = Some(
            project::RepositoryViewReference::new(
                Some(baseline.clone()),
                None,
                project::RepositoryViewStatus::Available,
            )
            .unwrap(),
        );

        std::fs::write(
            source_root.join("src/lib.rs"),
            b"pub struct CanonicalAdvanced;\n",
        )
        .unwrap();
        let source = context.discover().unwrap();
        let mut sidecar = match open_for_build_at(&sidecar_path).unwrap() {
            OpenSidecarResult::Ready(sidecar) => sidecar,
            OpenSidecarResult::RequiresRebuild(_) => panic!("sidecar unexpectedly incompatible"),
        };
        let advanced = IndexCoordinator::new(&mut sidecar)
            .index(
                &source,
                &context.config,
                IndexRequest {
                    build_id: BuildId::new("build-advanced").unwrap(),
                    view_name: PublishedViewName::new(CANONICAL_VIEW).unwrap(),
                    force_full: false,
                },
            )
            .unwrap();
        assert_ne!(advanced.snapshot.id, baseline);
        drop(sidecar);

        let request = SearchRequest {
            scope: context
                .scope(default_budget(&context.config.query_limits).unwrap())
                .unwrap(),
            text: "RuntimeTaskContext".to_string(),
            node_kinds: vec![],
            paths: vec![],
            page: repository_graph::query::PageRequest { cursor: None },
        };
        let response = search_response_at(&context, &sidecar_path, None, &request).unwrap();

        assert_eq!(response.snapshot_id, baseline);
        assert!(response.data.hits.iter().any(|hit| {
            hit.semantic_key
                .as_ref()
                .is_some_and(|key| key.as_str().contains("RuntimeTaskContext"))
        }));
    }

    #[tokio::test]
    async fn dispatch_pins_git_baseline_without_changing_task_lifecycle() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(root.join(".ferrus/projects/test-project")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("ferrus.toml"),
            "[repository_graph]\nenabled = true\n",
        )
        .unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub struct BaselineSymbol;\n").unwrap();
        let data_dir = root.join(".ferrus/projects/test-project");
        std::fs::write(
            root.join(".ferrus/project.toml"),
            toml::to_string(&project::LocalProjectRef {
                project_id: "test-project".to_string(),
                name: "test".to_string(),
                data_dir: data_dir.to_string_lossy().into_owned(),
            })
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            data_dir.join("project.toml"),
            toml::to_string(&project::ProjectMetadata {
                id: "test-project".to_string(),
                name: "test".to_string(),
                workspace_dir: root.to_string_lossy().into_owned(),
                ferrus_dir: root.join(".ferrus").to_string_lossy().into_owned(),
                vcs: Some("git".to_string()),
                origin_repo: None,
                default_branch: Some("main".to_string()),
                current_head: None,
                created_at: "2026-07-22T00:00:00Z".to_string(),
                last_opened_at: "2026-07-22T00:00:00Z".to_string(),
                version: 1,
            })
            .unwrap(),
        )
        .unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git command failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        git(&["init"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Ferrus Test"]);
        git(&["config", "commit.gpgsign", "false"]);
        git(&["add", "ferrus.toml", "src/lib.rs"]);
        git(&["commit", "-m", "baseline"]);
        let baseline_tree = git(&["rev-parse", "HEAD^{tree}"]);

        std::env::set_current_dir(root).unwrap();
        project::record_task_status(
            "t-001",
            ".ferrus/tasks/t-001.md",
            project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        schedule_task_baseline_pin("t-001", root, Some(&baseline_tree)).await;
        for _ in 0..100 {
            if project::task_repository_view("t-001")
                .await
                .unwrap()
                .is_some_and(|view| view.status == project::RepositoryViewStatus::Available)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let repository_view = project::task_repository_view("t-001")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            repository_view.status,
            project::RepositoryViewStatus::Available
        );
        assert!(repository_view.baseline_snapshot_id.is_some());
        assert_eq!(
            project::list_tasks().await.unwrap()[0].status,
            project::TaskStatus::Executing.as_str()
        );

        project::record_task_status(
            "t-002",
            ".ferrus/tasks/t-002.md",
            project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        schedule_task_baseline_pin("t-002", root, Some("invalid-tree")).await;
        for _ in 0..100 {
            if project::task_repository_view("t-002")
                .await
                .unwrap()
                .is_some_and(|view| view.status == project::RepositoryViewStatus::Failed)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            project::task_repository_view("t-002")
                .await
                .unwrap()
                .unwrap()
                .status,
            project::RepositoryViewStatus::Failed
        );
        assert_eq!(
            project::list_tasks()
                .await
                .unwrap()
                .into_iter()
                .find(|task| task.id == "t-002")
                .unwrap()
                .status,
            project::TaskStatus::Executing.as_str()
        );

        std::fs::write(root.join("src/lib.rs"), "pub struct AgentEdit;\n").unwrap();
        project::record_task_status(
            "t-003",
            ".ferrus/tasks/t-003.md",
            project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        schedule_task_baseline_pin("t-003", root, Some(&baseline_tree)).await;
        for _ in 0..100 {
            if project::task_repository_view("t-003")
                .await
                .unwrap()
                .is_some_and(|view| view.status == project::RepositoryViewStatus::Failed)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let changed_worktree_view = project::task_repository_view("t-003")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            changed_worktree_view.status,
            project::RepositoryViewStatus::Failed
        );
        assert!(changed_worktree_view.baseline_snapshot_id.is_none());
        assert_eq!(
            project::list_tasks()
                .await
                .unwrap()
                .into_iter()
                .find(|task| task.id == "t-003")
                .unwrap()
                .status,
            project::TaskStatus::Executing.as_str()
        );
        std::env::set_current_dir(previous).unwrap();
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
