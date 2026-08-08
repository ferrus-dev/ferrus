//! Source-boundary tests for the optional distributed data-plane contracts.

#[test]
fn distributed_contracts_are_vendor_and_storage_neutral() {
    let contracts = concat!(
        include_str!("identity.rs"),
        include_str!("protocol.rs"),
        include_str!("security.rs"),
        include_str!("source.rs"),
        include_str!("coordinator.rs"),
        include_str!("fact_store.rs"),
        include_str!("worker.rs"),
        include_str!("publication.rs"),
        include_str!("api.rs"),
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
fn stateless_worker_has_no_repository_execution_or_network_api() {
    let worker = include_str!("worker.rs");
    for forbidden in [
        "std::process::Command",
        "tokio::process",
        "TcpStream",
        "UdpSocket",
        "ureq::",
        "reqwest::",
    ] {
        assert!(
            !worker.contains(forbidden),
            "stateless worker gained a forbidden capability: {forbidden}"
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
