//! Tests for files.

use super::*;

#[test]
fn path_filters_treat_like_metacharacters_as_literals() {
    let (_source, _sidecar_dir, sidecar, config, _comparison) =
        indexed_fixture_with_extra_files(&[
            ("src_a/lib.rs", "pub struct PathScopedMarker;\n"),
            ("srcXa/lib.rs", "pub struct PathScopedMarker;\n"),
            ("src%a/lib.rs", "pub struct PathScopedMarker;\n"),
        ]);
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);

    for prefix in ["src_a", "src%a"] {
        let response = query
            .search(&SearchRequest {
                scope: scope(&config),
                text: "PathScopedMarker".to_string(),
                node_kinds: vec!["struct".to_string()],
                paths: vec![RepoPath::new(prefix).unwrap()],
                page: super::super::super::query::PageRequest { cursor: None },
            })
            .unwrap();
        let expected_path = format!("{prefix}/lib.rs");

        assert!(!response.data.hits.is_empty());
        assert!(response.data.hits.iter().all(|hit| {
            hit.path
                .as_ref()
                .is_some_and(|path| path.as_str() == expected_path)
        }));
    }
}
