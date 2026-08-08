//! Machine-local repository graph runtime adapter shared by CLI and MCP reads.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU64,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    agent_id::{ENV_AGENT_ID, ENV_BASELINE_TREE, ENV_TASK_ID},
    project, repository_graph,
};
use anyhow::{Context, Result};
use repository_graph::{
    QUERY_WIRE_VERSION,
    config::RepositoryGraphConfig,
    domain::{
        Availability, BuildId, DiagnosticCode, DiagnosticLocation, DiagnosticSeverity, Digest,
        Freshness, PublishedViewName, QueryBudget, RepositoryId, RepositoryNamespace,
        RepositoryRef, TaskViewId, TaskViewLifecycle, WorkspaceRef,
    },
    index::{IndexCoordinator, IndexRequest, active_extractor_identities},
    maintenance::{GraphMaintenanceReport, RefreshLeaseOutcome, RetentionProtection},
    ports::{GraphQuery, RepositorySource, SnapshotContent},
    query::{
        ContentRequest, ContextRequest, ContextResponse, ContextSnippet, DiagnosticSummary,
        DiagnosticsEnvelope, FreshnessEnvelope, PageInfo, QueryDiagnostic, QueryError,
        QueryErrorCode, QueryResponse, RetrievalAction, RetrievalFallback, SearchRequest,
        SearchResponse, SnapshotSelector, StatusData, StatusRequest, StatusResponse,
        TaskViewEnvelope, TaskViewStatus,
    },
    query_sqlite::{
        FreshnessComparison, SqliteGraphQuery, all_snapshot_file_descriptors, default_budget,
        snapshot_file_descriptors,
    },
    source::{
        GitTreeSnapshotContent, LocalRepositorySource, LocalSnapshotContent,
        SourceDiscoveryContext, TaskBaselineSource, TaskOverlaySource, capture_worktree_tree,
        parse_git_tree_digest, pin_submitted_tree, release_submitted_tree_pin,
    },
    sqlite::{
        OpenQuerySidecarResult, OpenSidecarResult, SIDECAR_FILE_NAME, open_for_build_at,
        open_for_query_at,
    },
};

pub(crate) const CANONICAL_VIEW: &str = "canonical";
static TASK_BASELINE_BUILD_COUNTER: AtomicU64 = AtomicU64::new(0);
static TASK_OVERLAY_BUILD_COUNTER: AtomicU64 = AtomicU64::new(0);
static CANONICAL_REFRESH_BUILD_COUNTER: AtomicU64 = AtomicU64::new(0);
pub(crate) const REFRESH_LEASE_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, thiserror::Error)]
#[error("repository graph refresh is already in progress for this view")]
struct RefreshAlreadyInProgress;

pub(crate) struct LocalGraphContext {
    pub(crate) project_root: std::path::PathBuf,
    pub(crate) root: std::path::PathBuf,
    pub(crate) repository: RepositoryRef,
    pub(crate) config: RepositoryGraphConfig,
    pub(crate) repository_view: Option<project::RepositoryViewReference>,
    pub(crate) task_view_id: Option<TaskViewId>,
    pub(crate) run_id: Option<String>,
}

impl LocalGraphContext {
    pub(crate) async fn load(require_enabled: bool) -> Result<Self> {
        let agent_id = std::env::var(ENV_AGENT_ID)
            .ok()
            .filter(|value| !value.trim().is_empty());
        Self::load_with_agent(require_enabled, agent_id.as_deref()).await
    }

    pub(crate) async fn load_for_agent(require_enabled: bool, agent_id: &str) -> Result<Self> {
        Self::load_with_agent(require_enabled, Some(agent_id)).await
    }

