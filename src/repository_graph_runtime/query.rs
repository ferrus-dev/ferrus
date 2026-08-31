use super::*;

pub(super) fn search_response_at(
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

pub(super) fn context_response_at(
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

pub(super) fn attach_snippets_at(
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

    let content: Box<dyn SnapshotContent> = match context.repository_view.as_ref() {
        Some(view) if view.lifecycle == TaskViewLifecycle::FrozenSubmitted => {
            let tree = view.frozen_source_tree.clone().ok_or_else(|| {
                query_error(
                    QueryErrorCode::ContentUnavailable,
                    "frozen repository view is missing its source tree identity",
                    false,
                    None,
                )
            })?;
            Box::new(
                GitTreeSnapshotContent::new(
                    &context.project_root,
                    context.repository.clone(),
                    response.snapshot_id.clone(),
                    tree,
                    &context.config.source,
                    files,
                    hard_limit,
                )
                .map_err(|_| {
                    query_error(
                        QueryErrorCode::ContentUnavailable,
                        "frozen repository content boundary could not be initialized",
                        true,
                        None,
                    )
                })?,
            )
        }
        _ => Box::new(
            LocalSnapshotContent::new(
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
            })?,
        ),
    };

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

pub(super) fn add_content_diagnostic(
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

pub(super) fn add_content_diagnostic_without_location(
    response: &mut ContextResponse,
    max_diagnostics: usize,
    code: &str,
) {
    add_bounded_content_diagnostic(response, max_diagnostics, code, None);
}

pub(super) fn add_bounded_content_diagnostic(
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

pub(super) fn unavailable_status(
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
        task_view: None,
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
            task_view_status: None,
            fallback: None,
        },
    })
}

pub(super) fn query_error(
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

pub(super) fn unavailable_task_view_error(status: project::RepositoryViewStatus) -> QueryError {
    let mut error = query_error(
        QueryErrorCode::NotBuilt,
        "repository graph is unavailable for the current task baseline; inspect source directly",
        matches!(status, project::RepositoryViewStatus::Stale),
        Some(RetrievalAction::Index),
    );
    error
        .details
        .insert("task_view_status".to_string(), status.as_str().to_string());
    error.details.insert(
        "fallback".to_string(),
        "direct_source_inspection".to_string(),
    );
    error
}
