use std::{cell::Cell, fs, path::Path, process::Command};

use tempfile::TempDir;

use crate::project_memory::{
    domain::{MemoryRecordId, MemoryViewName, ProjectId, ProjectNamespace, ProjectRef},
    index::{MemoryIndexOptions, MemoryIndexer},
    policy::MemoryPolicy,
    ports::MemorySource,
    query::{MemoryFreshnessComparison, MemoryPageRequest, MemoryQueryScope},
    source::LocalMemorySource,
    sqlite::{MEMORY_SIDECAR_FILE_NAME, OpenMemoryQuerySidecarResult, open_for_query_at},
};
use crate::repository_graph::{domain::RepoPath, query::EdgeDirection};

use super::*;

fn project() -> ProjectRef {
    ProjectRef {
        namespace: ProjectNamespace::new("local:test").unwrap(),
        project_id: ProjectId::new("query-project").unwrap(),
    }
}

fn initialize_repository(root: &Path) {
    assert!(
        Command::new("git")
            .arg("init")
            .arg(root)
            .status()
            .unwrap()
            .success()
    );
    fs::create_dir_all(root.join("docs/specs")).unwrap();
    fs::write(
        root.join("docs/specs/query.md"),
        "# Query memory\n\n- [x] #4.4 Federation\n\nID: rg-test\nDepends on: none\n\n## Outcome\n\nDelivered bounded SQLite retrieval.\n\n### Decisions\n\nUse deterministic ranking.\n",
    )
    .unwrap();
    assert!(
        Command::new("git")
            .current_dir(root)
            .args(["add", "--", "docs/specs/query.md"])
            .status()
            .unwrap()
            .success()
    );
}

fn indexed_fixture() -> (TempDir, TempDir, ProjectRef, MemoryRevisionId) {
    let root = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    initialize_repository(root.path());
    let project = project();
    crate::project_memory::source::record_approved_outcome_for_test(
        data.path(),
        "docs/specs/query.md",
        &fs::read_to_string(root.path().join("docs/specs/query.md")).unwrap(),
    );
    let source = LocalMemorySource::discover_at(
        root.path().to_path_buf(),
        data.path().to_path_buf(),
        project.clone(),
        RepoPath::new("docs/specs").unwrap(),
        MemoryPolicy::default(),
    )
    .unwrap();
    let mut sidecar = MemorySidecar::open_at(data.path()).unwrap();
    let outcome = MemoryIndexer::new(&source, &mut sidecar)
        .unwrap()
        .index(MemoryIndexOptions::default())
        .unwrap();
    (root, data, project, outcome.revision.id)
}

fn published_scope(project: ProjectRef, limits: &QueryLimitsConfig) -> MemoryQueryScope {
    MemoryQueryScope::current(
        project,
        super::super::query::MemoryRevisionSelector::Published(
            MemoryViewName::new("project").unwrap(),
        ),
        default_budget(limits).unwrap(),
    )
}

struct RecordingContent {
    max_requested: Cell<u64>,
    max_duration: Cell<Duration>,
}

struct ExpiredContent;

impl MemoryContent for ExpiredContent {
    fn content(
        &self,
        _request: MemoryContentRequest,
    ) -> Result<super::super::query::MemoryContentResponse, MemoryQueryError> {
        unreachable!("deadline-aware context reads use content_with_deadline")
    }

    fn content_with_deadline(
        &self,
        _request: MemoryContentRequest,
        _max_duration: Duration,
    ) -> Result<super::super::query::MemoryContentResponse, MemoryQueryError> {
        Err(MemoryQueryError::BudgetExceeded(
            MemoryTruncationReason::Duration,
        ))
    }
}

impl MemoryContent for RecordingContent {
    fn content(
        &self,
        request: MemoryContentRequest,
    ) -> Result<super::super::query::MemoryContentResponse, MemoryQueryError> {
        Ok(super::super::query::MemoryContentResponse {
            verified_fingerprint: request.expected_fingerprint,
            bytes: vec![b'x'; request.max_bytes.get() as usize],
            truncated: true,
        })
    }

    fn content_with_deadline(
        &self,
        request: MemoryContentRequest,
        max_duration: Duration,
    ) -> Result<super::super::query::MemoryContentResponse, MemoryQueryError> {
        self.max_requested
            .set(self.max_requested.get().max(request.max_bytes.get()));
        self.max_duration.set(max_duration);
        self.content(request)
    }
}

