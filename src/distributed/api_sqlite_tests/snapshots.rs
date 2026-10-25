//! Tests for snapshots.

use super::*;

#[test]
fn local_and_remote_sqlite_adapters_share_snapshot_query_semantics() {
    use std::{fs, process::Command};

    use crate::{
        project_memory::{
            domain::{MemoryQueryText, MemoryViewName},
            index::{MemoryIndexOptions, MemoryIndexer},
            policy::MemoryPolicy,
            ports::MemoryQuery,
            query::{
                MemoryPageRequest, MemoryQueryScope, MemoryRevisionSelector, MemorySearchRequest,
            },
            query_sqlite::{SqliteMemoryQuery, default_budget as memory_budget},
            source::LocalMemorySource,
            sqlite::MemorySidecar,
        },
        repository_graph::{
            config::RepositoryGraphConfig,
            domain::{BuildId, PublishedViewName, RepositoryId, RepositoryNamespace},
            index::{IndexCoordinator, IndexRequest, active_extractor_identities},
            ports::GraphQuery,
            query::{PageRequest, QueryScope, SearchRequest, SnapshotSelector},
            query_sqlite::{SqliteGraphQuery, default_budget as graph_budget},
            source::{FilesystemRepositorySource, SourceDiscoveryContext},
            sqlite::{OpenSidecarResult, open_for_build_at},
        },
    };

    let local_root = tempfile::tempdir().unwrap();
    fs::create_dir_all(local_root.path().join("src")).unwrap();
    fs::write(
        local_root.path().join("Cargo.toml"),
        "[package]\nname='parity'\nversion='0.1.0'\n",
    )
    .unwrap();
    fs::write(
        local_root.path().join("src/lib.rs"),
        "pub struct ImportantType;\n",
    )
    .unwrap();
    let repository = crate::repository_graph::domain::RepositoryRef {
        namespace: RepositoryNamespace::new("local:parity").unwrap(),
        repository_id: RepositoryId::new("root").unwrap(),
    };
    let graph_config = RepositoryGraphConfig::default();
    let identities = active_extractor_identities(&graph_config).unwrap();
    let context =
        SourceDiscoveryContext::from_config(repository.clone(), &graph_config, &identities)
            .unwrap();
    let source = FilesystemRepositorySource::discover(local_root.path(), context).unwrap();
    let graph_data = tempfile::tempdir().unwrap();
    let OpenSidecarResult::Ready(mut sidecar) =
        open_for_build_at(&graph_data.path().join("repo-graph.db")).unwrap()
    else {
        panic!("new parity sidecar unexpectedly requires rebuild");
    };
    let snapshot = IndexCoordinator::new(&mut sidecar)
        .index(
            &source,
            &graph_config,
            IndexRequest {
                build_id: BuildId::new("build-parity").unwrap(),
                view_name: PublishedViewName::new("canonical").unwrap(),
                force_full: false,
            },
        )
        .unwrap()
        .snapshot;
    let local_graph = SqliteGraphQuery::new(&sidecar, graph_config.query_limits.clone(), None);
    let local_graph_request = SearchRequest {
        scope: QueryScope::current(
            repository,
            SnapshotSelector::Snapshot(snapshot.id),
            graph_budget(&graph_config.query_limits).unwrap(),
        ),
        text: "ImportantType".to_string(),
        node_kinds: Vec::new(),
        paths: Vec::new(),
        page: PageRequest { cursor: None },
    };
    let observe_local_graph = || {
        let response = local_graph.search(&local_graph_request).unwrap();
        AdapterContractObservation {
            target: response.snapshot_id.to_string(),
            result_ids: response
                .data
                .hits
                .iter()
                .map(|hit| hit.node_id.to_string())
                .collect(),
            every_result_has_provenance: response
                .data
                .hits
                .iter()
                .all(|hit| hit.provenance.evidence.is_some()),
            protocol_version: response.wire_version,
        }
    };
    assert_snapshot_query_contract(
        "local repository SQLite",
        observe_local_graph(),
        observe_local_graph(),
    );

    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(local_root.path())
            .status()
            .unwrap()
            .success()
    );
    fs::create_dir_all(local_root.path().join("docs/specs")).unwrap();
    fs::write(
        local_root.path().join("docs/specs/parity.md"),
        "# Parity\n\n- [x] #5.6 Adapter parity\n\nID: parity\nDepends on: none\n\n## Outcome\n\nDelivered important bounded retrieval.\n",
    )
    .unwrap();
    assert!(
        Command::new("git")
            .args(["add", "--", "docs/specs/parity.md"])
            .current_dir(local_root.path())
            .status()
            .unwrap()
            .success()
    );
    let memory_data = tempfile::tempdir().unwrap();
    let memory_project = ProjectRef {
        namespace: ProjectNamespace::new("local:parity").unwrap(),
        project_id: ProjectId::new("project").unwrap(),
    };
    crate::project_memory::source::record_approved_outcome_for_test(
        memory_data.path(),
        "docs/specs/parity.md",
        &fs::read_to_string(local_root.path().join("docs/specs/parity.md")).unwrap(),
    );
    let memory_source = LocalMemorySource::discover_at(
        local_root.path().to_path_buf(),
        memory_data.path().to_path_buf(),
        memory_project.clone(),
        RepoPath::new("docs/specs").unwrap(),
        MemoryPolicy::default(),
    )
    .unwrap();
    let mut memory_sidecar = MemorySidecar::open_at(memory_data.path()).unwrap();
    MemoryIndexer::new(&memory_source, &mut memory_sidecar)
        .unwrap()
        .index(MemoryIndexOptions::default())
        .unwrap();
    let memory_limits = graph_config.query_limits.clone();
    let local_memory = SqliteMemoryQuery::new(&memory_sidecar, memory_limits.clone());
    let local_memory_request = MemorySearchRequest {
        scope: MemoryQueryScope::current(
            memory_project,
            MemoryRevisionSelector::Published(MemoryViewName::new("project").unwrap()),
            memory_budget(&memory_limits).unwrap(),
        ),
        text: MemoryQueryText::new("important").unwrap(),
        entity_kinds: Vec::new(),
        source_categories: Vec::new(),
        page: MemoryPageRequest { cursor: None },
    };
    let observe_local_memory = || {
        let response = local_memory.search(local_memory_request.clone()).unwrap();
        AdapterContractObservation {
            target: response.revision_id.to_string(),
            result_ids: response
                .hits
                .iter()
                .map(|hit| hit.entity.id.to_string())
                .collect(),
            every_result_has_provenance: response
                .hits
                .iter()
                .all(|hit| serde_json::to_vec(&hit.entity.provenance).is_ok()),
            protocol_version: response.wire_version,
        }
    };
    assert_snapshot_query_contract(
        "local memory SQLite",
        observe_local_memory(),
        observe_local_memory(),
    );

    let remote = fixture();
    let authorization = auth(
        CredentialClass::QueryAgent,
        AuthorizationScope::Project(remote.project.clone()),
    );
    let observe_remote = |target: RemoteQueryTarget, text: &str| {
        let response = remote
            .query
            .query(
                &authorization,
                &query_request(
                    &remote,
                    target,
                    RemoteQueryOperation::Search {
                        text: text.to_string(),
                        graph_kinds: Vec::new(),
                        graph_paths: Vec::new(),
                        memory_kinds: Vec::new(),
                    },
                    10,
                    None,
                ),
            )
            .unwrap();
        let RemoteQueryData::Search(data) = response.body.data else {
            panic!("remote parity search response expected");
        };
        AdapterContractObservation {
            target: serde_json::to_string(&response.resolved_target).unwrap(),
            result_ids: data
                .items
                .iter()
                .map(|item| match item {
                    RemoteSearchItem::Repository { node, .. } => node.id.to_string(),
                    RemoteSearchItem::Memory { entity, .. } => entity.id.to_string(),
                })
                .collect(),
            every_result_has_provenance: data.items.iter().all(|item| match item {
                RemoteSearchItem::Repository { node, .. } => node.provenance.evidence.is_some(),
                RemoteSearchItem::Memory { entity, .. } => {
                    serde_json::to_vec(&entity.provenance).is_ok()
                }
            }),
            protocol_version: response.protocol_version,
        }
    };
    assert_snapshot_query_contract(
        "remote repository SQLite",
        observe_remote(
            RemoteQueryTarget::Repository(remote.graph.clone()),
            "important",
        ),
        observe_remote(
            RemoteQueryTarget::Repository(remote.graph.clone()),
            "important",
        ),
    );
    assert_snapshot_query_contract(
        "remote memory SQLite",
        observe_remote(
            RemoteQueryTarget::Memory(remote.memory.clone()),
            "important",
        ),
        observe_remote(
            RemoteQueryTarget::Memory(remote.memory.clone()),
            "important",
        ),
    );
}
