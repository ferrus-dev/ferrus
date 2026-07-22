use std::time::Instant;

use serde::Serialize;

use crate::{
    repository_graph::{
        domain::Freshness,
        query::{
            ContextResponse, QueryError, QueryErrorCode, SearchResponse, StatusResponse,
            TruncationReason,
        },
    },
    repository_graph_runtime::LocalGraphContext,
};

/// Privacy-safe retrieval metric. Request text, filters, repository paths,
/// snippets, and source bodies are deliberately not representable here.
#[derive(Debug, Serialize)]
struct RepositoryQueryMetric<'a> {
    tool: &'static str,
    repository_namespace: &'a str,
    repository_id: &'a str,
    task_view_id: Option<&'a str>,
    run_id: Option<&'a str>,
    baseline_snapshot: Option<&'a str>,
    overlay_revision: Option<&'a str>,
    snapshot: Option<&'a str>,
    build: Option<&'a str>,
    freshness: Option<Freshness>,
    duration_ms: u64,
    result_count: u64,
    response_bytes: u64,
    truncation: Option<TruncationReason>,
    diagnostics_count: u64,
    error_category: Option<QueryErrorCode>,
}

pub(super) fn status(
    context: &LocalGraphContext,
    started: Instant,
    response: &StatusResponse,
    response_bytes: usize,
) {
    emit(
        context,
        metric(
            context,
            "repository_graph_status",
            RepositoryQueryMetricData {
                snapshot: response.snapshot_id.as_ref().map(|id| id.as_str()),
                build: response.data.build_id.as_ref().map(|id| id.as_str()),
                freshness: Some(response.freshness.freshness),
                duration_ms: elapsed_ms(started),
                result_count: response.data.statistics.is_some() as u64,
                response_bytes: response_bytes as u64,
                truncation: response.page.truncation.as_ref().map(|value| value.reason),
                diagnostics_count: diagnostic_count(&response.diagnostics.summary),
                error_category: None,
            },
        ),
    );
}

pub(super) fn search(
    context: &LocalGraphContext,
    started: Instant,
    response: &Result<SearchResponse, QueryError>,
    response_bytes: usize,
) {
    let metric = match response {
        Ok(response) => metric(
            context,
            "repository_search",
            RepositoryQueryMetricData {
                snapshot: Some(response.snapshot_id.as_str()),
                build: None,
                freshness: Some(response.freshness.freshness),
                duration_ms: elapsed_ms(started),
                result_count: response.data.hits.len() as u64,
                response_bytes: response_bytes as u64,
                truncation: response.page.truncation.as_ref().map(|value| value.reason),
                diagnostics_count: diagnostic_count(&response.diagnostics.summary),
                error_category: None,
            },
        ),
        Err(error) => error_metric(context, "repository_search", started, error, response_bytes),
    };
    emit(context, metric);
}

pub(super) fn context(
    context: &LocalGraphContext,
    started: Instant,
    response: &Result<ContextResponse, QueryError>,
    response_bytes: usize,
) {
    let metric = match response {
        Ok(response) => metric(
            context,
            "repository_context",
            RepositoryQueryMetricData {
                snapshot: Some(response.snapshot_id.as_str()),
                build: None,
                freshness: Some(response.freshness.freshness),
                duration_ms: elapsed_ms(started),
                result_count: response.data.items.len() as u64,
                response_bytes: response_bytes as u64,
                truncation: response.page.truncation.as_ref().map(|value| value.reason),
                diagnostics_count: diagnostic_count(&response.diagnostics.summary),
                error_category: None,
            },
        ),
        Err(error) => error_metric(
            context,
            "repository_context",
            started,
            error,
            response_bytes,
        ),
    };
    emit(context, metric);
}

struct RepositoryQueryMetricData<'a> {
    snapshot: Option<&'a str>,
    build: Option<&'a str>,
    freshness: Option<Freshness>,
    duration_ms: u64,
    result_count: u64,
    response_bytes: u64,
    truncation: Option<TruncationReason>,
    diagnostics_count: u64,
    error_category: Option<QueryErrorCode>,
}

fn metric<'a>(
    context: &'a LocalGraphContext,
    tool: &'static str,
    data: RepositoryQueryMetricData<'a>,
) -> RepositoryQueryMetric<'a> {
    RepositoryQueryMetric {
        tool,
        repository_namespace: context.repository.namespace.as_str(),
        repository_id: context.repository.repository_id.as_str(),
        task_view_id: context.task_view_id.as_ref().map(|id| id.as_str()),
        run_id: context.run_id.as_deref(),
        baseline_snapshot: context
            .repository_view
            .as_ref()
            .and_then(|view| view.baseline_snapshot_id.as_ref().map(|id| id.as_str())),
        overlay_revision: context
            .repository_view
            .as_ref()
            .and_then(|view| view.overlay_revision_id.as_ref().map(|id| id.as_str())),
        snapshot: data.snapshot,
        build: data.build,
        freshness: data.freshness,
        duration_ms: data.duration_ms,
        result_count: data.result_count,
        response_bytes: data.response_bytes,
        truncation: data.truncation,
        diagnostics_count: data.diagnostics_count,
        error_category: data.error_category,
    }
}

fn error_metric<'a>(
    context: &'a LocalGraphContext,
    tool: &'static str,
    started: Instant,
    error: &'a QueryError,
    response_bytes: usize,
) -> RepositoryQueryMetric<'a> {
    metric(
        context,
        tool,
        RepositoryQueryMetricData {
            snapshot: None,
            build: None,
            freshness: None,
            duration_ms: elapsed_ms(started),
            result_count: 0,
            response_bytes: response_bytes as u64,
            truncation: None,
            diagnostics_count: 0,
            error_category: Some(error.code),
        },
    )
}

fn diagnostic_count(summary: &crate::repository_graph::query::DiagnosticSummary) -> u64 {
    summary.info + summary.warning + summary.error
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn emit(context: &LocalGraphContext, metric: RepositoryQueryMetric<'_>) {
    if !context.config.telemetry.enabled {
        return;
    }
    let encoded = serde_json::to_string(&metric)
        .expect("privacy-safe repository query metrics are always serializable");
    tracing::info!(
        target: "ferrus::repository_graph::query",
        metric = %encoded,
        "repository graph query"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_shape_cannot_contain_queries_paths_or_source_content() {
        let metric = RepositoryQueryMetric {
            tool: "repository_search",
            repository_namespace: "local:test",
            repository_id: "root",
            task_view_id: Some("t-001"),
            run_id: Some("r-001"),
            baseline_snapshot: Some("snapshot-baseline"),
            overlay_revision: Some("overlay-1"),
            snapshot: Some("snapshot-1"),
            build: None,
            freshness: Some(Freshness::Fresh),
            duration_ms: 12,
            result_count: 3,
            response_bytes: 400,
            truncation: None,
            diagnostics_count: 1,
            error_category: None,
        };

        let value = serde_json::to_value(metric).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 16);
        for forbidden in ["query", "text", "path", "snippet", "source"] {
            assert!(!object.contains_key(forbidden));
        }
    }
}
