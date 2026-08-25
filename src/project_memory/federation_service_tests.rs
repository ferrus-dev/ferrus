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

#[test]
fn cursor_is_bound_to_both_domain_revisions() {
    let cursor = encode_cursor("search", "request", "revision-one", 4).unwrap();
    let decoded = decode_cursor(Some(&cursor), "search", "request")
        .unwrap()
        .unwrap();
    assert_eq!(
        validate_cursor_revision(Some(&decoded), "revision-two"),
        Err(MemoryQueryError::StaleCursor)
    );
}

#[test]
fn search_candidate_cap_is_reported_as_result_truncation() {
    assert!(candidate_cap_truncated(MAX_FEDERATION_CANDIDATES, true));
    assert!(!candidate_cap_truncated(MAX_FEDERATION_CANDIDATES, false));
    assert!(!candidate_cap_truncated(
        MAX_FEDERATION_CANDIDATES - 1,
        true
    ));
}

#[test]
fn explicit_domain_seed_validation_never_broadens_scope() {
    let seed = FederatedContextSeed::MemoryEntity(
        super::super::domain::MemoryEntityId::new("memory-entity").unwrap(),
    );
    assert!(repository_only_seeds(&[seed]).is_err());
}

#[test]
fn repository_and_memory_search_scopes_remain_explicit() {
    let fixture = fixture();
    let graph = SqliteGraphQuery::new(
        &fixture.graph_sidecar,
        fixture.config.query_limits.clone(),
        Some(fixture.graph_freshness.clone()),
    );
    let memory =
        SqliteMemoryQuery::new(&fixture.memory_sidecar, fixture.config.query_limits.clone());
    let service = service(&fixture, &graph, &memory);
    let request = |domain| FederatedSearchRequest {
        scope: super::super::federation::FederatedScope::current(
            fixture.project.clone(),
            target(&fixture, domain),
            budget(&fixture.config),
        ),
        text: MemoryQueryText::new("federat").unwrap(),
        repository_kinds: vec![],
        repository_paths: vec![],
        memory_kinds: vec![],
        memory_sources: vec![],
        cursor: None,
    };

    let repository = service.search(request(ContextDomain::Repository)).unwrap();
    assert!(repository.repository.is_some());
    assert!(repository.memory.is_none());
    assert!(
        repository
            .results
            .iter()
            .all(|result| matches!(result, FederatedSearchResult::Repository(_)))
    );

    let memory = service.search(request(ContextDomain::Memory)).unwrap();
    assert!(memory.repository.is_none());
    assert!(memory.memory.is_some());
    assert!(
        memory
            .results
            .iter()
            .all(|result| matches!(result, FederatedSearchResult::Memory(_)))
    );
}

#[test]
fn combined_context_crosses_only_the_exact_resolved_link_set() {
    let fixture = fixture();
    let graph = SqliteGraphQuery::new(
        &fixture.graph_sidecar,
        fixture.config.query_limits.clone(),
        Some(fixture.graph_freshness.clone()),
    );
    let memory =
        SqliteMemoryQuery::new(&fixture.memory_sidecar, fixture.config.query_limits.clone());
    let memory_search = memory
        .search(MemorySearchRequest {
            scope: MemoryQueryScope::current(
                fixture.project.clone(),
                MemoryRevisionSelector::Published(MemoryViewName::new("project").unwrap()),
                budget(&fixture.config),
            ),
            text: MemoryQueryText::new("src/lib.rs").unwrap(),
            entity_kinds: vec![],
            source_categories: vec![],
            page: MemoryPageRequest::default(),
        })
        .unwrap();
    let memory_entity = memory_search.hits[0].entity.id.clone();
    let service = service(&fixture, &graph, &memory);
    let response = service
        .context(FederatedContextRequest {
            scope: super::super::federation::FederatedScope::current(
                fixture.project.clone(),
                target(&fixture, ContextDomain::All),
                budget(&fixture.config),
            ),
            seeds: vec![FederatedContextSeed::MemoryEntity(memory_entity)],
            repository_policy: ContextPolicy {
                direction: EdgeDirection::Both,
                edge_kinds: vec![],
                include_unresolved: false,
                include_external: false,
            },
            memory_policy: MemoryContextPolicy {
                direction: EdgeDirection::Both,
                relationship_kinds: vec![],
                include_unresolved: false,
                include_stale: false,
                include_snippets: false,
            },
            cursor: None,
        })
        .unwrap();
    assert!(response.repository.is_some());
    assert!(response.memory.is_some());
    assert!(response.items.iter().any(|item| {
        matches!(item, FederatedContextItem::Repository(item) if item.path.as_str() == "src/lib.rs")
    }));
    assert!(
        response
            .items
            .iter()
            .any(|item| matches!(item, FederatedContextItem::Memory(_)))
    );
    assert!(!response.cross_domain_links.is_empty());
    assert!(response.cross_domain_links.iter().all(|relationship| {
        relationship.provenance.resolution == MemoryResolutionState::Resolved
    }));
    assert_eq!(
        response.repository.as_ref().unwrap().freshness.freshness,
        crate::repository_graph::domain::Freshness::Fresh
    );
    assert_eq!(
        response.memory.as_ref().unwrap().freshness.freshness,
        super::super::query::MemoryFreshness::Fresh
    );
}