    async fn load_with_agent(require_enabled: bool, agent_id: Option<&str>) -> Result<Self> {
        let root = project::canonical_project_root().await?;
        let contents = tokio::fs::read_to_string(root.join("ferrus.toml"))
            .await
            .context("ferrus.toml not found -- run ferrus init first")?;
        let config = RepositoryGraphConfig::from_ferrus_toml(&contents)
            .context("Invalid [repository_graph] configuration")?;
        if require_enabled && !config.enabled {
            anyhow::bail!(
                "repository graph is disabled; set repository_graph.enabled = true in ferrus.toml"
            );
        }
        let project_id = project::current_project_id().await?;
        let requested_task_id = std::env::var(ENV_TASK_ID)
            .ok()
            .filter(|value| !value.trim().is_empty());
        let runtime_context = match agent_id.map(str::trim).filter(|value| !value.is_empty()) {
            Some(agent_id) => project::runtime_task_context_for_agent_read_only(agent_id).await?,
            None => None,
        };
        if let Some(requested_task_id) = requested_task_id.as_deref() {
            let Some(runtime) = runtime_context.as_ref() else {
                anyhow::bail!(
                    "repository graph task binding {requested_task_id:?} is not attached to the current runtime"
                );
            };
            if runtime.task_id != requested_task_id {
                anyhow::bail!(
                    "repository graph task binding does not match the current runtime task"
                );
            }
        }
        if config.enabled
            && runtime_context.as_ref().is_some_and(|runtime| {
                runtime.run_role.as_deref() == Some("supervisor")
                    && runtime.status == project::TaskStatus::Reviewing.as_str()
                    && runtime.repository_view.lifecycle != TaskViewLifecycle::FrozenSubmitted
            })
        {
            anyhow::bail!(
                "the submitted repository view is unavailable for this reviewer; inspect the task source directly"
            );
        }
        let repository_view = runtime_context
            .as_ref()
            .map(|context| context.repository_view.clone());
        let task_view_id = runtime_context
            .as_ref()
            .map(|context| TaskViewId::new(&context.task_id))
            .transpose()?;
        let run_id = runtime_context
            .as_ref()
            .and_then(|context| context.run_id.clone());
        let source_root = runtime_context
            .as_ref()
            .and_then(|context| context.repository_workspace_path.as_ref())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| root.clone());
        Ok(Self {
            project_root: root,
            root: source_root,
            repository: RepositoryRef {
                namespace: RepositoryNamespace::new(format!("local:{project_id}"))?,
                repository_id: RepositoryId::new("root")?,
            },
            config,
            repository_view,
            task_view_id,
            run_id,
        })
    }

    pub(crate) fn discover(&self) -> Result<LocalRepositorySource> {
        self.discover_at(&self.root)
            .context("Failed to discover the repository source")
    }

    pub(crate) fn discover_canonical(&self) -> Result<LocalRepositorySource> {
        self.discover_at(&self.project_root)
            .context("Failed to discover the canonical repository source")
    }

    fn discover_at(&self, root: &Path) -> Result<LocalRepositorySource> {
        let identities = active_extractor_identities(&self.config)?;
        let context = SourceDiscoveryContext::from_config(
            self.repository.clone(),
            &self.config,
            &identities,
        )?;
        LocalRepositorySource::discover(root, context).map_err(Into::into)
    }

    pub(crate) fn freshness_comparison(&self) -> Result<Option<FreshnessComparison>> {
        if !self.config.enabled {
            return Ok(None);
        }
        // Task snapshots use baseline/overlay composition identities rather
        // than the ordinary worktree manifest identity. Comparing those
        // different digest domains would report a false stale result.
        if self.repository_view.is_some() {
            return Ok(None);
        }
        let source = self.discover()?;
        Ok(Some(FreshnessComparison::from_manifest(source.manifest())))
    }

    pub(crate) fn scope(&self, budget: QueryBudget) -> Result<repository_graph::query::QueryScope> {
        let snapshot = match (&self.repository_view, &self.task_view_id) {
            (Some(view), _) if view.view_snapshot_id.is_some() => SnapshotSelector::Snapshot(
                view.view_snapshot_id
                    .clone()
                    .expect("checked materialized snapshot identity"),
            ),
            (Some(view), Some(task_view_id)) if view.overlay_revision_id.is_some() => {
                SnapshotSelector::Published(task_overlay_view_name(task_view_id)?)
            }
            (Some(view), _) if view.baseline_snapshot_id.is_some() => SnapshotSelector::Snapshot(
                view.baseline_snapshot_id
                    .clone()
                    .expect("checked baseline snapshot identity"),
            ),
            (Some(view), _) => anyhow::bail!(
                "repository graph is unavailable for the current task view ({})",
                view.status.as_str()
            ),
            (None, _) => SnapshotSelector::Published(
                PublishedViewName::new(CANONICAL_VIEW)
                    .expect("canonical published view name is non-empty"),
            ),
        };
        Ok(repository_graph::query::QueryScope::current(
            self.repository.clone(),
            snapshot,
            budget,
        ))
    }

    pub(crate) async fn status(&self) -> Result<StatusResponse> {
        if let Some(status) = self.unavailable_task_view_status() {
            let mut response = unavailable_status(
                self.repository.clone(),
                Availability::NotBuilt,
                status.as_str(),
                RetrievalAction::Index,
            )?;
            response.data.published_view = None;
            self.attach_task_view_to_status(&mut response);
            return Ok(response);
        }
        let path = sidecar_path().await?;
        // Discovering the current manifest can walk and hash the repository.
        // MCP retrieval must stay latency-bounded, so without a reliable source
        // mutation token it reports freshness as unknown rather than stale data
        // as fresh. The local CLI uses freshness_comparison() for exact checks.
        let mut response = status_response_at(self, &path, None)?;
        if let Some(reference) = self.canonical_invalidation().await {
            response.freshness = canonical_stale_freshness(reference);
            response.data.recommended_action = Some(RetrievalAction::RefreshIndex);
        }
        self.attach_task_view_to_status(&mut response);
        Ok(response)
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
        let mut response = search_response_at(self, &path, None, request);
        if let Ok(response) = response.as_mut() {
            // The durable stale marker is conservative and avoids a full
            // source scan on latency-bounded MCP reads.
            self.apply_canonical_invalidation(response).await;
            self.attach_task_view(response);
        }
        Ok(response)
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
        let mut response = context_response_at(self, &path, None, request);
        if let Ok(response) = response.as_mut() {
            self.apply_canonical_invalidation(response).await;
            self.attach_task_view(response);
        }
        Ok(response)
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

    pub(crate) fn task_view_envelope(&self) -> Option<TaskViewEnvelope> {
        let view = self.repository_view.as_ref()?;
        Some(TaskViewEnvelope {
            task_view_id: self.task_view_id.clone()?,
            baseline_snapshot_id: view.baseline_snapshot_id.clone()?,
            overlay_revision_id: view.overlay_revision_id.clone(),
            lifecycle: view.lifecycle,
        })
    }

    fn attach_task_view<T>(&self, response: &mut QueryResponse<T>) {
        response.task_view = self.task_view_envelope();
    }

    fn attach_task_view_to_status(&self, response: &mut StatusResponse) {
        response.task_view = self.task_view_envelope();
        if let Some(view) = self.repository_view.as_ref() {
            response.data.task_view_status = Some(task_view_status(view.status));
            response.data.fallback = (view.status != project::RepositoryViewStatus::Available)
                .then_some(RetrievalFallback::DirectSourceInspection);
        }
    }

    async fn apply_canonical_invalidation<T>(&self, response: &mut QueryResponse<T>) {
        let Some(reference) = self.canonical_invalidation().await else {
            return;
        };
        response.freshness = canonical_stale_freshness(reference);
    }

    async fn canonical_invalidation(&self) -> Option<project::CanonicalGraphReference> {
        if self.repository_view.is_some() {
            return None;
        }
        let reference = match project::canonical_graph_reference().await {
            Ok(reference) => reference,
            Err(error) => {
                tracing::warn!(error = ?error, "failed to read canonical graph invalidation state");
                return None;
            }
        };
        if reference.status != project::CanonicalGraphStatus::Stale {
            return None;
        }
        Some(reference)
    }
}

