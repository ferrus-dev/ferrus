//! Rust extractor tests for symbols, imports, relationships, and bounded parsing.

use std::collections::BTreeSet;

use super::*;
use crate::repository_graph::{
    domain::{BuildId, Digest, RepositoryId, RepositoryNamespace, RepositoryRef, SnapshotId},
    ports::{ExtractionContext, SourceFileMode},
};

fn fixture(path: &str, content: &[u8]) -> (ExtractionContext, SourceFileDescriptor) {
    (
        ExtractionContext {
            snapshot_id: SnapshotId::new("snapshot-rust-test").unwrap(),
            build_id: BuildId::new("build-rust-test").unwrap(),
            repository: RepositoryRef {
                namespace: RepositoryNamespace::new("local").unwrap(),
                repository_id: RepositoryId::new("repo").unwrap(),
            },
            max_facts_per_file: 1_000,
            max_parser_duration_ms: 2_000,
            max_diagnostics: 100,
        },
        SourceFileDescriptor {
            path: crate::repository_graph::domain::RepoPath::new(path).unwrap(),
            content_identity: Digest::new("sha256", "00").unwrap(),
            byte_len: content.len() as u64,
            file_mode: SourceFileMode::Regular,
        },
    )
}

fn extract(path: &str, content: &[u8]) -> GraphFragment {
    let (context, file) = fixture(path, content);
    RustSyntaxExtractor
        .extract(FileExtractionInput {
            context: &context,
            file: &file,
            content,
        })
        .unwrap()
}

#[test]
fn supports_only_canonical_rust_extensions() {
    let (_, rust) = fixture("src/lib.rs", b"");
    let (_, uppercase) = fixture("src/lib.RS", b"");
    let (_, other) = fixture("src/lib.toml", b"");
    assert!(RustSyntaxExtractor.supports(&rust));
    assert!(!RustSyntaxExtractor.supports(&uppercase));
    assert!(!RustSyntaxExtractor.supports(&other));
}

#[test]
fn derives_useful_file_module_names() {
    assert_eq!(file_module_name("src/lib.rs"), "crate");
    assert_eq!(file_module_name("src/main.rs"), "crate");
    assert_eq!(file_module_name("src/http/mod.rs"), "http");
    assert_eq!(file_module_name("src/server.rs"), "server");
}

#[test]
fn extracts_required_declarations_and_conservative_relationships() {
    let source = br#"
pub mod api {
    pub struct Request;
    enum State { Ready }
    pub trait Run { fn run(&self); }
    impl Run for Request { fn run(&self) {} }
    pub(crate) async fn serve() -> bool { true }
    const LIMIT: usize = 3;
    type ResultAlias = Result<(), ()>;
    use crate::private::Thing;
    pub use crate::shared::{One, Two};
    mod external;
}
"#;
    let fragment = extract("src/lib.rs", source);
    let kinds = fragment
        .nodes
        .iter()
        .map(|node| node.kind.as_str())
        .collect::<BTreeSet<_>>();
    for expected in [
        "module",
        "struct",
        "enum",
        "trait",
        "impl",
        "function",
        "constant",
        "type_alias",
        "import",
        "re_export",
        "mod_declaration",
    ] {
        assert!(kinds.contains(expected), "missing {expected}: {kinds:?}");
    }
    assert!(fragment.edges.iter().any(|edge| edge.kind == "imports"));
    assert!(fragment.edges.iter().any(|edge| edge.kind == "re_exports"));
    assert!(
        fragment
            .edges
            .iter()
            .any(|edge| edge.kind == "declares_module")
    );
    assert!(fragment.edges.iter().all(|edge| edge.kind != "calls"));
    assert!(fragment.edges.iter().all(|edge| edge.kind != "implements"));
    let implementation = fragment
        .nodes
        .iter()
        .find(|node| node.kind == "impl")
        .unwrap();
    assert_eq!(
        implementation.properties.get("implementation_target"),
        Some(&GraphValue::String("Run for Request".to_string()))
    );
    let import = fragment
        .edges
        .iter()
        .find(|edge| edge.kind == "imports")
        .unwrap();
    assert_eq!(
        import.target,
        EdgeTarget::Unresolved("crate::private::Thing".to_string())
    );
    assert!(fragment.diagnostics.is_empty());
}