#[test]
fn combined_context_charges_cross_domain_links_against_depth() {
    let fixture = fixture();
    let graph = SqliteGraphQuery::new(
        &fixture.graph_sidecar,
        fixture.config.query_limits.clone(),
        Some(fixture.graph_freshness.clone()),
    );
    let memory =
        SqliteMemoryQuery::new(&fixture.memory_sidecar, fixture.config.query_limits.clone());
    let memory_search = memory
        .search(MemorySearchRequest {
            scope: MemoryQueryScope::current(
                fixture.project.clone(),
                MemoryRevisionSelector::Published(MemoryViewName::new("project").unwrap()),
                budget(&fixture.config),
            ),
            text: MemoryQueryText::new("src/lib.rs").unwrap(),
            entity_kinds: vec![],
            source_categories: vec![],
            page: MemoryPageRequest::default(),
        })
        .unwrap();
    let memory_entity = memory_search.hits[0].entity.id.clone();
    let service = service(&fixture, &graph, &memory);
    let request = |max_results| {
        let mut query_budget = budget(&fixture.config);
        query_budget.max_depth = NonZeroU32::new(1).unwrap();
        query_budget.max_results = NonZeroU32::new(max_results).unwrap();
        FederatedContextRequest {
            scope: FederatedScope::current(
                fixture.project.clone(),
                target(&fixture, ContextDomain::All),
                query_budget,
            ),
            seeds: vec![FederatedContextSeed::MemoryEntity(memory_entity.clone())],
            repository_policy: ContextPolicy {
                direction: EdgeDirection::Both,
                edge_kinds: vec![],
                include_unresolved: false,
                include_external: false,
            },
            memory_policy: MemoryContextPolicy {
                direction: EdgeDirection::Outgoing,
                relationship_kinds: vec![],
                include_unresolved: false,
                include_stale: false,
                include_snippets: false,
            },
            cursor: None,
        }
    };

    let response = service.context(request(100)).unwrap();
    let repository_items = response
        .items
        .iter()
        .filter_map(|item| match item {
            FederatedContextItem::Repository(item) => Some(item),
            FederatedContextItem::Memory(_) => None,
        })
        .collect::<Vec<_>>();
    assert!(!repository_items.is_empty());
    assert!(repository_items.iter().all(|item| {
        item.selection_reasons
            .iter()
            .any(|reason| reason.kind == ContextSelectionKind::ExactSeed)
    }));

    let paged = service.context(request(1)).unwrap();
    assert_eq!(
        paged
            .page
            .truncation
            .as_ref()
            .map(|truncation| truncation.explored_depth),
        Some(1)
    );
}

