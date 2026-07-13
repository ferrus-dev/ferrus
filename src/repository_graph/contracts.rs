//! Compile-time source-boundary checks for the Phase 0 architecture.

#[test]
fn backend_neutral_contracts_do_not_depend_on_rusqlite() {
    let contracts = concat!(
        include_str!("domain.rs"),
        include_str!("query.rs"),
        include_str!("ports.rs"),
        include_str!("diagnostics.rs"),
    );
    assert!(!contracts.contains("rusqlite::"));
    assert!(!contracts.contains("rusqlite::{"));
}

#[test]
fn graph_storage_sql_does_not_leak_into_runtime_project_module() {
    let project = include_str!("../project.rs");
    for graph_storage_marker in [
        "repo-graph.db",
        "published_views",
        "extractor_set_digest",
        "CREATE TABLE snapshots",
        "CREATE TABLE nodes",
        "CREATE TABLE edges",
    ] {
        assert!(
            !project.contains(graph_storage_marker),
            "graph storage marker leaked into project.rs: {graph_storage_marker}"
        );
    }
}

#[test]
fn sidecar_schema_has_no_source_body_storage_columns() {
    let schema = include_str!("sqlite.rs").to_ascii_lowercase();
    for forbidden_column in [
        "source_body",
        "content_text",
        "content_blob",
        "source_snippet",
        "snippet_text",
    ] {
        assert!(
            !schema.contains(forbidden_column),
            "sidecar schema contains forbidden source storage: {forbidden_column}"
        );
    }
}