#[test]
fn cursor_rejects_a_different_request_fingerprint() {
    let revision = MemoryRevisionId::new("memory:revision").unwrap();
    let cursor = encode_cursor("search", &revision, "one", 2).unwrap();
    assert_eq!(
        decode_cursor(Some(&cursor), "search", &revision, "two"),
        Err(MemoryQueryError::StaleCursor)
    );
}

#[test]
fn effective_budget_clamps_every_caller_value() {
    use std::num::{NonZeroU32, NonZeroU64};

    let requested = MemoryQueryBudget {
        max_results: NonZeroU32::new(1_000).unwrap(),
        max_bytes: NonZeroU64::new(1_000_000).unwrap(),
        max_snippet_bytes: NonZeroU64::new(1_000_000).unwrap(),
        max_depth: NonZeroU32::new(100).unwrap(),
        max_duration_ms: NonZeroU64::new(100_000).unwrap(),
        max_diagnostics: NonZeroU32::new(1_000).unwrap(),
    };
    let limits = QueryLimitsConfig::default();
    let effective = EffectiveBudget::new(&requested, &limits);
    assert_eq!(effective.max_results, limits.max_results);
    assert_eq!(effective.max_bytes, limits.max_bytes);
    assert_eq!(effective.max_snippet_bytes, limits.max_snippet_bytes);
    assert_eq!(effective.max_depth, limits.max_depth);
    assert_eq!(effective.max_duration_ms, limits.max_duration_ms);
    assert_eq!(effective.max_diagnostics, limits.max_diagnostics);
}

#[test]
fn status_reports_source_policy_retention_and_stale_archive_freshness() {
    let (root, data, project, first_revision) = indexed_fixture();
    fs::write(
        root.path().join("docs/specs/query.md"),
        "# Query memory\n\n- [x] #4.4 Federation\n\nID: rg-test\nDepends on: none\n\n## Outcome\n\nDelivered a changed archive outcome.\n",
    )
    .unwrap();
    assert!(
        Command::new("git")
            .current_dir(root.path())
            .args(["add", "--", "docs/specs/query.md"])
            .status()
            .unwrap()
            .success()
    );
    crate::project_memory::source::record_approved_outcome_for_test(
        data.path(),
        "docs/specs/query.md",
        &fs::read_to_string(root.path().join("docs/specs/query.md")).unwrap(),
    );
    let changed_source = LocalMemorySource::discover_at(
        root.path().to_path_buf(),
        data.path().to_path_buf(),
        project.clone(),
        RepoPath::new("docs/specs").unwrap(),
        MemoryPolicy::default(),
    )
    .unwrap();
    let comparison = MemoryFreshnessComparison::from_manifest(&changed_source.manifest().unwrap());
    let OpenMemoryQuerySidecarResult::Ready(sidecar) =
        open_for_query_at(&data.path().join(MEMORY_SIDECAR_FILE_NAME)).unwrap()
    else {
        panic!("memory query sidecar should be ready");
    };
    let limits = QueryLimitsConfig::default();
    let mut scope = published_scope(project.clone(), &limits);
    scope.freshness_comparison = Some(comparison);
    let status = SqliteMemoryQuery::new(&sidecar, limits.clone())
        .status(MemoryStatusRequest { scope })
        .unwrap();
    assert_eq!(status.revision_id, Some(first_revision.clone()));
    assert_eq!(status.freshness.freshness, MemoryFreshness::Stale);
    assert_eq!(
        status.data.recommended_action,
        Some(MemoryRetrievalAction::Refresh)
    );
    assert_eq!(
        status.data.source_policy.len(),
        MemorySourceCategory::ALL.len()
    );
    assert_eq!(
        status
            .data
            .source_policy
            .iter()
            .filter(|source| source.policy.enabled)
            .count(),
        4
    );
    assert!(status.data.source_policy.iter().all(|source| {
        source.policy.enabled
            || source.policy.sensitivity == super::super::policy::MemorySourceSensitivity::Sensitive
    }));
    let retention = status.data.retention.unwrap();
    assert_eq!(retention.revisions, 1);
    assert_eq!(retention.historical_revisions, 0);
    assert_eq!(retention.builds, 1);
    assert_eq!(retention.terminal_unpublished_builds, 0);

    drop(sidecar);
    let mut writable = MemorySidecar::open_at(data.path()).unwrap();
    let changed = MemoryIndexer::new(&changed_source, &mut writable)
        .unwrap()
        .index(MemoryIndexOptions::default())
        .unwrap();
    assert_ne!(changed.revision.id, first_revision);
    drop(writable);
    let OpenMemoryQuerySidecarResult::Ready(sidecar) =
        open_for_query_at(&data.path().join(MEMORY_SIDECAR_FILE_NAME)).unwrap()
    else {
        panic!("memory query sidecar should be ready");
    };
    let status = SqliteMemoryQuery::new(&sidecar, limits)
        .status(MemoryStatusRequest {
            scope: published_scope(project, &QueryLimitsConfig::default()),
        })
        .unwrap();
    let retention = status.data.retention.unwrap();
    assert_eq!(retention.revisions, 2);
    assert_eq!(retention.historical_revisions, 1);
    assert_eq!(retention.builds, 2);
    assert_eq!(retention.terminal_unpublished_builds, 1);
    assert!(retention.repository_link_sets >= 2);
}

