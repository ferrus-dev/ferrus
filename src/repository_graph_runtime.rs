//! Machine-local repository graph runtime adapter shared by CLI and MCP reads.

use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result};

use crate::{project, repository_graph};
use repository_graph::{
    QUERY_WIRE_VERSION,
    config::RepositoryGraphConfig,
    domain::{
        Availability, Freshness, PublishedViewName, QueryBudget, RepositoryId, RepositoryNamespace,
        RepositoryRef,
    },
    index::active_extractor_identities,
    ports::GraphQuery,
    query::{
        DiagnosticSummary, DiagnosticsEnvelope, FreshnessEnvelope, PageInfo, QueryError,
        QueryErrorCode, RetrievalAction, SearchRequest, SearchResponse, SnapshotSelector,
        StatusData, StatusRequest, StatusResponse,
    },
    query_sqlite::{FreshnessComparison, SqliteGraphQuery, default_budget},
    source::{LocalRepositorySource, SourceDiscoveryContext},
    sqlite::{OpenQuerySidecarResult, SIDECAR_FILE_NAME, open_for_query_at},
};

pub(crate) const CANONICAL_VIEW: &str = "canonical";

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

    pub(crate) fn scope(&self, budget: QueryBudget) -> Result<repository_graph::query::QueryScope> {
        Ok(repository_graph::query::QueryScope::current(
            self.repository.clone(),
            SnapshotSelector::Published(PublishedViewName::new(CANONICAL_VIEW)?),
            budget,
        ))
    }

    pub(crate) async fn status(&self) -> Result<StatusResponse> {
        let path = sidecar_path().await?;
        let comparison = self.freshness_comparison().ok().flatten();
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
        let comparison = self.freshness_comparison().ok().flatten();
        Ok(search_response_at(self, &path, comparison, request))
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
    fn absent_status_and_search_are_read_only_and_actionable() {
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

        assert_eq!(status.data.availability, Availability::NotBuilt);
        assert_eq!(status.data.recommended_action, Some(RetrievalAction::Index));
        assert_eq!(search.code, QueryErrorCode::NotBuilt);
        assert_eq!(search.recommended_action, Some(RetrievalAction::Index));
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
}
