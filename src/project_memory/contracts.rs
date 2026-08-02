//! Compile-time source-boundary checks for the RG4 contract layer.

#[test]
fn backend_neutral_memory_contracts_do_not_depend_on_sqlite() {
    let contracts = concat!(
        include_str!("domain.rs"),
        include_str!("policy.rs"),
        include_str!("query.rs"),
        include_str!("federation.rs"),
        include_str!("ports.rs"),
        include_str!("diagnostics.rs"),
    );
    assert!(!contracts.contains("rusqlite::"));
    assert!(!contracts.contains("repo-graph.db"));
    assert!(!contracts.contains("ferrus.db"));
}

#[test]
fn memory_storage_contracts_do_not_leak_into_runtime_project_module() {
    let project = include_str!("../project.rs");
    for marker in [
        "memory_revisions",
        "memory_entities",
        "memory_relationships",
        "published_memory_views",
        "CREATE TABLE project_memory",
    ] {
        assert!(
            !project.contains(marker),
            "project memory storage marker leaked into project.rs: {marker}"
        );
    }
}
