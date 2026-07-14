//! Explicit local performance baseline harness.
//!
//! This is ignored in the normal test suite. Run with cargo test and the
//! repository_graph::benchmarks::medium_fixture_baseline test filter.

use std::time::Instant;

use serde::Serialize;

use super::{
    config::RepositoryGraphConfig,
    domain::{BuildId, PublishedViewName, RepositoryId, RepositoryNamespace, RepositoryRef},
    index::{IndexCoordinator, IndexRequest, active_extractor_identities},
    ports::GraphQuery,
    query::{PageRequest, QueryScope, SearchRequest, SnapshotSelector},
    query_sqlite::{SqliteGraphQuery, default_budget},
    source::{FilesystemRepositorySource, SourceDiscoveryContext},
    sqlite::{OpenSidecarResult, open_for_build_at},
};

const MODULE_COUNT: usize = 300;

#[derive(Serialize)]
struct Baseline {
    fixture_files: usize,
    cold_us: u128,
    cold_parsed: u64,
    cold_nodes: u64,
    cold_edges: u64,
    noop_us: u128,
    noop_reused: u64,
    noop_parsed: u64,
    changed_us: u128,
    changed_reused: u64,
    changed_parsed: u64,
    search_us: u128,
    search_hits: usize,
}

fn repository() -> RepositoryRef {
    RepositoryRef {
        namespace: RepositoryNamespace::new("local:benchmark").unwrap(),
        repository_id: RepositoryId::new("medium-fixture").unwrap(),
    }
}

fn discover(root: &std::path::Path, config: &RepositoryGraphConfig) -> FilesystemRepositorySource {
    let identities = active_extractor_identities(config).unwrap();
    let context = SourceDiscoveryContext::from_config(repository(), config, &identities).unwrap();
    FilesystemRepositorySource::discover(root, context).unwrap()
}

fn write_fixture(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='graph-benchmark'\nversion='0.1.0'\n",
    )
    .unwrap();
    let mut lib = String::new();
    for index in 0..MODULE_COUNT {
        lib.push_str(&format!("pub mod module_{index};\n"));
        std::fs::write(
            root.join(format!("src/module_{index}.rs")),
            format!(
                "pub struct Type{index};\nimpl Type{index} {{ pub fn value() -> usize {{ {index} }} }}\n"
            ),
        )
        .unwrap();
    }
    std::fs::write(root.join("src/lib.rs"), lib).unwrap();
}

#[test]
#[ignore = "explicit performance baseline"]
fn medium_fixture_baseline() {
    let source_dir = tempfile::tempdir().unwrap();
    write_fixture(source_dir.path());
    let sidecar_dir = tempfile::tempdir().unwrap();
    let OpenSidecarResult::Ready(mut sidecar) =
        open_for_build_at(&sidecar_dir.path().join("repo-graph.db")).unwrap()
    else {
        panic!("new sidecar unexpectedly requires rebuild");
    };
    let config = RepositoryGraphConfig::default();
    let view = PublishedViewName::new("canonical").unwrap();

    let source = discover(source_dir.path(), &config);
    let started = Instant::now();
    let cold = IndexCoordinator::new(&mut sidecar)
        .index(
            &source,
            &config,
            IndexRequest {
                build_id: BuildId::new("benchmark-cold").unwrap(),
                view_name: view.clone(),
                force_full: false,
            },
        )
        .unwrap();
    let cold_us = started.elapsed().as_micros();

    let source = discover(source_dir.path(), &config);
    let started = Instant::now();
    let noop = IndexCoordinator::new(&mut sidecar)
        .index(
            &source,
            &config,
            IndexRequest {
                build_id: BuildId::new("benchmark-noop").unwrap(),
                view_name: view.clone(),
                force_full: false,
            },
        )
        .unwrap();
    let noop_us = started.elapsed().as_micros();

    std::fs::write(
        source_dir.path().join("src/module_150.rs"),
        "pub struct Type150;\nimpl Type150 { pub fn changed() -> bool { true } }\n",
    )
    .unwrap();
    let source = discover(source_dir.path(), &config);
    let compared_manifest = source.manifest().revision.manifest_digest.clone();
    let started = Instant::now();
    let changed = IndexCoordinator::new(&mut sidecar)
        .index(
            &source,
            &config,
            IndexRequest {
                build_id: BuildId::new("benchmark-changed").unwrap(),
                view_name: view.clone(),
                force_full: false,
            },
        )
        .unwrap();
    let changed_us = started.elapsed().as_micros();

    let query = SqliteGraphQuery::new(
        &sidecar,
        config.query_limits.clone(),
        Some(compared_manifest),
    );
    let started = Instant::now();
    let search = query
        .search(&SearchRequest {
            scope: QueryScope::v1(
                repository(),
                SnapshotSelector::Published(view),
                default_budget(&config.query_limits).unwrap(),
            ),
            text: "Type150".to_string(),
            node_kinds: vec!["struct".to_string()],
            paths: vec![],
            page: PageRequest { cursor: None },
        })
        .unwrap();
    let search_us = started.elapsed().as_micros();
    assert_eq!(cold.metrics.parsed_files, (MODULE_COUNT + 2) as u64);
    assert_eq!(noop.metrics.reused_files, (MODULE_COUNT + 2) as u64);
    assert_eq!(noop.metrics.parsed_files, 0);
    assert_eq!(changed.metrics.reused_files, (MODULE_COUNT + 1) as u64);
    assert_eq!(changed.metrics.parsed_files, 1);
    assert!(!search.data.hits.is_empty());

    println!(
        "{}",
        serde_json::to_string_pretty(&Baseline {
            fixture_files: MODULE_COUNT + 2,
            cold_us,
            cold_parsed: cold.metrics.parsed_files,
            cold_nodes: cold.metrics.nodes,
            cold_edges: cold.metrics.edges,
            noop_us,
            noop_reused: noop.metrics.reused_files,
            noop_parsed: noop.metrics.parsed_files,
            changed_us,
            changed_reused: changed.metrics.reused_files,
            changed_parsed: changed.metrics.parsed_files,
            search_us,
            search_hits: search.data.hits.len(),
        })
        .unwrap()
    );
}