#[test]
fn block_local_import_relationships_use_the_nearest_module() {
    let fragment = extract(
        "src/lib.rs",
        br#"
mod api { pub struct Api; }
fn root_scope() { use crate::api::Api; }
mod nested {
    fn nested_scope() { use crate::api::Api; }
}
"#,
    );
    let imports = fragment
        .edges
        .iter()
        .filter(|edge| edge.kind == "imports")
        .collect::<Vec<_>>();
    assert_eq!(imports.len(), 2);
    assert!(imports.iter().all(|edge| {
        fragment
            .nodes
            .iter()
            .any(|node| node.id == edge.source && node.kind == "module")
    }));

    let import_ids = fragment
        .nodes
        .iter()
        .filter(|node| node.kind == "import")
        .map(|node| &node.id)
        .collect::<BTreeSet<_>>();
    let function_ids = fragment
        .nodes
        .iter()
        .filter(|node| node.kind == "function")
        .map(|node| &node.id)
        .collect::<BTreeSet<_>>();
    assert!(import_ids.iter().all(|import| {
        fragment.edges.iter().any(|edge| {
            edge.kind == "contains"
                && function_ids.contains(&edge.source)
                && edge.target == EdgeTarget::Node((*import).clone())
        })
    }));
}

#[test]
fn records_signature_visibility_containment_and_one_based_spans() {
    let source = b"// heading\npub(crate) async fn serve(value: usize) -> bool { value > 0 }\n";
    let fragment = extract("src/server.rs", source);
    let function = fragment
        .nodes
        .iter()
        .find(|node| node.kind == "function")
        .unwrap();
    assert_eq!(
        function.properties.get("visibility"),
        Some(&GraphValue::String("pub(crate)".to_string()))
    );
    assert_eq!(
        function.properties.get("signature"),
        Some(&GraphValue::String(
            "pub(crate) async fn serve(value: usize) -> bool".to_string()
        ))
    );
    let evidence = function.provenance.evidence.as_ref().unwrap();
    let function_span = evidence.span.as_ref().unwrap();
    assert_eq!(function_span.start.byte_offset, 11);
    assert_eq!(function_span.start.line, Some(2));
    assert_eq!(function_span.start.column, Some(1));
    assert!(fragment.edges.iter().any(|edge| {
        edge.kind == "contains"
            && matches!(&edge.target, EdgeTarget::Node(id) if id == &function.id)
    }));
}

#[test]
fn incomplete_source_preserves_recoverable_facts_and_diagnostics() {
    let fragment = extract(
        "src/lib.rs",
        b"pub struct Kept;\nfn unfinished(value: {\nconst ALSO: usize = 1;\n",
    );
    assert!(fragment.nodes.iter().any(|node| {
        node.kind == "struct"
            && node.properties.get("name") == Some(&GraphValue::String("Kept".to_string()))
    }));
    assert!(fragment.diagnostics.iter().any(|diagnostic| {
        matches!(
            diagnostic.code.as_str(),
            "rust.parse_error" | "rust.missing_syntax"
        )
    }));
}

#[test]
fn extraction_is_deterministic() {
    let source = b"pub struct Item;\nuse crate::other::Item as Other;\n";
    let first = extract("src/lib.rs", source);
    let second = extract("src/lib.rs", source);
    assert_eq!(first.nodes, second.nodes);
    assert_eq!(first.edges, second.edges);
    assert_eq!(first.diagnostics, second.diagnostics);
}

#[test]
fn sibling_declarations_do_not_inherit_each_others_lexical_scopes() {
    let mut source = String::from("mod outer {\n");
    for index in 0..100 {
        source.push_str(&format!(
            "fn function_{index}() {{ {{ struct Local; }} }}\n"
        ));
    }
    source.push_str("}\nstruct Root;\n");
    let graph = extract("src/lib.rs", source.as_bytes());
    let keys: BTreeSet<_> = graph
        .nodes
        .iter()
        .filter_map(|node| node.semantic_key.as_ref())
        .map(|key| key.as_str())
        .collect();
    assert!(keys.contains("rust:struct:src/lib.rs:crate::Root"));
    for index in 0..100 {
        assert!(
            keys.contains(
                format!("rust:function:src/lib.rs:crate::outer::function_{index}").as_str()
            )
        );
    }
    assert_eq!(
        graph
            .nodes
            .iter()
            .filter(|node| node.kind.as_str() == "struct")
            .count(),
        101
    );
}