fn task_view_status(status: project::RepositoryViewStatus) -> TaskViewStatus {
    match status {
        project::RepositoryViewStatus::NotBuilt => TaskViewStatus::NotBuilt,
        project::RepositoryViewStatus::Available => TaskViewStatus::Available,
        project::RepositoryViewStatus::Stale => TaskViewStatus::Stale,
        project::RepositoryViewStatus::Unavailable => TaskViewStatus::Unavailable,
        project::RepositoryViewStatus::Failed => TaskViewStatus::Failed,
    }
}

fn canonical_stale_freshness(reference: project::CanonicalGraphReference) -> FreshnessEnvelope {
    FreshnessEnvelope {
        freshness: Freshness::Stale,
        compared_manifest: reference.source.map(|source| source.manifest_digest),
        reason_codes: vec!["canonical_invalidation".to_string()],
    }
}

fn task_overlay_view_name(task_view_id: &TaskViewId) -> Result<PublishedViewName> {
    Ok(PublishedViewName::new(format!(
        "task-overlay:{}",
        task_view_id.as_str()
    ))?)
}

pub(crate) async fn sidecar_path() -> Result<std::path::PathBuf> {
    Ok(project::current_project_data_dir()
        .await?
        .join(SIDECAR_FILE_NAME))
}

