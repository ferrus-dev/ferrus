//! Deterministic repository/file facts, hierarchy, and conventional-file classification.

use super::*;
use crate::repository_graph::{
    domain::{
        BuildId, Digest, RepositoryId, RepositoryNamespace, RepositoryRef, SnapshotId, SourceKind,
        SourceRevision, SourceRevisionId,
    },
    ports::{SourceDiscoveryMetrics, SourceFileDescriptor},
};

#[test]
fn repository_fragment_is_deterministic_and_contains_root() {
    let context = context(100, 10);
    let manifest = manifest(vec![file("src/main.rs", 4, SourceFileMode::Regular)]);
    let extractor = GenericExtractor::new();

    let first = extractor.repository_fragment(&context, &manifest);
    let second = extractor.repository_fragment(&context, &manifest);

    assert_eq!(first.nodes, second.nodes);
    assert_eq!(first.edges, second.edges);
    assert_eq!(first.diagnostics, second.diagnostics);
    assert_eq!(kinds(&first), vec![DIRECTORY_KIND, REPOSITORY_KIND]);
    assert_eq!(first.edges.len(), 1);
    assert_eq!(first.edges[0].kind, CONTAINS_EDGE);
    assert_eq!(
        property(&first.nodes, REPOSITORY_KIND, "file_count"),
        &GraphValue::Integer(1)
    );
    assert_eq!(
        property(&first.nodes, DIRECTORY_KIND, "path"),
        &GraphValue::String(".".to_string())
    );
}

#[test]
fn repository_fragment_rejects_a_manifest_from_another_repository() {
    let context = context(100, 10);
    let mut manifest = manifest(vec![file("src/main.rs", 4, SourceFileMode::Regular)]);
    manifest.revision.repository.repository_id = RepositoryId::new("another-repository").unwrap();

    let fragment = GenericExtractor::new().repository_fragment(&context, &manifest);

    assert!(fragment.nodes.is_empty());
    assert!(fragment.edges.is_empty());
    assert_eq!(fragment.diagnostics.len(), 1);
    assert_eq!(
        fragment.diagnostics[0].code.as_str(),
        "generic.repository_mismatch"
    );
}

#[test]
fn file_fragment_emits_deterministic_hierarchy_and_file_evidence() {
    let context = context(100, 10);
    let descriptor = file("src/bin/main.rs", 4, SourceFileMode::Executable);
    let input = input(&context, &descriptor, b"fn x");
    let extractor = GenericExtractor::new();

    let first = extractor.extract(input).unwrap();
    let second = extractor.extract(input).unwrap();

    assert_eq!(first.nodes, second.nodes);
    assert_eq!(first.edges, second.edges);
    assert_eq!(first.diagnostics, second.diagnostics);
    assert_eq!(
        first
            .nodes
            .iter()
            .filter(|node| node.kind == DIRECTORY_KIND)
            .count(),
        3
    );
    assert!(first.nodes.iter().any(|node| node.kind == FILE_KIND));
    assert!(first.nodes.iter().any(|node| node.kind == ENTRY_POINT_KIND));
    assert_eq!(
        first
            .edges
            .iter()
            .filter(|edge| edge.kind == CONTAINS_EDGE)
            .count(),
        3
    );
    assert_eq!(
        first
            .edges
            .iter()
            .filter(|edge| edge.kind == CLASSIFIED_AS_EDGE)
            .count(),
        1
    );
    assert_eq!(
        property(&first.nodes, FILE_KIND, "executable"),
        &GraphValue::Boolean(true)
    );
    let evidence = first
        .nodes
        .iter()
        .find(|node| node.kind == FILE_KIND)
        .unwrap()
        .provenance
        .evidence
        .as_ref()
        .unwrap();
    assert_eq!(evidence.path.as_str(), "src/bin/main.rs");
    assert_eq!(evidence.content_identity, descriptor.content_identity);
}