#[test]
fn combined_context_applies_direction_and_kind_to_cross_domain_links() {
    let fixture = fixture();
    let graph = SqliteGraphQuery::new(
        &fixture.graph_sidecar,
        fixture.config.query_limits.clone(),
        Some(fixture.graph_freshness.clone()),
    );
    let memory =
        SqliteMemoryQuery::new(&fixture.memory_sidecar, fixture.config.query_limits.clone());
    let memory_search = memory
        .search(MemorySearchRequest {
            scope: MemoryQueryScope::current(
                fixture.project.clone(),
                MemoryRevisionSelector::Published(MemoryViewName::new("project").unwrap()),
                budget(&fixture.config),
            ),
            text: MemoryQueryText::new("src/lib.rs").unwrap(),
            entity_kinds: vec![],
            source_categories: vec![],
            page: MemoryPageRequest::default(),
        })
        .unwrap();
    let memory_entity = memory_search.hits[0].entity.id.clone();
    let service = service(&fixture, &graph, &memory);
    let request = |seeds, direction, relationship_kinds| FederatedContextRequest {
        scope: FederatedScope::current(
            fixture.project.clone(),
            target(&fixture, ContextDomain::All),
            budget(&fixture.config),
        ),
        seeds,
        repository_policy: ContextPolicy {
            direction: EdgeDirection::Both,
            edge_kinds: vec![],
            include_unresolved: false,
            include_external: false,
        },
        memory_policy: MemoryContextPolicy {
            direction,
            relationship_kinds,
            include_unresolved: false,
            include_stale: false,
            include_snippets: false,
        },
        cursor: None,
    };

    let forward = service
        .context(request(
            vec![FederatedContextSeed::MemoryEntity(memory_entity.clone())],
            EdgeDirection::Outgoing,
            vec![],
        ))
        .unwrap();
    let link = forward
        .cross_domain_links
        .first()
        .expect("outgoing memory traversal should cross an exact repository link");
    let snapshot_id = forward
        .repository
        .as_ref()
        .and_then(|state| state.snapshot_id.as_ref());
    let repository_seed = repository_seed_from_target(&link.target, snapshot_id)
        .expect("resolved fixture link should produce a repository seed");
    assert!(
        forward
            .items
            .iter()
            .any(|item| { matches!(item, FederatedContextItem::Repository(_)) })
    );

    let blocked_forward = service
        .context(request(
            vec![FederatedContextSeed::MemoryEntity(memory_entity)],
            EdgeDirection::Incoming,
            vec![],
        ))
        .unwrap();
    assert!(blocked_forward.cross_domain_links.is_empty());
    assert!(
        blocked_forward
            .items
            .iter()
            .all(|item| { !matches!(item, FederatedContextItem::Repository(_)) })
    );

    let reverse = service
        .context(request(
            vec![FederatedContextSeed::Repository(repository_seed.clone())],
            EdgeDirection::Incoming,
            vec![],
        ))
        .unwrap();
    assert!(!reverse.cross_domain_links.is_empty());
    assert!(
        reverse
            .items
            .iter()
            .any(|item| { matches!(item, FederatedContextItem::Memory(_)) })
    );

    let blocked_reverse = service
        .context(request(
            vec![FederatedContextSeed::Repository(repository_seed.clone())],
            EdgeDirection::Outgoing,
            vec![],
        ))
        .unwrap();
    assert!(blocked_reverse.cross_domain_links.is_empty());
    assert!(
        blocked_reverse
            .items
            .iter()
            .all(|item| { !matches!(item, FederatedContextItem::Memory(_)) })
    );

    let kind_filtered = service
        .context(request(
            vec![FederatedContextSeed::Repository(repository_seed)],
            EdgeDirection::Both,
            vec![MemoryRelationshipKind::Supersedes],
        ))
        .unwrap();
    assert!(kind_filtered.cross_domain_links.is_empty());
    assert!(
        kind_filtered
            .items
            .iter()
            .all(|item| { !matches!(item, FederatedContextItem::Memory(_)) })
    );
}

