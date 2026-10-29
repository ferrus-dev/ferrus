//! Tests for authorization.

use super::*;

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
            || source.policy.sensitivity
                == super::super::super::policy::MemorySourceSensitivity::Sensitive
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