mod canonical;
pub(crate) use canonical::*;

async fn compare_and_record_task_baseline_at(
    database_path: &Path,
    task_id: &str,
    expected: Option<&project::RepositoryViewReference>,
    repository_view: &project::RepositoryViewReference,
) -> Result<bool> {
    let Some(expected) = expected else {
        return Ok(false);
    };
    project::compare_and_record_task_repository_view_at(
        database_path,
        task_id,
        expected,
        repository_view,
    )
    .await
}

/// Explicitly refreshes the mutable task overlay without coupling graph
/// failures to the orchestration state machine. `/check` and the final submit
/// gate call this after source-changing work; retrieval tools remain read-only.
pub(crate) async fn refresh_task_overlay_best_effort_for_context(
    runtime: &project::RuntimeTaskContext,
) {
    let Some(workspace_root) = runtime.workspace_path.as_deref() else {
        return;
    };
    let Some(baseline_tree) = std::env::var(ENV_BASELINE_TREE)
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return;
    };
    if let Err(error) = refresh_task_overlay(
        &runtime.task_id,
        Path::new(workspace_root),
        baseline_tree.trim(),
    )
    .await
    {
        tracing::warn!(
            task_id = runtime.task_id,
            error = ?error,
            "failed to refresh task repository graph overlay; task lifecycle is unchanged"
        );
    }
}

#[derive(Debug)]
pub(crate) enum RepositoryViewFreeze {
    NotAttempted,
    Frozen(project::RepositoryViewReference),
    Failed,
}

fn frozen_tree_matches_snapshot(
    sidecar_path: &Path,
    workspace_root: &Path,
    repository: RepositoryRef,
    config: &RepositoryGraphConfig,
    source_tree: Digest,
    snapshot_id: &repository_graph::domain::SnapshotId,
) -> Result<bool> {
    let identities = active_extractor_identities(config)?;
    let discovery = SourceDiscoveryContext::from_config(repository, config, &identities)?;
    let source = TaskBaselineSource::discover(workspace_root, discovery, source_tree)?;
    let sidecar = match open_for_query_at(sidecar_path)? {
        OpenQuerySidecarResult::Ready(sidecar) => sidecar,
        OpenQuerySidecarResult::Absent => {
            anyhow::bail!("repository graph sidecar is unavailable")
        }
        OpenQuerySidecarResult::NeedsMigration {
            found_schema_version,
        } => anyhow::bail!(
            "repository graph sidecar schema {found_schema_version} requires migration"
        ),
        OpenQuerySidecarResult::RequiresRebuild(reason) => anyhow::bail!(
            "repository graph sidecar schema {} is incompatible with {}",
            reason.found_schema_version,
            reason.supported_schema_version
        ),
    };
    let snapshot = sidecar
        .snapshot(snapshot_id)?
        .context("submitted repository graph snapshot is no longer retained")?;
    let snapshot_files = all_snapshot_file_descriptors(&sidecar, snapshot_id)?;
    Ok(snapshot.repository == source.manifest().revision.repository
        && snapshot.analysis_config_digest == source.manifest().revision.analysis_config_digest
        && snapshot.extractor_set_digest == source.manifest().extractor_set_digest
        && snapshot_files == source.manifest().files)
}

