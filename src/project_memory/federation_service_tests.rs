//! Federated retrieval tests for domain routing, exact revision links, freshness, and budgets.

use std::{
    fs,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

use tempfile::TempDir;

use crate::project_memory::federation::{ContextDomain, FederatedScope};
use crate::{
    project_memory::{
        domain::{
            MemoryEntityData, MemoryQueryText, MemoryRelationshipKind, MemoryViewName, ProjectId,
            ProjectNamespace, ProjectRef,
        },
        index::{MemoryIndexOptions, MemoryIndexer},
        policy::MemoryPolicy,
        ports::MemorySource,
        query::{MemoryContextPolicy, MemoryPageRequest},
        query_sqlite::{SqliteMemoryQuery, default_budget as default_memory_budget},
        source::LocalMemorySource,
        sqlite::MemorySidecar,
    },
    repository_graph::{
        config::RepositoryGraphConfig,
        domain::{
            BuildId, PublishedViewName, RepoPath, RepositoryId, RepositoryNamespace, RepositoryRef,
        },
        index::{IndexCoordinator, IndexRequest, active_extractor_identities},
        query::{ContextPolicy, EdgeDirection, SnapshotSelector},
        query_sqlite::{FreshnessComparison, SqliteGraphQuery},
        source::{FilesystemRepositorySource, SourceDiscoveryContext},
        sqlite::{OpenSidecarResult, open_for_build_at},
    },
};

use super::*;

struct Fixture {
    _root: TempDir,
    _data: TempDir,
    project: ProjectRef,
    repository: RepositoryRef,
    graph_sidecar: crate::repository_graph::sqlite::Sidecar,
    memory_sidecar: MemorySidecar,
    config: RepositoryGraphConfig,
    graph_freshness: FreshnessComparison,
    memory_freshness: MemoryFreshnessComparison,
}

#[derive(serde::Deserialize)]
struct EvaluationCorpus {
    version: u32,
    cases: Vec<EvaluationCase>,
}

#[derive(serde::Deserialize)]
struct EvaluationCase {
    id: String,
    search_query: String,
    expected_memory_text: String,
    expected_repository_path: String,
    baseline_raw_artifact_reads: u32,
    forbidden_raw_markers: Vec<String>,
}

fn project() -> ProjectRef {
    ProjectRef {
        namespace: ProjectNamespace::new("local:test").unwrap(),
        project_id: ProjectId::new("federation-project").unwrap(),
    }
}

fn repository() -> RepositoryRef {
    RepositoryRef {
        namespace: RepositoryNamespace::new("local:federation-project").unwrap(),
        repository_id: RepositoryId::new("root").unwrap(),
    }
}

fn initialize(root: &Path) {
    assert!(
        Command::new("git")
            .arg("init")
            .arg(root)
            .status()
            .unwrap()
            .success()
    );
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("docs/specs")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"federation-fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub struct ContextService;\npub fn federated_context() {}\n",
    )
    .unwrap();
    fs::write(
        root.join("docs/specs/federation.md"),
        "# Federation\n\n- [x] #4.4 Context\n\nID: rg4.4-test\nDepends on: none\n\n## Outcome\n\nImplemented `path:src/lib.rs` with bounded retrieval.\n",
    )
    .unwrap();
    assert!(
        Command::new("git")
            .current_dir(root)
            .args([
                "add",
                "--",
                "Cargo.toml",
                "src/lib.rs",
                "docs/specs/federation.md"
            ])
            .status()
            .unwrap()
            .success()
    );
}

