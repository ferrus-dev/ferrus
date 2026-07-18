use std::time::Instant;

use serde::Serialize;

use crate::repository_graph::{
    config::RepositoryGraphConfig,
    domain::Freshness,
    query::{
        ContextResponse, QueryError, QueryErrorCode, SearchResponse, StatusResponse,
        TruncationReason,
    },
};

/// Privacy-safe retrieval metric. Request text, filters, repository paths,
/// snippets, and source bodies are deliberately not representable here.
#[derive(Debug, Serialize)]
struct RepositoryQueryMetric<'a> {
    tool: &'static str,
    snapshot: Option<&'a str>,
    freshness: Option<Freshness>,
    duration_ms: u64,
    result_count: u64,
    response_bytes: u64,
    truncation: Option<TruncationReason>,
    diagnostics_count: u64,
    error_category: Option<QueryErrorCode>,
}

pub(super) fn status(
    config: &RepositoryGraphConfig,
    started: Instant,
    response: &StatusResponse,
    response_bytes: usize,
) {
    emit(
        config,
        RepositoryQueryMetric {
            tool: "repository_graph_status",
            snapshot: response.snapshot_id.as_ref().map(|id| id.as_str()),
            freshness: Some(response.freshness.freshness),
            duration_ms: elapsed_ms(started),
            result_count: response.data.statistics.is_some() as u64,
            response_bytes: response_bytes as u64,
            truncation: response.page.truncation.as_ref().map(|value| value.reason),
            diagnostics_count: diagnostic_count(&response.diagnostics.summary),
            error_category: None,
        },
    );
}

pub(super) fn search(
    config: &RepositoryGraphConfig,
    started: Instant,
    response: &Result<SearchResponse, QueryError>,
    response_bytes: usize,
) {
    let metric = match response {
        Ok(response) => RepositoryQueryMetric {
            tool: "repository_search",
            snapshot: Some(response.snapshot_id.as_str()),
            freshness: Some(response.freshness.freshness),
            duration_ms: elapsed_ms(started),
            result_count: response.data.hits.len() as u64,
            response_bytes: response_bytes as u64,
            truncation: response.page.truncation.as_ref().map(|value| value.reason),
            diagnostics_count: diagnostic_count(&response.diagnostics.summary),
            error_category: None,
        },
        Err(error) => error_metric("repository_search", started, error, response_bytes),
    };
    emit(config, metric);
}

pub(super) fn context(
    config: &RepositoryGraphConfig,
    started: Instant,
    response: &Result<ContextResponse, QueryError>,
    response_bytes: usize,
) {
    let metric = match response {
        Ok(response) => RepositoryQueryMetric {
            tool: "repository_context",
            snapshot: Some(response.snapshot_id.as_str()),
            freshness: Some(response.freshness.freshness),
            duration_ms: elapsed_ms(started),
            result_count: response.data.items.len() as u64,
            response_bytes: response_bytes as u64,
            truncation: response.page.truncation.as_ref().map(|value| value.reason),
            diagnostics_count: diagnostic_count(&response.diagnostics.summary),
            error_category: None,
        },
        Err(error) => error_metric("repository_context", started, error, response_bytes),
    };
    emit(config, metric);
}

fn error_metric<'a>(
    tool: &'static str,
    started: Instant,
    error: &'a QueryError,
    response_bytes: usize,
) -> RepositoryQueryMetric<'a> {
    RepositoryQueryMetric {
        tool,
        snapshot: None,
        freshness: None,
        duration_ms: elapsed_ms(started),
        result_count: 0,
        response_bytes: response_bytes as u64,
        truncation: None,
        diagnostics_count: 0,
        error_category: Some(error.code),
    }
}

fn diagnostic_count(summary: &crate::repository_graph::query::DiagnosticSummary) -> u64 {
    summary.info + summary.warning + summary.error
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn emit(config: &RepositoryGraphConfig, metric: RepositoryQueryMetric<'_>) {
    if !config.telemetry.enabled {
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
            snapshot: Some("snapshot-1"),
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
        assert_eq!(object.len(), 9);
        for forbidden in ["query", "text", "path", "snippet", "source"] {
            assert!(!object.contains_key(forbidden));
        }
    }
}