#[test]
fn semantic_keys_disambiguate_impls_and_block_local_items() {
    let source = br#"
struct Item;
impl Item {}
impl Item {}
fn outer() {
    { fn helper() {} }
    { fn helper() {} }
}
"#;
    let fragment = extract("src/lib.rs", source);
    let keys = fragment
        .nodes
        .iter()
        .filter_map(|node| node.semantic_key.as_ref())
        .map(|key| key.as_str())
        .collect::<Vec<_>>();
    assert_eq!(keys.len(), keys.iter().collect::<BTreeSet<_>>().len());
}

#[test]
fn fact_limit_never_emits_partial_declarations() {
    let source = b"struct One; struct Two; struct Three;";
    let (mut context, file) = fixture("src/lib.rs", source);
    context.max_facts_per_file = 3;
    let fragment = RustSyntaxExtractor
        .extract(FileExtractionInput {
            context: &context,
            file: &file,
            content: source,
        })
        .unwrap();
    assert_eq!(fragment.nodes.len() + fragment.edges.len(), 3);
    assert_eq!(
        fragment
            .edges
            .iter()
            .filter(|edge| edge.kind == "contains")
            .count(),
        1
    );
    assert!(
        fragment
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code.as_str() == "rust.fact_limit")
    );
}

#[test]
fn diagnostics_are_bounded_and_report_suppression() {
    let source = b"struct One; ? struct Two;";
    let (mut context, file) = fixture("src/lib.rs", source);
    context.max_diagnostics = 1;
    context.max_facts_per_file = 3;
    let fragment = RustSyntaxExtractor
        .extract(FileExtractionInput {
            context: &context,
            file: &file,
            content: source,
        })
        .unwrap();
    assert_eq!(fragment.diagnostics.len(), 1);
    let diagnostic = &fragment.diagnostics[0];
    assert_eq!(diagnostic.code.as_str(), "rust.diagnostics_truncated");
    assert!(diagnostic.metrics.values().all(|count| *count > 0));
}

#[test]
fn oversized_source_values_are_not_persisted() {
    let target = "a".repeat(MAX_SOURCE_VALUE_BYTES + 1);
    let source = format!("use crate::{target};");
    let fragment = extract("src/lib.rs", source.as_bytes());

    assert!(
        fragment
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.code.as_str() == "rust.source_value_limit" })
    );
    assert!(fragment.edges.iter().all(|edge| match &edge.target {
        EdgeTarget::Node(_) => true,
        EdgeTarget::External(target) | EdgeTarget::Unresolved(target) => {
            target.len() <= MAX_SOURCE_VALUE_BYTES
        }
    }));
}

#[test]
fn invalid_utf8_and_zero_time_budget_are_safe_failures() {
    let invalid = extract("src/lib.rs", b"fn ok() {}\xff");
    assert!(invalid.nodes.is_empty());
    assert_eq!(invalid.diagnostics[0].code.as_str(), "rust.invalid_utf8");

    let source = b"fn never_parsed() {}";
    let (mut context, file) = fixture("src/lib.rs", source);
    context.max_parser_duration_ms = 0;
    let timed_out = RustSyntaxExtractor
        .extract(FileExtractionInput {
            context: &context,
            file: &file,
            content: source,
        })
        .unwrap();
    assert!(timed_out.nodes.is_empty());
    assert_eq!(
        timed_out.diagnostics[0].code.as_str(),
        "rust.parser_timeout"
    );
}

#[test]
fn constant_signatures_do_not_persist_initializer_values() {
    let fragment = extract(
        "src/lib.rs",
        br#"
const TOKEN: &str = "do-not-copy-in-signature";
type Handler = fn(Request) -> Response;
"#,
    );
    let constant = fragment
        .nodes
        .iter()
        .find(|node| node.kind == "constant")
        .unwrap();
    assert_eq!(
        constant.properties.get("signature"),
        Some(&GraphValue::String("const TOKEN: &str".to_string()))
    );
    let alias = fragment
        .nodes
        .iter()
        .find(|node| node.kind == "type_alias")
        .unwrap();
    assert_eq!(
        alias.properties.get("signature"),
        Some(&GraphValue::String(
            "type Handler = fn(Request) -> Response".to_string()
        ))
    );
}
