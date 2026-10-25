//! Explicit evidence parsing and deadline enforcement during link resolution.

use std::cell::Cell;

use super::*;
use crate::{
    project_memory::domain::{MemoryRevisionId, ProjectId, ProjectNamespace},
    repository_graph::domain::{
        Confidence, Digest, ExtractorId, ExtractorIdentity, FactProvenance, ResolutionState,
    },
};

#[test]
fn only_explicit_inline_repository_references_are_accepted() {
    let references = explicit_references(
        "Use src/lib.rs, `path:src/lib.rs`, `symbol:rust:function:src/lib.rs:run`, and `https://example.com`.",
    ).unwrap();
    assert_eq!(references.len(), 2);
    assert!(matches!(
        references[0],
        ExplicitRepositoryReference::Path(_)
    ));
    assert!(matches!(
        references[1],
        ExplicitRepositoryReference::Symbol(_)
    ));
}

#[test]
fn snapshot_link_resolution_checks_the_deadline_during_node_scans() {
    let snapshot_id = SnapshotId::new("snapshot").unwrap();
    let revision = MemoryRevision {
        id: MemoryRevisionId::new("revision").unwrap(),
        project: ProjectRef {
            namespace: ProjectNamespace::new("local:test").unwrap(),
            project_id: ProjectId::new("project").unwrap(),
        },
        source_set_digest: Digest::new("sha256", "11").unwrap(),
        policy_digest: Digest::new("sha256", "22").unwrap(),
        memory_model_version: 1,
        extractor_set_digest: Digest::new("sha256", "33").unwrap(),
        completed_by: MemoryBuildId::new("build").unwrap(),
    };
    let repository = RepositoryRef {
        namespace: RepositoryNamespace::new("local:test").unwrap(),
        repository_id: RepositoryId::new("repository").unwrap(),
    };
    let node = GraphNode {
        snapshot_id: snapshot_id.clone(),
        id: NodeId::new("node").unwrap(),
        kind: "symbol".to_string(),
        semantic_key: None,
        provenance: FactProvenance {
            extractor: ExtractorIdentity {
                id: ExtractorId::new("test.graph").unwrap(),
                version: "1".to_string(),
                contract_version: 1,
            },
            evidence: None,
            resolution: ResolutionState::Resolved,
            confidence: Confidence::Exact,
        },
        properties: BTreeMap::new(),
    };
    let checks = Cell::new(0usize);

    let result = resolve_repository_links_for_snapshot(
        &revision,
        &[],
        RepositoryLinkSnapshotSet {
            repository: &repository,
            current: RepositoryLinkSnapshot {
                snapshot_id: &snapshot_id,
                nodes: &[node],
            },
            origins: &[],
        },
        Utc::now(),
        || {
            checks.set(checks.get() + 1);
            checks.get() > 1
        },
    );

    assert_eq!(result, Err(SnapshotLinkResolutionError::DeadlineExceeded));
}

#[test]
fn explicit_reference_scan_checks_the_deadline_between_candidates() {
    let checks = Cell::new(0usize);
    let result = explicit_references_until(
        "`path:src/lib.rs` and `symbol:rust:function:src/lib.rs:run`",
        &mut || {
            checks.set(checks.get() + 1);
            checks.get() > 2
        },
    );

    assert_eq!(result, Err(SnapshotLinkResolutionError::DeadlineExceeded));
}