fn capture_matching_submitted_tree(
    sidecar_path: &Path,
    workspace_root: &Path,
    task_id: &str,
    repository: RepositoryRef,
    config: &RepositoryGraphConfig,
    snapshot_id: &repository_graph::domain::SnapshotId,
) -> Result<Digest> {
    let source_tree = capture_worktree_tree(workspace_root)?;
    pin_submitted_tree(workspace_root, task_id, &source_tree)?;
    let matches = frozen_tree_matches_snapshot(
        sidecar_path,
        workspace_root,
        repository,
        config,
        source_tree.clone(),
        snapshot_id,
    );
    match matches {
        Ok(true) => Ok(source_tree),
        result => {
            release_submitted_tree_pin(workspace_root, task_id)?;
            match result {
                Ok(false) => {
                    anyhow::bail!("submitted source tree does not match the indexed task view")
                }
                Ok(true) => unreachable!("matching submitted tree returned above"),
                Err(error) => Err(error),
            }
        }
    }
}

/// Prepares the immutable graph/source identity persisted by the submit
/// transaction. Git writes only content-addressed objects through an isolated
/// temporary index; it never changes the task's real index or lifecycle row.
pub(crate) async fn prepare_submitted_repository_view(
    runtime: &project::RuntimeTaskContext,
) -> RepositoryViewFreeze {
    let Some(workspace_root) = runtime.workspace_path.as_deref() else {
        return RepositoryViewFreeze::NotAttempted;
    };
    let Some(baseline_tree) = std::env::var(ENV_BASELINE_TREE)
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return RepositoryViewFreeze::NotAttempted;
    };
    let graph_context = match LocalGraphContext::load(false).await {
        Ok(context) if context.config.enabled => context,
        Ok(_) => return RepositoryViewFreeze::NotAttempted,
        Err(error) => {
            tracing::warn!(
                task_id = runtime.task_id,
                error = ?error,
                "failed to prepare submitted repository view; submit will continue"
            );
            return RepositoryViewFreeze::Failed;
        }
    };
    let sidecar_path = match sidecar_path().await {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(
                task_id = runtime.task_id,
                error = ?error,
                "failed to resolve repository graph sidecar; submit will continue"
            );
            return RepositoryViewFreeze::Failed;
        }
    };

    let mutable_view = match refresh_task_overlay(
        &runtime.task_id,
        Path::new(workspace_root),
        baseline_tree.trim(),
    )
    .await
    {
        Ok(view) => view,
        Err(error) => {
            tracing::warn!(
                task_id = runtime.task_id,
                error = ?error,
                "failed to refresh submitted repository view; submit will continue"
            );
            return RepositoryViewFreeze::Failed;
        }
    };
    let Some(view_snapshot_id) = mutable_view.view_snapshot_id.clone() else {
        tracing::warn!(
            task_id = runtime.task_id,
            "submitted repository view was not materialized; submit will continue"
        );
        return RepositoryViewFreeze::Failed;
    };
    let workspace_root = std::path::PathBuf::from(workspace_root);
    let task_id = runtime.task_id.clone();
    let repository = graph_context.repository;
    let config = graph_context.config;
    let source_tree = match tokio::task::spawn_blocking(move || {
        capture_matching_submitted_tree(
            &sidecar_path,
            &workspace_root,
            &task_id,
            repository,
            &config,
            &view_snapshot_id,
        )
    })
    .await
    {
        Ok(Ok(tree)) => tree,
        Ok(Err(error)) => {
            tracing::warn!(
                task_id = runtime.task_id,
                error = ?error,
                "failed to capture submitted source tree; submit will continue"
            );
            return RepositoryViewFreeze::Failed;
        }
        Err(error) => {
            tracing::warn!(
                task_id = runtime.task_id,
                error = ?error,
                "submitted source capture task failed; submit will continue"
            );
            return RepositoryViewFreeze::Failed;
        }
    };
    match mutable_view.frozen(source_tree) {
        Ok(view) => RepositoryViewFreeze::Frozen(view),
        Err(error) => {
            release_submitted_tree_pin_best_effort(runtime).await;
            tracing::warn!(
                task_id = runtime.task_id,
                error = ?error,
                "submitted repository view was not materialized; submit will continue"
            );
            RepositoryViewFreeze::Failed
        }
    }
}

