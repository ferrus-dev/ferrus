use std::{hint::black_box, path::Path, time::Duration};

use criterion::{BatchSize, Criterion, Throughput, criterion_group};
use ferrus::repository_graph::{
    config::RepositoryGraphConfig,
    domain::{BuildId, PublishedViewName, RepositoryId, RepositoryNamespace, RepositoryRef},
    extractors::cargo::run_parser_worker_if_requested,
    index::{IndexCoordinator, IndexOutcome, IndexRequest, active_extractor_identities},
    ports::GraphQuery,
    query::{PageRequest, QueryScope, SearchRequest, SnapshotSelector},
    query_sqlite::{FreshnessComparison, SqliteGraphQuery, default_budget},
    source::{FilesystemRepositorySource, SourceDiscoveryContext},
    sqlite::{OpenSidecarResult, Sidecar, open_for_build_at},
};
use tempfile::TempDir;

const MODULE_COUNT: usize = 300;
const FIXTURE_FILE_COUNT: usize = MODULE_COUNT + 2;

struct Fixture {
    source_dir: TempDir,
    _sidecar_dir: TempDir,
    sidecar: Sidecar,
    config: RepositoryGraphConfig,
    view: PublishedViewName,
}

impl Fixture {
    fn new() -> Self {
        let source_dir = tempfile::tempdir().unwrap();
        write_fixture(source_dir.path());
        let sidecar_dir = tempfile::tempdir().unwrap();
        let OpenSidecarResult::Ready(sidecar) =
            open_for_build_at(&sidecar_dir.path().join("repo-graph.db")).unwrap()
        else {
            panic!("new sidecar unexpectedly requires rebuild");
        };
        Self {
            source_dir,
            _sidecar_dir: sidecar_dir,
            sidecar,
            config: RepositoryGraphConfig::default(),
            view: PublishedViewName::new("canonical").unwrap(),
        }
    }

    fn discover(&self) -> FilesystemRepositorySource {
        discover(self.source_dir.path(), &self.config)
    }

    fn index(&mut self, source: &FilesystemRepositorySource, build_id: &str) -> IndexOutcome {
        IndexCoordinator::new(&mut self.sidecar)
            .index(
                source,
                &self.config,
                IndexRequest {
                    build_id: BuildId::new(build_id).unwrap(),
                    view_name: self.view.clone(),
                    force_full: false,
                },
            )
            .unwrap()
    }

    fn change_one_file(&self) {
        std::fs::write(
            self.source_dir.path().join("src/module_150.rs"),
            "pub struct Type150;\nimpl Type150 { pub fn changed() -> bool { true } }\n",
        )
        .unwrap();
    }
}

fn repository() -> RepositoryRef {
    RepositoryRef {
        namespace: RepositoryNamespace::new("local:benchmark").unwrap(),
        repository_id: RepositoryId::new("medium-fixture").unwrap(),
    }
}

fn discover(root: &Path, config: &RepositoryGraphConfig) -> FilesystemRepositorySource {
    let identities = active_extractor_identities(config).unwrap();
    let context = SourceDiscoveryContext::from_config(repository(), config, &identities).unwrap();
    FilesystemRepositorySource::discover(root, context).unwrap()
}

fn write_fixture(root: &Path) {
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

fn prepare_noop() -> (Fixture, FilesystemRepositorySource) {
    let mut fixture = Fixture::new();
    let source = fixture.discover();
    fixture.index(&source, "benchmark-cold");
    let source = fixture.discover();
    (fixture, source)
}

fn prepare_incremental() -> (Fixture, FilesystemRepositorySource) {
    let (fixture, _) = prepare_noop();
    fixture.change_one_file();
    let source = fixture.discover();
    (fixture, source)
}

fn verify_invariants() {
    let mut fixture = Fixture::new();
    let source = fixture.discover();
    let cold = fixture.index(&source, "verify-cold");
    assert_eq!(cold.metrics.parsed_files, FIXTURE_FILE_COUNT as u64);

    let source = fixture.discover();
    let noop = fixture.index(&source, "verify-noop");
    assert_eq!(noop.metrics.reused_files, FIXTURE_FILE_COUNT as u64);
    assert_eq!(noop.metrics.parsed_files, 0);

    fixture.change_one_file();
    let source = fixture.discover();
    let freshness_comparison = FreshnessComparison::from_manifest(source.manifest());
    let changed = fixture.index(&source, "verify-changed");
    assert_eq!(
        changed.metrics.reused_files,
        (FIXTURE_FILE_COUNT - 1) as u64
    );
    assert_eq!(changed.metrics.parsed_files, 1);

    let query = SqliteGraphQuery::new(
        &fixture.sidecar,
        fixture.config.query_limits.clone(),
        Some(freshness_comparison),
    );
    let search = query.search(&search_request(&fixture)).unwrap();
    assert!(!search.data.hits.is_empty());
}

fn search_request(fixture: &Fixture) -> SearchRequest {
    SearchRequest {
        scope: QueryScope::current(
            repository(),
            SnapshotSelector::Published(fixture.view.clone()),
            default_budget(&fixture.config.query_limits).unwrap(),
        ),
        text: "Type150".to_string(),
        node_kinds: vec!["struct".to_string()],
        paths: vec![],
        page: PageRequest { cursor: None },
    }
}

fn repository_graph_benchmarks(criterion: &mut Criterion) {
    verify_invariants();

    let mut indexing = criterion.benchmark_group("repository_graph/index");
    indexing.sample_size(10);
    indexing.warm_up_time(Duration::from_secs(1));
    indexing.measurement_time(Duration::from_secs(5));
    indexing.throughput(Throughput::Elements(FIXTURE_FILE_COUNT as u64));

    indexing.bench_function("cold", |bencher| {
        bencher.iter_batched(
            || {
                let fixture = Fixture::new();
                let source = fixture.discover();
                (fixture, source)
            },
            |(mut fixture, source)| {
                black_box(fixture.index(&source, "benchmark-cold"));
            },
            BatchSize::LargeInput,
        );
    });

    indexing.bench_function("noop", |bencher| {
        bencher.iter_batched(
            prepare_noop,
            |(mut fixture, source)| {
                black_box(fixture.index(&source, "benchmark-noop"));
            },
            BatchSize::LargeInput,
        );
    });

    indexing.bench_function("one_file_change", |bencher| {
        bencher.iter_batched(
            prepare_incremental,
            |(mut fixture, source)| {
                black_box(fixture.index(&source, "benchmark-changed"));
            },
            BatchSize::LargeInput,
        );
    });
    indexing.finish();

    let (search_fixture, search_source) = prepare_noop();
    let freshness_comparison = FreshnessComparison::from_manifest(search_source.manifest());
    let query = SqliteGraphQuery::new(
        &search_fixture.sidecar,
        search_fixture.config.query_limits.clone(),
        Some(freshness_comparison),
    );
    let request = search_request(&search_fixture);
    let mut querying = criterion.benchmark_group("repository_graph/query");
    querying.bench_function("exact_symbol_search", |bencher| {
        bencher.iter(|| black_box(query.search(black_box(&request)).unwrap()));
    });
    querying.finish();
}

criterion_group!(benches, repository_graph_benchmarks);

fn main() {
    if run_parser_worker_if_requested().expect("Cargo parser worker protocol failed") {
        return;
    }
    benches();
    Criterion::default().configure_from_args().final_summary();
}