fn fixture() -> Fixture {
    let root = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    initialize(root.path());
    super::super::source::record_approved_outcome_for_test(
        data.path(),
        "docs/specs/federation.md",
        &fs::read_to_string(root.path().join("docs/specs/federation.md")).unwrap(),
    );
    fs::create_dir_all(data.path().join("runs/t-1")).unwrap();
    fs::write(
        data.path().join("runs/t-1/SUBMISSION.md"),
        "private executor reasoning",
    )
    .unwrap();
    fs::write(
        data.path().join("runs/t-1/REVIEW.md"),
        "private review discussion",
    )
    .unwrap();
    fs::write(
        data.path().join("runs/t-1/PATCH.diff"),
        "private patch payload",
    )
    .unwrap();
    let project = project();
    let repository = repository();
    let config = RepositoryGraphConfig::default();
    let identities = active_extractor_identities(&config).unwrap();
    let discovery =
        SourceDiscoveryContext::from_config(repository.clone(), &config, &identities).unwrap();
    let repository_source = FilesystemRepositorySource::discover(root.path(), discovery).unwrap();
    let graph_freshness = FreshnessComparison::from_manifest(repository_source.manifest());
    let OpenSidecarResult::Ready(mut graph_sidecar) =
        open_for_build_at(&data.path().join("repo-graph.db")).unwrap()
    else {
        panic!("new graph sidecar should be writable");
    };
    IndexCoordinator::new(&mut graph_sidecar)
        .index(
            &repository_source,
            &config,
            IndexRequest {
                build_id: BuildId::new("federation-build").unwrap(),
                view_name: PublishedViewName::new("canonical").unwrap(),
                force_full: false,
            },
        )
        .unwrap();

    let memory_source = LocalMemorySource::discover_at(
        root.path().to_path_buf(),
        data.path().to_path_buf(),
        project.clone(),
        RepoPath::new("docs/specs").unwrap(),
        MemoryPolicy::default(),
    )
    .unwrap();
    let memory_freshness =
        MemoryFreshnessComparison::from_manifest(&memory_source.manifest().unwrap());
    let mut memory_sidecar = MemorySidecar::open_at(data.path()).unwrap();
    MemoryIndexer::new(&memory_source, &mut memory_sidecar)
        .unwrap()
        .index(MemoryIndexOptions::default())
        .unwrap();
    Fixture {
        _root: root,
        _data: data,
        project,
        repository,
        graph_sidecar,
        memory_sidecar,
        config,
        graph_freshness,
        memory_freshness,
    }
}

fn budget(config: &RepositoryGraphConfig) -> MemoryQueryBudget {
    default_memory_budget(&config.query_limits).unwrap()
}

fn target(fixture: &Fixture, domain: ContextDomain) -> FederatedTarget {
    let repository = RepositoryContextTarget {
        repository: fixture.repository.clone(),
        snapshot: SnapshotSelector::Published(PublishedViewName::new("canonical").unwrap()),
    };
    let memory = MemoryRevisionSelector::Published(MemoryViewName::new("project").unwrap());
    match domain {
        ContextDomain::Repository => FederatedTarget::Repository { repository },
        ContextDomain::Memory => FederatedTarget::Memory { memory },
        ContextDomain::All => FederatedTarget::All { repository, memory },
    }
}

fn service<'a>(
    fixture: &'a Fixture,
    graph: &'a SqliteGraphQuery<'a>,
    memory: &'a SqliteMemoryQuery<'a>,
) -> FederatedContextService<'a, SqliteGraphQuery<'a>, SqliteMemoryQuery<'a>, MemorySidecar> {
    FederatedContextService::new(
        graph,
        memory,
        &fixture.memory_sidecar,
        fixture.config.query_limits.clone(),
        Some(fixture.memory_freshness.clone()),
    )
}

#[path = "federation_service_tests/authorization.rs"]
mod authorization_tests;
#[path = "federation_service_tests/memory.rs"]
mod memory_tests;
#[path = "federation_service_tests/pagination.rs"]
mod pagination_tests;
#[path = "federation_service_tests/review.rs"]
mod review_tests;

#[test]
fn federation_candidate_cap_is_reported_as_result_truncation() {
    assert!(candidate_cap_truncated(MAX_FEDERATION_CANDIDATES, true));
    assert!(!candidate_cap_truncated(MAX_FEDERATION_CANDIDATES, false));
    assert!(!candidate_cap_truncated(
        MAX_FEDERATION_CANDIDATES - 1,
        true
    ));
}

#[test]
fn federated_search_response_fitting_observes_the_request_deadline() {
    let fixture = fixture();
    let graph = SqliteGraphQuery::new(
        &fixture.graph_sidecar,
        fixture.config.query_limits.clone(),
        Some(fixture.graph_freshness.clone()),
    );
    let memory =
        SqliteMemoryQuery::new(&fixture.memory_sidecar, fixture.config.query_limits.clone());
    let service = service(&fixture, &graph, &memory);
    let mut response = service
        .search(FederatedSearchRequest {
            scope: FederatedScope::current(
                fixture.project.clone(),
                target(&fixture, ContextDomain::All),
                budget(&fixture.config),
            ),
            text: MemoryQueryText::new("federat").unwrap(),
            repository_kinds: vec![],
            repository_paths: vec![],
            memory_kinds: vec![],
            memory_sources: vec![],
            cursor: None,
        })
        .unwrap();
    let total = response.results.len();
    let response_revision_key =
        revision_key(response.repository.as_ref(), response.memory.as_ref()).unwrap();
    let started = Instant::now()
        .checked_sub(Duration::from_millis(2))
        .unwrap();

    let error = fit_federated_search_response(
        &mut response,
        u64::MAX,
        None,
        0,
        total,
        "expired-search-fitting",
        &response_revision_key,
        started,
        1,
    )
    .unwrap_err();

    assert_eq!(
        error,
        MemoryQueryError::BudgetExceeded(MemoryTruncationReason::Duration)
    );
}