#[test]
fn repeated_ancestor_facts_are_identical_across_file_fragments() {
    let context = context(100, 10);
    let left = file("src/left.rs", 1, SourceFileMode::Regular);
    let right = file("src/right.rs", 1, SourceFileMode::Regular);
    let extractor = GenericExtractor::new();
    let left = extractor.extract(input(&context, &left, b"x")).unwrap();
    let right = extractor.extract(input(&context, &right, b"y")).unwrap();

    let left_directory = left
        .nodes
        .iter()
        .find(|node| {
            node.kind == DIRECTORY_KIND
                && node.properties.get("path") == Some(&GraphValue::String("src".to_string()))
        })
        .unwrap();
    let right_directory = right
        .nodes
        .iter()
        .find(|node| node.id == left_directory.id)
        .unwrap();
    assert_eq!(left_directory, right_directory);

    let left_containment = left
        .edges
        .iter()
        .find(|edge| edge.target == EdgeTarget::Node(left_directory.id.clone()))
        .unwrap();
    let right_containment = right
        .edges
        .iter()
        .find(|edge| edge.id == left_containment.id)
        .unwrap();
    assert_eq!(left_containment, right_containment);
}

#[test]
fn conventional_files_receive_all_applicable_classifications() {
    let context = context(100, 10);
    let extractor = GenericExtractor::new();

    let readme = file("README.md", 7, SourceFileMode::Regular);
    let readme_fragment = extractor
        .extract(input(&context, &readme, b"# Hello"))
        .unwrap();
    assert!(
        readme_fragment
            .nodes
            .iter()
            .any(|node| node.kind == DOCUMENT_KIND)
    );

    let cargo = file("Cargo.toml", 9, SourceFileMode::Regular);
    let cargo_fragment = extractor
        .extract(input(&context, &cargo, b"[package]"))
        .unwrap();
    assert!(
        cargo_fragment
            .nodes
            .iter()
            .any(|node| node.kind == MANIFEST_KIND)
    );
    assert!(
        cargo_fragment
            .nodes
            .iter()
            .any(|node| node.kind == CONFIGURATION_KIND)
    );

    let main = file("src/main.rs", 11, SourceFileMode::Regular);
    let main_fragment = extractor
        .extract(input(&context, &main, b"fn main(){}"))
        .unwrap();
    assert!(
        main_fragment
            .nodes
            .iter()
            .any(|node| node.kind == ENTRY_POINT_KIND)
    );
}

#[test]
fn evidence_spans_are_half_open_with_one_based_human_positions() {
    let context = context(100, 10);
    let descriptor = file("README.md", 4, SourceFileMode::Regular);
    let fragment = GenericExtractor::new()
        .extract(input(&context, &descriptor, "a\nβ".as_bytes()))
        .unwrap();
    let span = fragment
        .nodes
        .iter()
        .find(|node| node.kind == DOCUMENT_KIND)
        .unwrap()
        .provenance
        .evidence
        .as_ref()
        .unwrap()
        .span
        .as_ref()
        .unwrap();

    assert_eq!(span.start.byte_offset, 0);
    assert_eq!(span.start.line, Some(1));
    assert_eq!(span.start.column, Some(1));
    assert_eq!(span.end.byte_offset, 4);
    assert_eq!(span.end.line, Some(2));
    assert_eq!(span.end.column, Some(3));
}

#[test]
fn binary_and_non_utf8_content_are_skipped_with_bounded_diagnostics() {
    let context = context(100, 1);
    let descriptor = file("README.md", 3, SourceFileMode::Regular);
    let extractor = GenericExtractor::new();

    let binary = extractor
        .extract(input(&context, &descriptor, b"a\0b"))
        .unwrap();
    assert_eq!(binary.diagnostics.len(), 1);
    assert_eq!(
        binary.diagnostics[0].code.as_str(),
        "generic.binary_content_skipped"
    );
    assert!(!binary.nodes.iter().any(|node| node.kind == DOCUMENT_KIND));

    let non_utf8 = extractor
        .extract(input(&context, &descriptor, &[0xff, 0xfe]))
        .unwrap();
    assert_eq!(non_utf8.diagnostics.len(), 1);
    assert_eq!(
        non_utf8.diagnostics[0].code.as_str(),
        "generic.non_utf8_content_skipped"
    );
    assert!(!non_utf8.nodes.iter().any(|node| node.kind == DOCUMENT_KIND));
}