#[test]
fn search_and_context_are_revision_bound_and_deterministic() {
    let (_root, data, project, revision_id) = indexed_fixture();
    let OpenMemoryQuerySidecarResult::Ready(sidecar) =
        open_for_query_at(&data.path().join(MEMORY_SIDECAR_FILE_NAME)).unwrap()
    else {
        panic!("memory query sidecar should be ready");
    };
    let limits = QueryLimitsConfig::default();
    let query = SqliteMemoryQuery::new(&sidecar, limits.clone());
    let search = query
        .search(MemorySearchRequest {
            scope: published_scope(project.clone(), &limits),
            text: super::super::domain::MemoryQueryText::new("sqlite").unwrap(),
            entity_kinds: vec![],
            source_categories: vec![],
            page: MemoryPageRequest::default(),
        })
        .unwrap();
    assert_eq!(search.revision_id, revision_id);
    assert!(!search.hits.is_empty());
    assert!(
        search
            .hits
            .windows(2)
            .all(|pair| { memory_hit_order(&pair[0], &pair[1]) != std::cmp::Ordering::Greater })
    );

    let context = query
        .context(MemoryContextRequest {
            scope: published_scope(project, &limits),
            seeds: vec![MemoryContextSeed::Milestone(
                MemoryRecordId::new("rg-test").unwrap(),
            )],
            policy: super::super::query::MemoryContextPolicy {
                direction: crate::repository_graph::query::EdgeDirection::Both,
                relationship_kinds: vec![],
                include_unresolved: false,
                include_stale: false,
                include_snippets: false,
            },
            page: MemoryPageRequest::default(),
        })
        .unwrap();
    assert!(context.items.len() >= 2);
    assert!(!context.relationships.is_empty());
}

#[test]
fn context_traversal_honors_relationship_direction() {
    let (_root, data, project, revision_id) = indexed_fixture();
    let OpenMemoryQuerySidecarResult::Ready(sidecar) =
        open_for_query_at(&data.path().join(MEMORY_SIDECAR_FILE_NAME)).unwrap()
    else {
        panic!("memory query sidecar should be ready");
    };
    let limits = QueryLimitsConfig::default();
    let query = SqliteMemoryQuery::new(&sidecar, limits.clone());
    let relationship = query
        .relationships(&revision_id)
        .unwrap()
        .into_iter()
        .find(|relationship| {
            matches!(
                relationship.target,
                MemoryRelationshipTarget::MemoryEntity { ref entity_id }
                    if entity_id != &relationship.source
            )
        })
        .expect("fixture should contain a directed entity relationship");
    let MemoryRelationshipTarget::MemoryEntity { entity_id: target } = relationship.target.clone()
    else {
        unreachable!("relationship was selected by target kind");
    };
    let source = relationship.source.clone();
    let relationship_id = relationship.id.clone();

    let context = |seed, direction| {
        query
            .context(MemoryContextRequest {
                scope: published_scope(project.clone(), &limits),
                seeds: vec![MemoryContextSeed::Entity(seed)],
                policy: super::super::query::MemoryContextPolicy {
                    direction,
                    relationship_kinds: vec![],
                    include_unresolved: false,
                    include_stale: false,
                    include_snippets: false,
                },
                page: MemoryPageRequest::default(),
            })
            .unwrap()
    };

    let outgoing_from_source = context(source.clone(), EdgeDirection::Outgoing);
    assert!(
        outgoing_from_source
            .items
            .iter()
            .any(|item| item.entity.id == target)
    );
    assert!(
        outgoing_from_source
            .relationships
            .iter()
            .any(|item| item.id == relationship_id)
    );

    let outgoing_from_target = context(target.clone(), EdgeDirection::Outgoing);
    assert!(
        outgoing_from_target
            .items
            .iter()
            .all(|item| item.entity.id != source)
    );
    assert!(
        outgoing_from_target
            .relationships
            .iter()
            .all(|item| item.id != relationship_id)
    );

    let incoming_from_target = context(target.clone(), EdgeDirection::Incoming);
    assert!(
        incoming_from_target
            .items
            .iter()
            .any(|item| item.entity.id == source)
    );
    assert!(
        incoming_from_target
            .relationships
            .iter()
            .any(|item| item.id == relationship_id)
    );

    let incoming_from_source = context(source, EdgeDirection::Incoming);
    assert!(
        incoming_from_source
            .items
            .iter()
            .all(|item| item.entity.id != target)
    );
    assert!(
        incoming_from_source
            .relationships
            .iter()
            .all(|item| item.id != relationship_id)
    );
}