#[test]
fn supplemental_byte_truncation_does_not_advertise_an_unusable_cursor() {
    let fixture = fixture();
    let graph = SqliteGraphQuery::new(
        &fixture.graph_sidecar,
        fixture.config.query_limits.clone(),
        Some(fixture.graph_freshness.clone()),
    );
    let memory =
        SqliteMemoryQuery::new(&fixture.memory_sidecar, fixture.config.query_limits.clone());
    let memory_search = memory
        .search(MemorySearchRequest {
            scope: MemoryQueryScope::current(
                fixture.project.clone(),
                MemoryRevisionSelector::Published(MemoryViewName::new("project").unwrap()),
                budget(&fixture.config),
            ),
            text: MemoryQueryText::new("src/lib.rs").unwrap(),
            entity_kinds: vec![],
            source_categories: vec![],
            page: MemoryPageRequest::default(),
        })
        .unwrap();
    let memory_entity = memory_search.hits[0].entity.id.clone();
    let service = service(&fixture, &graph, &memory);
    let request = |query_budget, cursor| FederatedContextRequest {
        scope: FederatedScope::current(
            fixture.project.clone(),
            target(&fixture, ContextDomain::All),
            query_budget,
        ),
        seeds: vec![FederatedContextSeed::MemoryEntity(memory_entity.clone())],
        repository_policy: ContextPolicy {
            direction: EdgeDirection::Both,
            edge_kinds: vec![],
            include_unresolved: false,
            include_external: false,
        },
        memory_policy: MemoryContextPolicy {
            direction: EdgeDirection::Both,
            relationship_kinds: vec![],
            include_unresolved: false,
            include_stale: false,
            include_snippets: false,
        },
        cursor,
    };
    let complete = service
        .context(request(budget(&fixture.config), None))
        .unwrap();
    assert!(!complete.cross_domain_links.is_empty());
    let primary_bytes = complete
        .items
        .iter()
        .map(serialized_len)
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .into_iter()
        .sum::<u64>();
    let mut constrained_budget = budget(&fixture.config);
    constrained_budget.max_bytes = std::num::NonZeroU64::new(primary_bytes).unwrap();

    let truncated = service.context(request(constrained_budget, None)).unwrap();
    assert!(!truncated.items.is_empty());
    assert_eq!(
        truncated.page.truncation.as_ref().map(|value| value.reason),
        Some(MemoryTruncationReason::Bytes)
    );
    let encoded_bytes = serde_json::to_vec(&truncated).unwrap().len() as u64;
    assert!(encoded_bytes <= primary_bytes);
    assert_eq!(
        truncated.page.truncation.as_ref().unwrap().returned_bytes,
        encoded_bytes
    );
    assert_eq!(
        truncated.page.next_cursor.is_some(),
        truncated.items.len() < complete.items.len()
    );

    assert!(complete.items.len() > 2);
    let page_size = complete.items.len() - 1;
    let page_primary_bytes = complete
        .items
        .iter()
        .take(page_size)
        .map(serialized_len)
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .into_iter()
        .sum::<u64>();
    let mut page_budget = budget(&fixture.config);
    page_budget.max_results = NonZeroU32::new(u32::try_from(page_size).unwrap()).unwrap();
    page_budget.max_bytes = NonZeroU64::new(page_primary_bytes).unwrap();

    let first = service.context(request(page_budget.clone(), None)).unwrap();
    assert_eq!(
        first.page.truncation.as_ref().map(|value| value.reason),
        Some(MemoryTruncationReason::Bytes)
    );
    let cursor = first
        .page
        .next_cursor
        .clone()
        .expect("remaining primary items must stay pageable");
    let second = service.context(request(page_budget, Some(cursor))).unwrap();
    assert!(!second.items.is_empty());
    assert!(second.items.iter().all(|item| !first.items.contains(item)));
}

#[test]
fn combined_context_counts_the_complete_response_against_the_byte_budget() {
    let fixture = fixture();
    let graph = SqliteGraphQuery::new(
        &fixture.graph_sidecar,
        fixture.config.query_limits.clone(),
        Some(fixture.graph_freshness.clone()),
    );
    let memory =
        SqliteMemoryQuery::new(&fixture.memory_sidecar, fixture.config.query_limits.clone());
    let service = service(&fixture, &graph, &memory);
    let memory_search = memory
        .search(MemorySearchRequest {
            scope: MemoryQueryScope::current(
                fixture.project.clone(),
                MemoryRevisionSelector::Published(MemoryViewName::new("project").unwrap()),
                budget(&fixture.config),
            ),
            text: MemoryQueryText::new("src/lib.rs").unwrap(),
            entity_kinds: vec![],
            source_categories: vec![],
            page: MemoryPageRequest::default(),
        })
        .unwrap();
    let seed = memory_search.hits[0].entity.id.clone();
    let request = |query_budget| FederatedContextRequest {
        scope: FederatedScope::current(
            fixture.project.clone(),
            target(&fixture, ContextDomain::All),
            query_budget,
        ),
        seeds: vec![FederatedContextSeed::MemoryEntity(seed.clone())],
        repository_policy: ContextPolicy {
            direction: EdgeDirection::Both,
            edge_kinds: vec![],
            include_unresolved: false,
            include_external: false,
        },
        memory_policy: MemoryContextPolicy {
            direction: EdgeDirection::Both,
            relationship_kinds: vec![],
            include_unresolved: false,
            include_stale: false,
            include_snippets: false,
        },
        cursor: None,
    };

    let complete = service.context(request(budget(&fixture.config))).unwrap();
    assert!(complete.items.len() > 1);
    let mut one_item_budget = budget(&fixture.config);
    one_item_budget.max_results = NonZeroU32::new(1).unwrap();
    let one_item = service.context(request(one_item_budget)).unwrap();
    let one_response_bytes = one_item
        .page
        .truncation
        .as_ref()
        .expect("one-item context must be truncated")
        .returned_bytes;
    assert_eq!(
        one_response_bytes,
        serde_json::to_vec(&one_item).unwrap().len() as u64
    );

    let mut constrained_budget = budget(&fixture.config);
    constrained_budget.max_bytes = NonZeroU64::new(one_response_bytes).unwrap();
    let constrained = service.context(request(constrained_budget)).unwrap();
    assert_eq!(constrained.items.len(), 1);
    let encoded_bytes = serde_json::to_vec(&constrained).unwrap().len() as u64;
    assert!(encoded_bytes <= one_response_bytes);
    assert_eq!(
        constrained
            .page
            .truncation
            .as_ref()
            .expect("context must report byte truncation")
            .returned_bytes,
        encoded_bytes
    );

    let bare_item_bytes = serialized_len(&complete.items[0]).unwrap();
    let mut insufficient_budget = budget(&fixture.config);
    insufficient_budget.max_bytes = NonZeroU64::new(bare_item_bytes + 1).unwrap();
    assert_eq!(
        service.context(request(insufficient_budget)).unwrap_err(),
        MemoryQueryError::BudgetExceeded(MemoryTruncationReason::Bytes)
    );
}