#[test]
fn fact_and_diagnostic_limits_are_hard_bounds() {
    let limited = context(3, 1);
    let descriptor = file("src/main.rs", 2, SourceFileMode::Regular);
    let fragment = GenericExtractor::new()
        .extract(input(&limited, &descriptor, b"fn"))
        .unwrap();

    assert!(fragment.nodes.len() + fragment.edges.len() <= 3);
    assert_eq!(fragment.diagnostics.len(), 1);
    assert_eq!(
        fragment.diagnostics[0].code.as_str(),
        "generic.fact_limit_reached"
    );

    let silent = context(0, 0);
    let fragment = GenericExtractor::new()
        .extract(input(&silent, &descriptor, b"fn"))
        .unwrap();
    assert!(fragment.nodes.is_empty());
    assert!(fragment.edges.is_empty());
    assert!(fragment.diagnostics.is_empty());
}

#[test]
fn source_text_is_never_copied_into_facts_or_diagnostics() {
    let context = context(100, 10);
    let descriptor = file("README.md", 22, SourceFileMode::Regular);
    let fragment = GenericExtractor::new()
        .extract(input(&context, &descriptor, b"token=TOP_SECRET_VALUE"))
        .unwrap();
    let encoded =
        serde_json::to_string(&(&fragment.nodes, &fragment.edges, &fragment.diagnostics)).unwrap();

    assert!(!encoded.contains("TOP_SECRET_VALUE"));
    assert!(!encoded.contains("token="));
}

#[test]
fn extractor_supports_every_regular_manifest_file() {
    let extractor = GenericExtractor::new();
    assert!(extractor.supports(&file("image.png", 3, SourceFileMode::Regular)));
    assert_eq!(extractor.identity().id.as_str(), EXTRACTOR_ID);
    assert_eq!(
        extractor.identity().contract_version,
        EXTRACTOR_CONTRACT_VERSION
    );
}

fn context(max_facts_per_file: u64, max_diagnostics: u64) -> ExtractionContext {
    ExtractionContext {
        snapshot_id: SnapshotId::new("snapshot-1").unwrap(),
        build_id: BuildId::new("build-1").unwrap(),
        repository: RepositoryRef {
            namespace: RepositoryNamespace::new("local").unwrap(),
            repository_id: RepositoryId::new("repository-1").unwrap(),
        },
        max_facts_per_file,
        max_parser_duration_ms: 1_000,
        max_diagnostics,
    }
}

fn file(path: &str, byte_len: u64, file_mode: SourceFileMode) -> SourceFileDescriptor {
    SourceFileDescriptor {
        path: RepoPath::new(path).unwrap(),
        content_identity: Digest::new("sha256", "ab").unwrap(),
        byte_len,
        file_mode,
    }
}

fn manifest(files: Vec<SourceFileDescriptor>) -> SourceManifest {
    let repository = RepositoryRef {
        namespace: RepositoryNamespace::new("local").unwrap(),
        repository_id: RepositoryId::new("repository-1").unwrap(),
    };
    SourceManifest {
        revision: SourceRevision {
            id: SourceRevisionId::new("revision-1").unwrap(),
            repository,
            source_kind: SourceKind::NonGitManifest,
            base_revision: None,
            manifest_digest: Digest::new("sha256", "aa").unwrap(),
            analysis_config_digest: Digest::new("sha256", "bb").unwrap(),
            dirty: false,
            includes_untracked: false,
        },
        extractor_set_digest: Digest::new("sha256", "cc").unwrap(),
        files,
        diagnostics: Vec::new(),
        metrics: SourceDiscoveryMetrics::default(),
    }
}

fn input<'a>(
    context: &'a ExtractionContext,
    file: &'a SourceFileDescriptor,
    content: &'a [u8],
) -> FileExtractionInput<'a> {
    FileExtractionInput {
        context,
        file,
        content,
    }
}

fn kinds(fragment: &GraphFragment) -> Vec<&str> {
    let mut kinds: Vec<_> = fragment
        .nodes
        .iter()
        .map(|node| node.kind.as_str())
        .collect();
    kinds.sort_unstable();
    kinds
}

fn property<'a>(nodes: &'a [GraphNode], kind: &str, name: &str) -> &'a GraphValue {
    nodes
        .iter()
        .find(|node| node.kind == kind)
        .and_then(|node| node.properties.get(name))
        .unwrap()
}
