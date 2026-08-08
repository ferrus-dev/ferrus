//! Source-boundary tests for the optional distributed data-plane contracts.

#[test]
fn distributed_contracts_are_vendor_and_storage_neutral() {
    let contracts = concat!(
        include_str!("identity.rs"),
        include_str!("protocol.rs"),
        include_str!("security.rs"),
        include_str!("source.rs"),
        include_str!("coordinator.rs"),
    );
    for forbidden in [
        "rusqlite::",
        "tokio::",
        "reqwest::",
        "aws_",
        "azure_",
        "google_cloud",
        "repo-graph.db",
        "project-memory.db",
        "ferrus.db",
    ] {
        assert!(
            !contracts.contains(forbidden),
            "distributed contract leaked backend detail: {forbidden}"
        );
    }
}

#[test]
fn distributed_contracts_have_no_credential_or_free_form_error_channel() {
    let security = include_str!("security.rs");
    let protocol = include_str!("protocol.rs");
    for forbidden in ["credential_secret", "access_token", "refresh_token"] {
        assert!(
            !security.contains(forbidden),
            "security contract leaked credential material: {forbidden}"
        );
    }
    for forbidden in ["pub message:", "pub details:", "pub stack_trace:"] {
        assert!(
            !protocol.contains(forbidden),
            "wire error added free-form detail: {forbidden}"
        );
    }
}

#[test]
fn local_runtime_does_not_depend_on_distributed_contracts() {
    for local_source in [
        include_str!("../project.rs"),
        include_str!("../repository_graph_runtime.rs"),
        include_str!("../project_memory_runtime.rs"),
    ] {
        assert!(!local_source.contains("distributed::"));
    }
}