#[test]
fn combined_search_cursor_is_deterministic_and_does_not_repeat_results() {
    let fixture = fixture();
    let graph = SqliteGraphQuery::new(
        &fixture.graph_sidecar,
        fixture.config.query_limits.clone(),
        Some(fixture.graph_freshness.clone()),
    );
    let memory =
        SqliteMemoryQuery::new(&fixture.memory_sidecar, fixture.config.query_limits.clone());
    let service = service(&fixture, &graph, &memory);
    let mut page_budget = budget(&fixture.config);
    page_budget.max_results = NonZeroU32::new(1).unwrap();
    let mut request = FederatedSearchRequest {
        scope: super::super::federation::FederatedScope::current(
            fixture.project.clone(),
            target(&fixture, ContextDomain::All),
            page_budget,
        ),
        text: MemoryQueryText::new("federat").unwrap(),
        repository_kinds: vec![],
        repository_paths: vec![],
        memory_kinds: vec![],
        memory_sources: vec![],
        cursor: None,
    };
    let first = service.search(request.clone()).unwrap();
    assert_eq!(first.results.len(), 1);
    request.cursor = first.page.next_cursor.clone();
    assert!(request.cursor.is_some());
    let second = service.search(request).unwrap();
    assert_eq!(second.results.len(), 1);
    assert_ne!(
        search_result_key(&first.results[0]),
        search_result_key(&second.results[0])
    );
}

