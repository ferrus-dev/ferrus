use std::time::Instant;

use serde::Serialize;

use crate::project_memory::{
    federation::{ContextDomain, FederatedContextResponse, FederatedSearchResponse},
    query::{MemoryQueryError, MemoryStatusResponse},
};

#[derive(Serialize)]
struct Metric<'a> {
    tool: &'static str,
    project_namespace: Option<&'a str>,
    project_id: Option<&'a str>,
    domain: Option<ContextDomain>,
    repository_snapshot_id: Option<&'a str>,
    memory_revision_id: Option<&'a str>,
    duration_ms: u64,
    result_count: u64,
    response_bytes: u64,
    diagnostics_count: u64,
    error_category: Option<&'static str>,
}

pub(super) fn memory_status(
    started: Instant,
    response: &MemoryStatusResponse,
    response_bytes: usize,
) {
    emit(Metric {
        tool: "project_memory_status",
        project_namespace: Some(response.project.namespace.as_str()),
        project_id: Some(response.project.project_id.as_str()),
        domain: Some(ContextDomain::Memory),
        repository_snapshot_id: None,
        memory_revision_id: response.revision_id.as_ref().map(|id| id.as_str()),
        duration_ms: elapsed_ms(started),
        result_count: response.data.statistics.is_some() as u64,
        response_bytes: response_bytes as u64,
        diagnostics_count: response.diagnostics.len() as u64,
        error_category: None,
    });
}

pub(super) fn search(
    started: Instant,
    response: &FederatedSearchResponse,
    response_bytes: usize,
    error_category: Option<&'static str>,
) {
    emit(Metric {
        tool: "project_context_search",
        project_namespace: Some(response.project.namespace.as_str()),
        project_id: Some(response.project.project_id.as_str()),
        domain: Some(response.requested_domain),
        repository_snapshot_id: response
            .repository
            .as_ref()
            .and_then(|state| state.snapshot_id.as_ref().map(|id| id.as_str())),
        memory_revision_id: response
            .memory
            .as_ref()
            .and_then(|state| state.revision_id.as_ref().map(|id| id.as_str())),
        duration_ms: elapsed_ms(started),
        result_count: response.results.len() as u64,
        response_bytes: response_bytes as u64,
        diagnostics_count: response.federation_diagnostics.len() as u64,
        error_category,
    });
}

pub(super) fn context(
    started: Instant,
    response: &FederatedContextResponse,
    response_bytes: usize,
    error_category: Option<&'static str>,
) {
    emit(Metric {
        tool: "project_context",
        project_namespace: Some(response.project.namespace.as_str()),
        project_id: Some(response.project.project_id.as_str()),
        domain: Some(response.requested_domain),
        repository_snapshot_id: response
            .repository
            .as_ref()
            .and_then(|state| state.snapshot_id.as_ref().map(|id| id.as_str())),
        memory_revision_id: response
            .memory
            .as_ref()
            .and_then(|state| state.revision_id.as_ref().map(|id| id.as_str())),
        duration_ms: elapsed_ms(started),
        result_count: response.items.len() as u64,
        response_bytes: response_bytes as u64,
        diagnostics_count: response.federation_diagnostics.len() as u64,
        error_category,
    });
}

pub(super) fn search_error(started: Instant, error: &MemoryQueryError, response_bytes: usize) {
    emit_error("project_context_search", started, error, response_bytes);
}

pub(super) fn context_error(started: Instant, error: &MemoryQueryError, response_bytes: usize) {
    emit_error("project_context", started, error, response_bytes);
}

fn emit_error(
    tool: &'static str,
    started: Instant,
    error: &MemoryQueryError,
    response_bytes: usize,
) {
    emit(Metric {
        tool,
        project_namespace: None,
        project_id: None,
        domain: None,
        repository_snapshot_id: None,
        memory_revision_id: None,
        duration_ms: elapsed_ms(started),
        result_count: 0,
        response_bytes: response_bytes as u64,
        diagnostics_count: 0,
        error_category: Some(error_category(error)),
    });
}

fn error_category(error: &MemoryQueryError) -> &'static str {
    match error {
        MemoryQueryError::Unavailable => "unavailable",
        MemoryQueryError::RevisionNotFound => "revision_not_found",
        MemoryQueryError::StaleCursor => "stale_cursor",
        MemoryQueryError::BudgetExceeded(_) => "budget_exceeded",
        MemoryQueryError::ContentChanged => "content_changed",
        MemoryQueryError::SourceNotAuthorized => "source_not_authorized",
        MemoryQueryError::Backend(_) => "backend",
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn emit(metric: Metric<'_>) {
    let encoded = serde_json::to_string(&metric)
        .expect("privacy-safe project context metric is always serializable");
    tracing::info!(
        target: "ferrus::project_memory::query",
        metric = %encoded,
        "project context query"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_shape_cannot_contain_queries_paths_or_source_content() {
        let encoded = serde_json::to_string(&Metric {
            tool: "project_context",
            project_namespace: Some("local:ferrus"),
            project_id: Some("project"),
            domain: Some(ContextDomain::All),
            repository_snapshot_id: Some("snapshot"),
            memory_revision_id: Some("revision"),
            duration_ms: 1,
            result_count: 2,
            response_bytes: 3,
            diagnostics_count: 4,
            error_category: None,
        })
        .unwrap();
        for forbidden in ["query", "path", "snippet", "source_body", "memory_text"] {
            assert!(!encoded.contains(forbidden));
        }
    }
}
