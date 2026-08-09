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
use crate::repository_graph::domain::RepoPath;

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
}

impl MemoryContent for RecordingContent {
    fn content(
        &self,
        request: MemoryContentRequest,
    ) -> Result<super::super::query::MemoryContentResponse, MemoryQueryError> {
        self.max_requested
            .set(self.max_requested.get().max(request.max_bytes.get()));
        Ok(super::super::query::MemoryContentResponse {
            verified_fingerprint: request.expected_fingerprint,
            bytes: vec![b'x'; request.max_bytes.get() as usize],
            truncated: true,
        })
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
fn context_bounds_relationship_expansion_by_the_effective_result_limit() {
    let (_root, data, project, _) = indexed_fixture();
    let OpenMemoryQuerySidecarResult::Ready(sidecar) =
        open_for_query_at(&data.path().join(MEMORY_SIDECAR_FILE_NAME)).unwrap()
    else {
        panic!("memory query sidecar should be ready");
    };
    let limits = QueryLimitsConfig::default();
    let mut scope = published_scope(project, &limits);
    scope.budget.max_results = std::num::NonZeroU32::new(1).unwrap();
    let response = SqliteMemoryQuery::new(&sidecar, limits)
        .context(MemoryContextRequest {
            scope,
            seeds: vec![MemoryContextSeed::Milestone(
                MemoryRecordId::new("rg-test").unwrap(),
            )],
            policy: super::super::query::MemoryContextPolicy {
                relationship_kinds: vec![],
                include_unresolved: false,
                include_stale: false,
                include_snippets: false,
            },
            page: MemoryPageRequest::default(),
        })
        .unwrap();

    assert_eq!(response.relationships.len(), 1);
    assert_eq!(
        response.page.truncation.map(|value| value.reason),
        Some(MemoryTruncationReason::Results)
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
    };
    let query = SqliteMemoryQuery::new(&sidecar, limits.clone()).with_content(&content);
    let response = query
        .context(MemoryContextRequest {
            scope: published_scope(project, &limits),
            seeds: vec![MemoryContextSeed::Milestone(
                MemoryRecordId::new("rg-test").unwrap(),
            )],
            policy: super::super::query::MemoryContextPolicy {
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
