//! Tests for snapshots.

use super::*;

#[test]
fn search_classifies_exact_matches_and_is_snapshot_deterministic() {
    let (_source, _sidecar_dir, sidecar, config, comparison) = indexed_fixture();
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), Some(comparison));

    for (text, expected) in [
        ("RuntimeTaskContext", SearchMatchKind::ExactNormalizedName),
        ("src/lib.rs", SearchMatchKind::ExactPath),
    ] {
        let request = SearchRequest {
            scope: scope(&config),
            text: text.to_string(),
            node_kinds: vec![],
            paths: vec![],
            page: super::super::super::query::PageRequest { cursor: None },
        };
        let first = query.search(&request).unwrap();
        let second = query.search(&request).unwrap();

        assert_eq!(first.data.hits.first().unwrap().match_kind, expected);
        assert_eq!(
            serde_json::to_value(first).unwrap(),
            serde_json::to_value(second).unwrap()
        );
    }

    let symbol = query
        .search(&SearchRequest {
            scope: scope(&config),
            text: "RuntimeTaskContext".to_string(),
            node_kinds: vec!["struct".to_string()],
            paths: vec![],
            page: super::super::super::query::PageRequest { cursor: None },
        })
        .unwrap()
        .data
        .hits
        .into_iter()
        .find_map(|hit| hit.semantic_key)
        .unwrap();
    let exact_symbol = query
        .search(&SearchRequest {
            scope: scope(&config),
            text: symbol.as_str().to_string(),
            node_kinds: vec![],
            paths: vec![],
            page: super::super::super::query::PageRequest { cursor: None },
        })
        .unwrap();
    assert_eq!(
        exact_symbol.data.hits.first().unwrap().match_kind,
        SearchMatchKind::ExactSemanticKey
    );
}

#[test]
fn analysis_and_extractor_changes_mark_the_snapshot_stale() {
    let (_source, _sidecar_dir, sidecar, config, current) = indexed_fixture();
    let snapshot = sidecar
        .published_snapshot(&repository(), &PublishedViewName::new("canonical").unwrap())
        .unwrap()
        .unwrap();
    for (comparison, reason) in [
        (
            FreshnessComparison {
                analysis_config_digest: Digest::new("sha256", "aa").unwrap(),
                ..current.clone()
            },
            "analysis_config_changed",
        ),
        (
            FreshnessComparison {
                extractor_set_digest: Digest::new("sha256", "bb").unwrap(),
                ..current.clone()
            },
            "extractor_set_changed",
        ),
    ] {
        let status = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), Some(comparison))
            .status(&StatusRequest {
                scope: scope(&config),
            })
            .unwrap();

        assert_eq!(status.freshness.freshness, Freshness::Stale);
        assert_eq!(
            status.freshness.compared_manifest.as_ref(),
            Some(&snapshot.source_manifest_digest)
        );
        assert_eq!(status.freshness.reason_codes, vec![reason]);
    }
}
