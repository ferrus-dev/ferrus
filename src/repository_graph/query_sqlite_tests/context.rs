//! Tests for context.

use super::*;

#[test]
fn context_resolves_seeds_ranks_deduplicates_and_preserves_evidence() {
    let (_source, _sidecar_dir, sidecar, config, comparison) = indexed_fixture();
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), Some(comparison));
    let symbol = ContextSeed::Symbol(fixture_symbol(&query, &config));
    let request = context_request(&config, symbol.clone());

    let first = query.context(&request).unwrap();
    let second = query.context(&request).unwrap();

    assert!(!first.data.items.is_empty());
    assert_eq!(
        first.data.items[0].selection_reasons[0].kind,
        ContextSelectionKind::ExactSeed
    );
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(&second).unwrap()
    );
    let mut node_ids = BTreeSet::new();
    let mut selection_kinds = BTreeSet::new();
    for item in &first.data.items {
        assert!(node_ids.insert(item.node_id.clone()));
        let evidence = item.provenance.evidence.as_ref().unwrap();
        assert_eq!(item.path, evidence.path);
        assert_eq!(item.span, evidence.span);
        assert_eq!(item.content_identity, evidence.content_identity);
        let mut reasons = item.selection_reasons.clone();
        sort_context_reasons(&mut reasons);
        reasons.dedup();
        assert_eq!(item.selection_reasons, reasons);
        selection_kinds.extend(
            item.selection_reasons
                .iter()
                .map(|reason| context_selection_rank(reason.kind)),
        );
    }

    assert!(selection_kinds.contains(&context_selection_rank(ContextSelectionKind::Containment)));
    assert!(selection_kinds.contains(&context_selection_rank(ContextSelectionKind::Declaration)));

    let mut ordered = context_request(&config, symbol.clone());
    ordered
        .seeds
        .push(ContextSeed::Path(RepoPath::new("src/lib.rs").unwrap()));
    let mut reversed = ordered.clone();
    reversed.seeds.reverse();
    assert_eq!(
        serde_json::to_value(query.context(&ordered).unwrap()).unwrap(),
        serde_json::to_value(query.context(&reversed).unwrap()).unwrap()
    );
}