#[test]
fn search_candidate_ranking_stops_when_the_deadline_expires() {
    let (_root, data, project, revision_id) = indexed_fixture();
    let OpenMemoryQuerySidecarResult::Ready(sidecar) =
        open_for_query_at(&data.path().join(MEMORY_SIDECAR_FILE_NAME)).unwrap()
    else {
        panic!("memory query sidecar should be ready");
    };
    let limits = QueryLimitsConfig::default();
    let query = SqliteMemoryQuery::new(&sidecar, limits.clone());
    let request = MemorySearchRequest {
        scope: published_scope(project, &limits),
        text: super::super::domain::MemoryQueryText::new("sqlite").unwrap(),
        entity_kinds: vec![],
        source_categories: vec![],
        page: MemoryPageRequest::default(),
    };
    let entities = query.entities(&revision_id).unwrap();
    let mut deadline_checks = 0;

    let (hits, timed_out) = rank_memory_search_hits(entities, &request, || {
        deadline_checks += 1;
        deadline_checks > 1
    });

    assert!(timed_out);
    assert_eq!(deadline_checks, 2);
    assert!(hits.len() <= 1);
}

#[test]
fn context_counts_relationships_against_the_effective_result_limit() {
    let (_root, data, project, revision_id) = indexed_fixture();
    let OpenMemoryQuerySidecarResult::Ready(sidecar) =
        open_for_query_at(&data.path().join(MEMORY_SIDECAR_FILE_NAME)).unwrap()
    else {
        panic!("memory query sidecar should be ready");
    };
    let limits = QueryLimitsConfig::default();
    let query = SqliteMemoryQuery::new(&sidecar, limits.clone());
    let relationship = query
        .relationships(&revision_id)
        .unwrap()
        .into_iter()
        .find(|relationship| {
            matches!(
                relationship.target,
                MemoryRelationshipTarget::MemoryEntity { ref entity_id }
                    if entity_id != &relationship.source
            )
        })
        .expect("fixture should contain a directed entity relationship");
    let relationship_id = relationship.id.clone();
    let request = |cursor| {
        let mut scope = published_scope(project.clone(), &limits);
        scope.budget.max_results = std::num::NonZeroU32::new(1).unwrap();
        MemoryContextRequest {
            scope,
            seeds: vec![MemoryContextSeed::Entity(relationship.source.clone())],
            policy: super::super::query::MemoryContextPolicy {
                direction: crate::repository_graph::query::EdgeDirection::Outgoing,
                relationship_kinds: vec![],
                include_unresolved: false,
                include_stale: false,
                include_snippets: false,
            },
            page: MemoryPageRequest { cursor },
        }
    };
    let mut response = query.context(request(None)).unwrap();

    assert!(
        response
            .items
            .len()
            .saturating_add(response.relationships.len())
            <= 1
    );
    assert_eq!(response.relationships.len(), 0);
    assert_eq!(
        response.page.truncation.as_ref().map(|value| value.reason),
        Some(MemoryTruncationReason::Results)
    );
    let mut pages = 1;
    while response.relationships.is_empty() {
        let cursor = response
            .page
            .next_cursor
            .clone()
            .expect("context relationships should remain pageable");
        response = query.context(request(Some(cursor))).unwrap();
        pages += 1;
        assert!(pages < 32, "context cursor should make progress");
    }
    assert_eq!(response.relationships.len(), 1);
    assert_eq!(response.relationships[0].id, relationship_id);
}

