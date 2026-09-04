//! Regression tests for CLI graph identity, freshness, evidence paths, and domain filters.

use super::*;

#[test]
fn build_ids_are_opaque_and_unique_per_process() {
    let first = next_build_id();
    let second = next_build_id();
    assert_ne!(first, second);
    assert!(first.as_str().starts_with("build:"));
}

#[test]
fn requested_budgets_reject_zero_cli_values() {
    let config = RepositoryGraphConfig::default();
    assert!(requested_budget(&config, Some(0), None, None).is_err());
    assert!(requested_budget(&config, None, Some(0), None).is_err());
    assert!(requested_budget(&config, None, None, Some(0)).is_err());
}

#[test]
fn only_the_published_snapshot_can_record_canonical_freshness() {
    let repository = crate::repository_graph::domain::RepositoryRef {
        namespace: crate::repository_graph::domain::RepositoryNamespace::new("local:test").unwrap(),
        repository_id: crate::repository_graph::domain::RepositoryId::new("root").unwrap(),
    };
    let published_snapshot =
        crate::repository_graph::domain::SnapshotId::new("snapshot-published").unwrap();
    let losing_snapshot =
        crate::repository_graph::domain::SnapshotId::new("snapshot-losing").unwrap();
    let view = crate::repository_graph::store::PublishedView {
        repository,
        view_name: PublishedViewName::new(CANONICAL_VIEW).unwrap(),
        snapshot_id: published_snapshot.clone(),
        build_id: BuildId::new("build-published").unwrap(),
        generation: 2,
    };

    assert!(publication_matches_snapshot(
        &PublicationOutcome::Published { view: view.clone() },
        &published_snapshot,
    ));
    assert!(!publication_matches_snapshot(
        &PublicationOutcome::Superseded { current: view },
        &losing_snapshot,
    ));
    assert_eq!(reported_index_freshness(true, true), Freshness::Fresh);
    assert_eq!(reported_index_freshness(true, false), Freshness::Unknown);
    assert_eq!(reported_index_freshness(false, false), Freshness::Unknown);
}

#[test]
fn evidence_locations_are_repository_relative() {
    let path = RepoPath::new("src/main.rs").unwrap();
    assert_eq!(evidence_location(Some(&path), None), "src/main.rs");
}

#[test]
fn combined_kind_filters_exclude_the_unselected_domain() {
    let (domain, repository_kinds, memory_kinds) =
        federated_kind_filters(GraphDomain::All, vec!["decision".to_string()]).unwrap();
    assert_eq!(domain, GraphDomain::Memory);
    assert!(repository_kinds.is_empty());
    assert_eq!(memory_kinds, vec![MemoryEntityKind::Decision]);

    let (domain, repository_kinds, memory_kinds) =
        federated_kind_filters(GraphDomain::All, vec!["struct".to_string()]).unwrap();
    assert_eq!(domain, GraphDomain::Repository);
    assert_eq!(repository_kinds[0].as_str(), "struct");
    assert!(memory_kinds.is_empty());

    let (domain, repository_kinds, memory_kinds) = federated_kind_filters(
        GraphDomain::All,
        vec!["decision".to_string(), "struct".to_string()],
    )
    .unwrap();
    assert_eq!(domain, GraphDomain::All);
    assert_eq!(repository_kinds[0].as_str(), "struct");
    assert_eq!(memory_kinds, vec![MemoryEntityKind::Decision]);
}