#[test]
fn context_classifies_documentation_facts_ahead_of_generic_relationships() {
    let (_source, _sidecar_dir, sidecar, config, _comparison) =
        indexed_fixture_with_extra_files(&[("README.md", "# Fixture\n")]);
    let (file, document): (String, String) = sidecar
        .connection()
        .query_row(
            "SELECT edge.source_node_id, edge.target_node_id \
             FROM edges AS edge \
             JOIN nodes AS source ON source.snapshot_id = edge.snapshot_id \
                                 AND source.id = edge.source_node_id \
             JOIN nodes AS target ON target.snapshot_id = edge.snapshot_id \
                                 AND target.id = edge.target_node_id \
             WHERE edge.kind = 'classified_as' \
               AND source.evidence_path = 'README.md' \
               AND target.kind = 'document' \
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
    let mut request = context_request(&config, ContextSeed::Node(NodeId::new(file).unwrap()));
    request.policy.direction = EdgeDirection::Outgoing;
    request.policy.edge_kinds = vec!["classified_as".to_string()];

    let response = query.context(&request).unwrap();
    let item = response
        .data
        .items
        .iter()
        .find(|item| item.node_id.as_str() == document)
        .unwrap();

    assert!(
        item.selection_reasons
            .iter()
            .any(|reason| reason.kind == ContextSelectionKind::Documentation)
    );
}

#[test]
fn context_labels_resolved_import_targets_as_dependencies() {
    let (_source, _sidecar_dir, sidecar, config, _comparison) =
        indexed_fixture_with_extra_files(&[
            (
                "src/lib.rs",
                "pub mod api;\nuse crate::api::Api;\npub fn make() -> Api { Api }\n",
            ),
            ("src/api.rs", "pub struct Api;\n"),
        ]);
    let (source, target): (String, String) = sidecar
        .connection()
        .query_row(
            "SELECT source_node_id, target_node_id FROM edges \
             WHERE kind = 'imports' AND target_node_id IS NOT NULL \
             ORDER BY id LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
    let mut request = context_request(&config, ContextSeed::Node(NodeId::new(source).unwrap()));
    request.policy.direction = EdgeDirection::Outgoing;
    request.policy.edge_kinds = vec!["imports".to_string()];

    let response = query.context(&request).unwrap();
    let target = response
        .data
        .items
        .iter()
        .find(|item| item.node_id.as_str() == target)
        .unwrap();

    assert!(
        target
            .selection_reasons
            .iter()
            .any(|reason| reason.kind == ContextSelectionKind::ResolvedDependency)
    );
    assert_eq!(target.provenance.resolution, ResolutionState::Resolved);
}

#[test]
fn search_show_and_neighborhood_return_evidence_and_provenance() {
    let (_source, _sidecar_dir, sidecar, config, freshness_comparison) = indexed_fixture();
    let query = SqliteGraphQuery::new(
        &sidecar,
        config.query_limits.clone(),
        Some(freshness_comparison.clone()),
    );
    let search = query
        .search(&SearchRequest {
            scope: scope(&config),
            text: "RuntimeTaskContext".to_string(),
            node_kinds: vec!["struct".to_string()],
            paths: vec![],
            page: super::super::super::query::PageRequest { cursor: None },
        })
        .unwrap();
    assert_eq!(search.freshness.freshness, Freshness::Fresh);
    assert_eq!(
        search.source_revision.manifest_digest,
        freshness_comparison.source_manifest_digest
    );
    assert!(!search.data.hits.is_empty());
    let hit = &search.data.hits[0];
    assert_eq!(hit.path.as_ref().unwrap().as_str(), "src/lib.rs");
    assert!(hit.span.is_some());
    assert_eq!(hit.provenance.resolution, ResolutionState::Resolved);

    let shown = query
        .show(&ShowRequest {
            scope: scope(&config),
            lookup: ShowLookup::Node(hit.node_id.clone()),
            page: super::super::super::query::PageRequest { cursor: None },
        })
        .unwrap();
    assert_eq!(shown.data.nodes.len(), 1);

    let neighborhood = query
        .neighborhood(&NeighborhoodRequest {
            scope: scope(&config),
            roots: vec![hit.node_id.clone()],
            direction: EdgeDirection::Both,
            edge_kinds: vec![],
            page: super::super::super::query::PageRequest { cursor: None },
        })
        .unwrap();
    assert!(
        neighborhood
            .data
            .nodes
            .iter()
            .any(|node| node.id == hit.node_id)
    );
}

#[test]
fn context_traverses_evidence_free_seeds_and_intermediate_nodes() {
    for strip_seed in [false, true] {
        let (_source, _sidecar_dir, sidecar, config, _comparison) = indexed_fixture();

        let (first_edge, second_edge, seed, bridge, target): (String, String, String, String, String) =
            sidecar.connection().query_row(
                "SELECT first.id, second.id, first.source_node_id, first.target_node_id, second.target_node_id \
                 FROM edges AS first JOIN edges AS second \
                   ON first.snapshot_id = second.snapshot_id \
                  AND first.target_node_id = second.source_node_id \
                 JOIN nodes AS target ON target.snapshot_id = second.snapshot_id \
                                     AND target.id = second.target_node_id \
                 WHERE first.source_node_id != first.target_node_id \
                   AND first.source_node_id != second.target_node_id \
                   AND first.target_node_id != second.target_node_id \
                   AND first.resolution_state = 'resolved' \
                   AND second.resolution_state = 'resolved' \
                   AND target.evidence_path IS NOT NULL ORDER BY first.id, second.id LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            ).unwrap();

        sidecar
            .connection()
            .execute(
                "DELETE FROM edges WHERE id NOT IN (?1, ?2)",
                rusqlite::params![first_edge, second_edge],
            )
            .unwrap();

        for node in std::iter::once(&bridge).chain(strip_seed.then_some(&seed)) {
            sidecar.connection().execute(
                "UPDATE nodes SET evidence_path=NULL, evidence_content_algorithm=NULL, \
                 evidence_content_digest=NULL, span_start_byte=NULL, span_end_byte=NULL WHERE id=?1",
                [node],
            ).unwrap();
        }

        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
        let mut request = context_request(&config, ContextSeed::Node(NodeId::new(&seed).unwrap()));
        request.policy.direction = EdgeDirection::Outgoing;
        request.scope.budget.max_depth = NonZeroU32::new(2).unwrap();
        let response = query.context(&request).unwrap();

        assert!(
            response
                .data
                .items
                .iter()
                .any(|item| item.node_id.as_str() == target)
        );
        assert!(
            response
                .data
                .items
                .iter()
                .all(|item| item.node_id.as_str() != bridge)
        );
        if strip_seed {
            assert!(
                response
                    .data
                    .items
                    .iter()
                    .all(|item| item.node_id.as_str() != seed)
            );
        }
        assert!(
            response
                .data
                .items
                .iter()
                .all(|item| item.provenance.evidence.is_some())
        );
    }
}