#[test]
fn context_pages_relationships_beyond_the_first_page_result_limit() {
    let (_root, data, project, revision_id) = indexed_fixture();
    let OpenMemoryQuerySidecarResult::Ready(sidecar) =
        open_for_query_at(&data.path().join(MEMORY_SIDECAR_FILE_NAME)).unwrap()
    else {
        panic!("memory query sidecar should be ready");
    };
    let limits = QueryLimitsConfig::default();
    let query = SqliteMemoryQuery::new(&sidecar, limits.clone());
    let relationships = query.relationships(&revision_id).unwrap();
    let mut by_source = std::collections::BTreeMap::<_, Vec<_>>::new();
    for relationship in relationships {
        if matches!(
            relationship.target,
            MemoryRelationshipTarget::MemoryEntity { .. }
        ) {
            by_source
                .entry(relationship.source.clone())
                .or_default()
                .push(relationship);
        }
    }
    let (source, expected_relationships) = by_source
        .into_iter()
        .find(|(_, relationships)| relationships.len() > 1)
        .expect("fixture should contain a source with multiple outgoing relationships");
    let expected_relationship_ids = expected_relationships
        .iter()
        .map(|relationship| relationship.id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let expected_neighbor_ids = expected_relationships
        .iter()
        .filter_map(|relationship| match &relationship.target {
            MemoryRelationshipTarget::MemoryEntity { entity_id } => Some(entity_id.clone()),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    let request = |cursor| {
        let mut scope = published_scope(project.clone(), &limits);
        scope.budget.max_results = std::num::NonZeroU32::new(1).unwrap();
        scope.budget.max_depth = std::num::NonZeroU32::new(1).unwrap();
        MemoryContextRequest {
            scope,
            seeds: vec![MemoryContextSeed::Entity(source.clone())],
            policy: super::super::query::MemoryContextPolicy {
                direction: crate::repository_graph::query::EdgeDirection::Outgoing,
                relationship_kinds: vec![],
                include_unresolved: false,
                include_stale: false,
                include_snippets: false,
            },
            page: MemoryPageRequest { cursor },
        }
    };
    let mut cursor = None;
    let mut returned_relationship_ids = std::collections::BTreeSet::new();
    let mut returned_entity_ids = std::collections::BTreeSet::new();
    let mut pages = 0;
    loop {
        let response = query.context(request(cursor)).unwrap();
        returned_entity_ids.extend(response.items.into_iter().map(|item| item.entity.id));
        returned_relationship_ids.extend(
            response
                .relationships
                .into_iter()
                .map(|relationship| relationship.id),
        );
        pages += 1;
        assert!(pages < 32, "context cursor should make progress");
        let Some(next_cursor) = response.page.next_cursor else {
            break;
        };
        cursor = Some(next_cursor);
    }

    assert_eq!(returned_relationship_ids, expected_relationship_ids);
    assert!(expected_neighbor_ids.is_subset(&returned_entity_ids));
}

#[test]
fn context_candidate_cap_allows_edges_between_selected_entities() {
    assert!(!context_candidate_limit_reached(
        MAX_CONTEXT_CANDIDATES,
        true,
    ));
    assert!(context_candidate_limit_reached(
        MAX_CONTEXT_CANDIDATES,
        false,
    ));
}

#[test]
fn context_counts_relationships_against_the_effective_byte_limit() {
    let (_root, data, project, _) = indexed_fixture();
    let OpenMemoryQuerySidecarResult::Ready(sidecar) =
        open_for_query_at(&data.path().join(MEMORY_SIDECAR_FILE_NAME)).unwrap()
    else {
        panic!("memory query sidecar should be ready");
    };
    let limits = QueryLimitsConfig::default();
    let query = SqliteMemoryQuery::new(&sidecar, limits.clone());
    let request = |scope| MemoryContextRequest {
        scope,
        seeds: vec![MemoryContextSeed::Milestone(
            MemoryRecordId::new("rg-test").unwrap(),
        )],
        policy: super::super::query::MemoryContextPolicy {
            direction: crate::repository_graph::query::EdgeDirection::Both,
            relationship_kinds: vec![],
            include_unresolved: false,
            include_stale: false,
            include_snippets: false,
        },
        page: MemoryPageRequest::default(),
    };
    let complete = query
        .context(request(published_scope(project.clone(), &limits)))
        .unwrap();
    assert!(!complete.relationships.is_empty());
    let item_bytes = complete
        .items
        .iter()
        .map(serialized_len)
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .into_iter()
        .sum::<u64>();
    let mut constrained_scope = published_scope(project, &limits);
    constrained_scope.budget.max_bytes = std::num::NonZeroU64::new(item_bytes).unwrap();

    let constrained = query.context(request(constrained_scope)).unwrap();
    let returned_bytes = constrained
        .items
        .iter()
        .map(serialized_len)
        .chain(constrained.relationships.iter().map(serialized_len))
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .into_iter()
        .sum::<u64>();
    assert!(returned_bytes <= item_bytes);
    assert_eq!(
        constrained
            .page
            .truncation
            .as_ref()
            .map(|value| value.returned_bytes),
        Some(returned_bytes)
    );
    assert_eq!(
        constrained.page.truncation.map(|value| value.reason),
        Some(MemoryTruncationReason::Bytes)
    );
}

#[test]
fn context_snippets_use_the_verified_content_boundary_and_effective_cap() {
    let (_root, data, project, _) = indexed_fixture();
    let OpenMemoryQuerySidecarResult::Ready(sidecar) =
        open_for_query_at(&data.path().join(MEMORY_SIDECAR_FILE_NAME)).unwrap()
    else {
        panic!("memory query sidecar should be ready");
    };
    let limits = QueryLimitsConfig {
        max_snippet_bytes: 7,
        ..QueryLimitsConfig::default()
    };
    let content = RecordingContent {
        max_requested: Cell::new(0),
        max_duration: Cell::new(Duration::ZERO),
    };
    let query = SqliteMemoryQuery::new(&sidecar, limits.clone()).with_content(&content);
    let response = query
        .context(MemoryContextRequest {
            scope: published_scope(project, &limits),
            seeds: vec![MemoryContextSeed::Milestone(
                MemoryRecordId::new("rg-test").unwrap(),
            )],
            policy: super::super::query::MemoryContextPolicy {
                direction: crate::repository_graph::query::EdgeDirection::Both,
                relationship_kinds: vec![],
                include_unresolved: false,
                include_stale: false,
                include_snippets: true,
            },
            page: MemoryPageRequest::default(),
        })
        .unwrap();
    let snippet_bytes = response
        .items
        .iter()
        .filter_map(|item| item.snippet.as_ref())
        .map(|snippet| snippet.text.len())
        .sum::<usize>();
    assert_eq!(snippet_bytes, 7);
    assert_eq!(content.max_requested.get(), 7);
    assert!(content.max_duration.get() <= Duration::from_millis(limits.max_duration_ms));
    assert!(!content.max_duration.get().is_zero());
}

#[test]
fn context_stops_before_accepting_a_snippet_that_exceeds_the_deadline() {
    let (_root, data, project, _) = indexed_fixture();
    let OpenMemoryQuerySidecarResult::Ready(sidecar) =
        open_for_query_at(&data.path().join(MEMORY_SIDECAR_FILE_NAME)).unwrap()
    else {
        panic!("memory query sidecar should be ready");
    };
    let limits = QueryLimitsConfig::default();
    let response = SqliteMemoryQuery::new(&sidecar, limits.clone())
        .with_content(&ExpiredContent)
        .context(MemoryContextRequest {
            scope: published_scope(project, &limits),
            seeds: vec![MemoryContextSeed::Milestone(
                MemoryRecordId::new("rg-test").unwrap(),
            )],
            policy: super::super::query::MemoryContextPolicy {
                direction: EdgeDirection::Both,
                relationship_kinds: vec![],
                include_unresolved: false,
                include_stale: false,
                include_snippets: true,
            },
            page: MemoryPageRequest::default(),
        })
        .unwrap();

    assert!(response.items.is_empty());
    assert_eq!(
        response.page.truncation.map(|value| value.reason),
        Some(MemoryTruncationReason::Duration)
    );
}

#[test]
fn query_open_is_absent_or_read_only_and_never_creates_or_migrates() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join(MEMORY_SIDECAR_FILE_NAME);
    assert!(matches!(
        open_for_query_at(&path).unwrap(),
        OpenMemoryQuerySidecarResult::Absent
    ));
    assert!(!path.exists());

    drop(MemorySidecar::open(path.clone()).unwrap());
    let OpenMemoryQuerySidecarResult::Ready(sidecar) = open_for_query_at(&path).unwrap() else {
        panic!("current memory sidecar should be queryable");
    };
    assert!(
        sidecar
            .connection()
            .execute("DELETE FROM memory_builds", [])
            .is_err()
    );
}