#[test]
fn combined_search_counts_the_complete_response_against_the_byte_budget() {
    let fixture = fixture();
    let graph = SqliteGraphQuery::new(
        &fixture.graph_sidecar,
        fixture.config.query_limits.clone(),
        Some(fixture.graph_freshness.clone()),
    );
    let memory =
        SqliteMemoryQuery::new(&fixture.memory_sidecar, fixture.config.query_limits.clone());
    let service = service(&fixture, &graph, &memory);
    let request = |query_budget| FederatedSearchRequest {
        scope: FederatedScope::current(
            fixture.project.clone(),
            target(&fixture, ContextDomain::All),
            query_budget,
        ),
        text: MemoryQueryText::new("federat").unwrap(),
        repository_kinds: vec![],
        repository_paths: vec![],
        memory_kinds: vec![],
        memory_sources: vec![],
        cursor: None,
    };

    let complete = service.search(request(budget(&fixture.config))).unwrap();
    assert!(complete.results.len() > 1);

    let mut one_result_budget = budget(&fixture.config);
    one_result_budget.max_results = NonZeroU32::new(1).unwrap();
    let one_result = service.search(request(one_result_budget)).unwrap();
    let one_response_bytes = one_result
        .page
        .truncation
        .as_ref()
        .expect("one-result response must be truncated")
        .returned_bytes;
    assert_eq!(
        one_response_bytes,
        serde_json::to_vec(&one_result).unwrap().len() as u64
    );

    let mut constrained_budget = budget(&fixture.config);
    constrained_budget.max_bytes = NonZeroU64::new(one_response_bytes).unwrap();
    let constrained = service.search(request(constrained_budget)).unwrap();
    assert_eq!(constrained.results.len(), 1);
    let encoded_bytes = serde_json::to_vec(&constrained).unwrap().len() as u64;
    assert!(encoded_bytes <= one_response_bytes);
    assert_eq!(
        constrained
            .page
            .truncation
            .as_ref()
            .expect("response must report byte truncation")
            .returned_bytes,
        encoded_bytes
    );

    let bare_result_bytes = serialized_len(&complete.results[0]).unwrap();
    let mut insufficient_budget = budget(&fixture.config);
    insufficient_budget.max_bytes = NonZeroU64::new(bare_result_bytes + 1).unwrap();
    assert_eq!(
        service.search(request(insufficient_budget)).unwrap_err(),
        MemoryQueryError::BudgetExceeded(MemoryTruncationReason::Bytes)
    );
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
        revision_key(response.repository.as_ref(), response.memory.as_ref());
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

#[test]
fn approved_history_reduces_raw_artifact_reading_without_displacing_source_evidence() {
    let corpus: EvaluationCorpus = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/project_memory_eval/cases.json"
    )))
    .unwrap();
    assert_eq!(corpus.version, 1);
    let fixture = fixture();
    let graph = SqliteGraphQuery::new(
        &fixture.graph_sidecar,
        fixture.config.query_limits.clone(),
        Some(fixture.graph_freshness.clone()),
    );
    let memory =
        SqliteMemoryQuery::new(&fixture.memory_sidecar, fixture.config.query_limits.clone());
    let service = service(&fixture, &graph, &memory);

    for case in corpus.cases {
        assert!(case.baseline_raw_artifact_reads > 0, "{}", case.id);
        let search = memory
            .search(MemorySearchRequest {
                scope: MemoryQueryScope::current(
                    fixture.project.clone(),
                    MemoryRevisionSelector::Published(MemoryViewName::new("project").unwrap()),
                    budget(&fixture.config),
                ),
                text: MemoryQueryText::new(case.search_query).unwrap(),
                entity_kinds: vec![],
                source_categories: vec![],
                page: MemoryPageRequest::default(),
            })
            .unwrap();
        let historical = search
            .hits
            .iter()
            .find(|hit| {
                matches!(
                    &hit.entity.data,
                    MemoryEntityData::Outcome { text }
                        if text.as_str().contains(&case.expected_memory_text)
                )
            })
            .unwrap_or_else(|| panic!("{} did not find approved history", case.id));
        assert_eq!(
            historical.entity.provenance.source_category,
            MemorySourceCategory::ApprovedOutcome
        );

        let response = service
            .context(FederatedContextRequest {
                scope: FederatedScope::current(
                    fixture.project.clone(),
                    target(&fixture, ContextDomain::All),
                    budget(&fixture.config),
                ),
                seeds: vec![FederatedContextSeed::MemoryEntity(
                    historical.entity.id.clone(),
                )],
                repository_policy: ContextPolicy {
                    direction: EdgeDirection::Both,
                    edge_kinds: vec![],
                    include_unresolved: false,
                    include_external: false,
                },
                memory_policy: MemoryContextPolicy {
                    direction: EdgeDirection::Both,
                    relationship_kinds: vec![],
                    include_unresolved: false,
                    include_stale: false,
                    include_snippets: false,
                },
                cursor: None,
            })
            .unwrap();
        assert!(matches!(
            response.items.first(),
            Some(FederatedContextItem::Repository(_))
        ));
        let repository_position = response
            .items
            .iter()
            .position(|item| {
                matches!(
                    item,
                    FederatedContextItem::Repository(item)
                        if item.path.as_str() == case.expected_repository_path
                )
            })
            .unwrap();
        let memory_position = response
            .items
            .iter()
            .position(|item| {
                matches!(
                    item,
                    FederatedContextItem::Memory(item)
                        if item.entity.id == historical.entity.id
                )
            })
            .unwrap();
        assert!(repository_position < memory_position);
        assert!(
            response
                .cross_domain_links
                .iter()
                .all(|link| { link.provenance.resolution == MemoryResolutionState::Resolved })
        );
        assert_eq!(
            response.repository.as_ref().unwrap().freshness.freshness,
            crate::repository_graph::domain::Freshness::Fresh
        );
        assert_eq!(
            response.memory.as_ref().unwrap().freshness.freshness,
            super::super::query::MemoryFreshness::Fresh
        );
        let serialized = serde_json::to_string(&response).unwrap();
        for marker in case.forbidden_raw_markers {
            assert!(
                !serialized.contains(&marker),
                "{} exposed {marker}",
                case.id
            );
        }
    }
}