pub(crate) async fn release_submitted_tree_pin_best_effort(runtime: &project::RuntimeTaskContext) {
    release_submitted_tree_pin_for_task_best_effort(&runtime.task_id).await;
}

pub(crate) async fn release_submitted_tree_pin_for_task_best_effort(task_id: &str) {
    let project_root = match project::canonical_project_root().await {
        Ok(root) => root,
        Err(error) => {
            tracing::warn!(
                task_id,
                error = ?error,
                "failed to resolve project root while releasing submitted tree pin"
            );
            return;
        }
    };
    let owned_task_id = task_id.to_string();
    match tokio::task::spawn_blocking(move || {
        release_submitted_tree_pin(&project_root, &owned_task_id)
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(
            task_id,
            error = ?error,
            "failed to release submitted tree pin"
        ),
        Err(error) => tracing::warn!(
            task_id,
            error = ?error,
            "submitted tree pin release task failed"
        ),
    }
}

pub(crate) async fn refresh_task_overlay(
    task_id: &str,
    workspace_root: &Path,
    baseline_tree: &str,
) -> Result<project::RepositoryViewReference> {
    let existing = project::task_repository_view(task_id)
        .await?
        .context("task repository view has not been initialized")?;
    let baseline_snapshot_id = existing
        .baseline_snapshot_id
        .clone()
        .context("task repository graph baseline is unavailable")?;
    let context = LocalGraphContext::load(false).await?;
    if !context.config.enabled {
        return Ok(existing);
    }
    let sidecar_path = sidecar_path().await?;
    let database_path = project::current_project_data_dir().await?.join("ferrus.db");
    let repository = context.repository.clone();
    let config = context.config.clone();
    let workspace_root = workspace_root.to_path_buf();
    let baseline_revision = parse_git_tree_digest(baseline_tree)?;
    let task_view_id = TaskViewId::new(task_id)?;
    let task_id = task_id.to_string();
    let indexed_baseline_snapshot_id = baseline_snapshot_id.clone();

    let refreshed = tokio::task::spawn_blocking(move || -> Result<_> {
        let identities = active_extractor_identities(&config)?;
        let discovery =
            SourceDiscoveryContext::from_config(repository.clone(), &config, &identities)?;
        let mut sidecar = match open_for_build_at(&sidecar_path)? {
            OpenSidecarResult::Ready(sidecar) => sidecar,
            OpenSidecarResult::RequiresRebuild(reason) => anyhow::bail!(
                "repository graph sidecar schema {} is incompatible with {}",
                reason.found_schema_version,
                reason.supported_schema_version
            ),
        };
        let baseline_snapshot = sidecar
            .snapshot(&indexed_baseline_snapshot_id)?
            .context("task repository graph baseline snapshot is no longer retained")?;
        let baseline_analysis_config_digest = baseline_snapshot.analysis_config_digest;
        let baseline_files =
            all_snapshot_file_descriptors(&sidecar, &indexed_baseline_snapshot_id)?;
        let view_name = task_overlay_view_name(&task_view_id)?;
        let build_id = next_task_overlay_build_id(&task_view_id)?;
        if sidecar.acquire_refresh_lease(
            &repository,
            &view_name,
            build_id.as_str(),
            REFRESH_LEASE_TTL,
        )? == RefreshLeaseOutcome::Busy
        {
            return Err(RefreshAlreadyInProgress.into());
        }
        let heartbeat = sidecar.start_refresh_lease_heartbeat(
            &repository,
            &view_name,
            build_id.as_str(),
            REFRESH_LEASE_TTL,
        )?;
        let refreshed = (|| -> Result<_> {
            let source = TaskOverlaySource::discover(
                &workspace_root,
                WorkspaceRef {
                    repository: repository.clone(),
                    task_view_id: task_view_id.clone(),
                    baseline_revision,
                },
                discovery,
                baseline_analysis_config_digest,
                baseline_files,
            )?;
            if !source.requires_index() {
                return Ok(None);
            }
            let overlay_revision_id = source.overlay_manifest().revision_id.clone();
            let outcome = IndexCoordinator::new(&mut sidecar).index(
                &source,
                &config,
                IndexRequest {
                    build_id: build_id.clone(),
                    view_name: view_name.clone(),
                    force_full: false,
                },
            )?;
            Ok(Some((overlay_revision_id, outcome.snapshot.id)))
        })();
        let lease_healthy = heartbeat.finish();
        let released = sidecar.release_refresh_lease(&repository, &view_name, build_id.as_str());
        let refreshed = refreshed?;
        if !lease_healthy || !released? {
            anyhow::bail!("task repository graph refresh lease was lost");
        }
        Ok(refreshed)
    })
    .await;

    let repository_view = match refreshed {
        Ok(Ok(Some((overlay_revision_id, view_snapshot_id)))) => {
            project::RepositoryViewReference::materialized(
                baseline_snapshot_id,
                Some(overlay_revision_id),
                view_snapshot_id,
                project::RepositoryViewStatus::Available,
            )?
        }
        Ok(Ok(None)) => project::RepositoryViewReference::materialized(
            baseline_snapshot_id.clone(),
            None,
            baseline_snapshot_id,
            project::RepositoryViewStatus::Available,
        )?,
        Ok(Err(error)) if error.downcast_ref::<RefreshAlreadyInProgress>().is_some() => {
            return Err(error);
        }
        Ok(Err(error)) => {
            let mut stale = existing.clone().mutable_successor();
            stale.status = project::RepositoryViewStatus::Stale;
            if !project::compare_and_record_task_repository_view_at(
                &database_path,
                &task_id,
                &existing,
                &stale,
            )
            .await?
            {
                return current_task_repository_view(&task_id).await;
            }
            return Err(error);
        }
        Err(error) => {
            let mut stale = existing.clone().mutable_successor();
            stale.status = project::RepositoryViewStatus::Stale;
            if !project::compare_and_record_task_repository_view_at(
                &database_path,
                &task_id,
                &existing,
                &stale,
            )
            .await?
            {
                return current_task_repository_view(&task_id).await;
            }
            return Err(error.into());
        }
    };
    if !project::compare_and_record_task_repository_view_at(
        &database_path,
        &task_id,
        &existing,
        &repository_view,
    )
    .await?
    {
        return current_task_repository_view(&task_id).await;
    }
    maintain_graph_best_effort().await;
    Ok(repository_view)
}

async fn current_task_repository_view(task_id: &str) -> Result<project::RepositoryViewReference> {
    project::task_repository_view(task_id)
        .await?
        .context("task repository view disappeared during refresh")
}

fn resolved_repository_view(
    task_id: &str,
    existing: Option<project::RepositoryViewReference>,
    resolved: Result<project::RepositoryViewReference>,
) -> project::RepositoryViewReference {
    match resolved {
        Ok(repository_view) => repository_view,
        Err(error) => {
            if error.downcast_ref::<RefreshAlreadyInProgress>().is_some() {
                return existing.unwrap_or_default();
            }
            tracing::warn!(
                task_id,
                error = ?error,
                "failed to pin task repository graph baseline; dispatch will continue"
            );
            match existing {
                Some(view) if view.baseline_snapshot_id.is_some() => {
                    let mut view = view.mutable_successor();
                    view.status = project::RepositoryViewStatus::Stale;
                    view
                }
                None => project::RepositoryViewReference::new(
                    None,
                    None,
                    project::RepositoryViewStatus::Failed,
                )
                .expect("a failed unavailable view has no overlay"),
                Some(_) => project::RepositoryViewReference::new(
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
    if let Some(existing) = existing
        && let Some(snapshot_id) = existing.baseline_snapshot_id.as_ref()
    {
        let retained = match open_for_query_at(&sidecar_path) {
            Ok(OpenQuerySidecarResult::Ready(sidecar)) => sidecar
                .snapshot(snapshot_id)
                .context("Failed to inspect the pinned baseline snapshot")?
                .is_some(),
            _ => false,
        };
        if retained {
            let mut retained_view = existing.clone().mutable_successor();
            retained_view.status = existing.status;
            return Ok(retained_view);
        }
    }
    if !context.config.enabled || baseline_tree.is_none() {
        return project::RepositoryViewReference::new(
            None,
            None,
            project::RepositoryViewStatus::Unavailable,
        );
    }

    let baseline_tree = parse_git_tree_digest(baseline_tree.expect("checked above"))?;
    let workspace_root = workspace_root.to_path_buf();
    let config = context.config.clone();
    let repository = context.repository.clone();
    let task_id = task_id.to_string();
    let snapshot = tokio::task::spawn_blocking(move || -> Result<_> {
        let identities = active_extractor_identities(&config)?;
        let discovery =
            SourceDiscoveryContext::from_config(repository.clone(), &config, &identities)?;
        let source = TaskBaselineSource::discover(&workspace_root, discovery, baseline_tree)?;
        let mut sidecar = match open_for_build_at(&sidecar_path)? {
            OpenSidecarResult::Ready(sidecar) => sidecar,
            OpenSidecarResult::RequiresRebuild(reason) => anyhow::bail!(
                "repository graph sidecar schema {} is incompatible with {}",
                reason.found_schema_version,
                reason.supported_schema_version
            ),
        };
        let build_id = next_task_baseline_build_id(&task_id)?;
        let view_name = PublishedViewName::new(format!("task-baseline:{task_id}"))?;
        if sidecar.acquire_refresh_lease(
            &repository,
            &view_name,
            build_id.as_str(),
            REFRESH_LEASE_TTL,
        )? == RefreshLeaseOutcome::Busy
        {
            return Err(RefreshAlreadyInProgress.into());
        }
        let heartbeat = sidecar.start_refresh_lease_heartbeat(
            &repository,
            &view_name,
            build_id.as_str(),
            REFRESH_LEASE_TTL,
        )?;
        let indexed = IndexCoordinator::new(&mut sidecar).index(
            &source,
            &config,
            IndexRequest {
                build_id: build_id.clone(),
                view_name: view_name.clone(),
                force_full: false,
            },
        );
        let lease_healthy = heartbeat.finish();
        let released = sidecar.release_refresh_lease(&repository, &view_name, build_id.as_str());
        let outcome = indexed?;
        if !lease_healthy || !released? {
            anyhow::bail!("task baseline repository graph refresh lease was lost");
        }
        Ok(outcome.snapshot.id)
    })
    .await??;

    project::RepositoryViewReference::materialized(
        snapshot.clone(),
        None,
        snapshot,
        project::RepositoryViewStatus::Available,
    )
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

fn next_task_overlay_build_id(task_view_id: &TaskViewId) -> Result<BuildId> {
    let sequence = TASK_OVERLAY_BUILD_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(BuildId::new(format!(
        "task-overlay:{}:{nanos:x}:{sequence:x}",
        task_view_id.as_str()
    ))?)
}

pub(crate) fn status_response_at(
    context: &LocalGraphContext,
    sidecar_path: &Path,
    freshness_comparison: Option<FreshnessComparison>,
) -> Result<StatusResponse> {
    if let Some(status) = context.unavailable_task_view_status() {
        let mut response = unavailable_status(
            context.repository.clone(),
            Availability::NotBuilt,
            status.as_str(),
            RetrievalAction::Index,
        )?;
        response.data.published_view = None;
        context.attach_task_view_to_status(&mut response);
        return Ok(response);
    }
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

mod query;
use query::*;

#[cfg(test)]
#[path = "repository_graph_runtime_tests.rs"]
mod tests;
