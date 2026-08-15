use super::*;
use crate::repository_graph::{
    domain::{
        BuildId, Digest, RepoPath, RepositoryId, RepositoryNamespace, RepositoryRef, SnapshotId,
    },
    ports::{ExtractionContext, SourceFileMode},
};

fn fixture(path: &str, content: &[u8]) -> (ExtractionContext, SourceFileDescriptor) {
    (
        ExtractionContext {
            snapshot_id: SnapshotId::new("snapshot-cargo-test").unwrap(),
            build_id: BuildId::new("build-cargo-test").unwrap(),
            repository: RepositoryRef {
                namespace: RepositoryNamespace::new("local").unwrap(),
                repository_id: RepositoryId::new("repo").unwrap(),
            },
            max_facts_per_file: 1_000,
            max_parser_duration_ms: 2_000,
            max_diagnostics: 100,
        },
        SourceFileDescriptor {
            path: RepoPath::new(path).unwrap(),
            content_identity: Digest::new("sha256", "00").unwrap(),
            byte_len: content.len() as u64,
            file_mode: SourceFileMode::Regular,
        },
    )
}

fn extract(path: &str, content: &[u8]) -> GraphFragment {
    let (context, file) = fixture(path, content);
    CargoExtractor::new()
        .extract(FileExtractionInput {
            context: &context,
            file: &file,
            content,
        })
        .unwrap()
}

fn nodes<'a>(fragment: &'a GraphFragment, kind: &'a str) -> Vec<&'a GraphNode> {
    fragment
        .nodes
        .iter()
        .filter(|node| node.kind == kind)
        .collect()
}

#[test]
fn supports_only_cargo_manifests() {
    let (_, root) = fixture("Cargo.toml", b"");
    let (_, nested) = fixture("crates/core/Cargo.toml", b"");
    let (_, lowercase) = fixture("cargo.toml", b"");
    let (_, other) = fixture("Cargo.lock", b"");
    let extractor = CargoExtractor::new();
    assert!(extractor.supports(&root));
    assert!(extractor.supports(&nested));
    assert!(!extractor.supports(&lowercase));
    assert!(!extractor.supports(&other));
}

#[test]
fn extracts_workspace_package_targets_and_entry_points() {
    let source = br#"
[workspace]
members = ["crates/b", "crates/a"]
resolver = "3"

[package]
name = "app-name"
version = "1.2.3"
edition = "2024"
autobins = false

[lib]
crate-type = ["rlib"]

[[bin]]
name = "server"
path = "cmd/server.rs"

[[example]]
name = "demo"
"#;
    let fragment = extract("Cargo.toml", source);
    assert_eq!(nodes(&fragment, "cargo_workspace").len(), 1);
    assert_eq!(nodes(&fragment, "cargo_package").len(), 1);
    assert_eq!(nodes(&fragment, "cargo_target").len(), 4);
    assert_eq!(nodes(&fragment, "entry_point").len(), 4);
    assert_eq!(
        nodes(&fragment, "cargo_workspace")[0]
            .properties
            .get("member_patterns"),
        Some(&GraphValue::StringList(vec![
            "crates/a".to_string(),
            "crates/b".to_string(),
        ]))
    );
    assert!(fragment.edges.iter().any(|edge| edge.kind == "contains"));
    assert_eq!(
        fragment
            .edges
            .iter()
            .filter(|edge| edge.kind == "declares_target")
            .count(),
        4
    );
    let server = nodes(&fragment, "entry_point")
        .into_iter()
        .find(|node| {
            node.properties.get("path") == Some(&GraphValue::String("cmd/server.rs".to_string()))
        })
        .unwrap();
    assert_eq!(server.provenance.resolution, ResolutionState::Unresolved);
    assert!(server.provenance.evidence.as_ref().unwrap().span.is_some());
    assert!(fragment.diagnostics.is_empty());
}

#[test]
fn emits_honest_conventional_candidates() {
    let fragment = extract(
        "crates/app/Cargo.toml",
        br#"[package]
name = "app"
version = "0.1.0"
"#,
    );
    let targets = nodes(&fragment, "cargo_target");
    assert_eq!(targets.len(), 3);
    assert!(targets.iter().all(|target| {
        target.properties.get("origin")
            == Some(&GraphValue::String("conventional_candidate".to_string()))
            && target.provenance.resolution == ResolutionState::Unresolved
    }));
    let paths = nodes(&fragment, "entry_point")
        .into_iter()
        .filter_map(|node| match node.properties.get("path") {
            Some(GraphValue::String(path)) => Some(path.as_str()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        paths,
        BTreeSet::from([
            "crates/app/build.rs",
            "crates/app/src/lib.rs",
            "crates/app/src/main.rs",
        ])
    );
}

#[test]
fn package_auto_target_switches_are_respected() {
    let fragment = extract(
        "Cargo.toml",
        br#"[package]
name = "app"
autolib = false
autobins = false
build = false
"#,
    );

    assert_eq!(nodes(&fragment, "cargo_package").len(), 1);
    assert!(nodes(&fragment, "cargo_target").is_empty());
    assert!(nodes(&fragment, "entry_point").is_empty());
}

#[test]
fn custom_build_entry_points_are_described_but_never_executed() {
    let fragment = extract(
        "crates/app/Cargo.toml",
        br#"[package]
name = "app"
autolib = false
autobins = false
build = "tools/build.rs"
"#,
    );

    let targets = nodes(&fragment, "cargo_target");
    assert_eq!(targets.len(), 1);
    assert_eq!(
        targets[0].properties.get("target_kind"),
        Some(&GraphValue::String("custom_build".to_string()))
    );
    assert!(nodes(&fragment, "entry_point").iter().any(|node| {
        node.properties.get("path")
            == Some(&GraphValue::String("crates/app/tools/build.rs".to_string()))
    }));
}

#[test]
fn explicit_bins_infer_cargo_conventional_paths_by_name() {
    let fragment = extract(
        "Cargo.toml",
        br#"[package]
name = "app"
autobins = false

[[bin]]
name = "app"

[[bin]]
name = "worker"
"#,
    );
    let paths = nodes(&fragment, "entry_point")
        .into_iter()
        .filter_map(|node| match node.properties.get("path") {
            Some(GraphValue::String(path)) => Some(path.as_str()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    assert!(paths.contains("src/main.rs"));
    assert!(paths.contains("src/bin/worker.rs"));
}

#[test]
fn classifies_dependency_sources_without_resolving_internal_candidates() {
    let fragment = extract(
        "crates/app/Cargo.toml",
        br#"[package]
name = "app"

[dependencies]
serde = "1"
renamed = { package = "actual", version = "2", registry = "private" }
core = { path = "../core", features = ["z", "a", "a"] }
shared = { workspace = true }
mystery = {}

[target.'cfg(unix)'.dev-dependencies]
tempfile = "3"

[build-dependencies]
build-helper = { git = "https://user:secret@example.invalid/repo" }
"#,
    );
    let dependencies = nodes(&fragment, "declared_dependency");
    assert_eq!(dependencies.len(), 7);
    let classification = dependencies
        .iter()
        .map(|node| {
            let GraphValue::String(alias) = &node.properties["alias"] else {
                panic!("alias must be a string")
            };
            let GraphValue::String(classification) = &node.properties["classification"] else {
                panic!("classification must be a string")
            };
            (alias.as_str(), classification.as_str())
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(classification["serde"], "external");
    assert_eq!(classification["renamed"], "external");
    assert_eq!(classification["core"], "internal_candidate");
    assert_eq!(classification["shared"], "workspace_unresolved");
    assert_eq!(classification["mystery"], "unresolved");
    let relations = fragment
        .edges
        .iter()
        .filter(|edge| edge.kind == "depends_on")
        .collect::<Vec<_>>();
    assert!(relations.iter().any(|edge| {
            matches!(&edge.target, EdgeTarget::Unresolved(target) if target.contains("crates/core/Cargo.toml"))
        }));
    assert!(relations.iter().any(|edge| {
        edge.provenance.resolution == ResolutionState::External
            && matches!(edge.target, EdgeTarget::External(_))
    }));
    assert!(!format!("{fragment:?}").contains("user:secret"));
    assert!(dependencies.iter().any(|node| {
        node.properties.get("scope") == Some(&GraphValue::String("dev".to_string()))
            && node.properties.get("target_condition")
                == Some(&GraphValue::String("cfg(unix)".to_string()))
    }));
}

#[test]
fn virtual_workspace_dependencies_are_emitted() {
    let fragment = extract(
        "Cargo.toml",
        br#"[workspace]
members = ["crates/*"]

[workspace.dependencies]
serde = "1"
local = { path = "crates/local" }
"#,
    );
    assert!(nodes(&fragment, "cargo_package").is_empty());
    assert_eq!(nodes(&fragment, "declared_dependency").len(), 2);
    assert!(nodes(&fragment, "declared_dependency").iter().all(|node| {
        node.properties.get("scope") == Some(&GraphValue::String("workspace".to_string()))
    }));
}

#[test]
fn root_relative_directory_candidates_preserve_the_repository_root() {
    let fragment = extract(
        "Cargo.toml",
        br#"[workspace]
members = ["."]

[package]
name = "root"

[dependencies]
self-path = { path = "." }
"#,
    );
    assert_eq!(
        nodes(&fragment, "cargo_workspace")[0]
            .properties
            .get("member_patterns"),
        Some(&GraphValue::StringList(vec![".".to_string()]))
    );
    assert!(fragment.edges.iter().any(|edge| {
            matches!(&edge.target, EdgeTarget::Unresolved(target) if target == "cargo-package-path:Cargo.toml")
        }));
    assert!(fragment.diagnostics.iter().all(|diagnostic| {
        diagnostic.code.as_str() != "cargo.dependency_path_outside_repository"
            && diagnostic.code.as_str() != "cargo.workspace_path_outside_repository"
    }));
}

#[test]
fn malformed_and_semantically_invalid_manifests_are_diagnostics() {
    let malformed = extract("Cargo.toml", b"[package\nname = 'oops'");
    assert!(malformed.nodes.is_empty());
    assert_eq!(
        malformed.diagnostics[0].code.as_str(),
        "cargo.malformed_manifest"
    );
    assert!(
        malformed.diagnostics[0]
            .location
            .as_ref()
            .unwrap()
            .span
            .is_some()
    );

    let invalid = extract(
        "Cargo.toml",
        br#"package = "not-a-table"
dependencies = ["not-a-table"]
"#,
    );
    assert!(invalid.nodes.is_empty());
    assert!(
        invalid
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code.as_str() == "cargo.invalid_package_table")
    );
}

#[test]
fn outside_paths_are_not_persisted_as_repository_candidates() {
    let fragment = extract(
        "Cargo.toml",
        br#"[package]
name = "app"

[[bin]]
name = "outside"
path = "../secret/main.rs"

[dependencies]
outside = { path = "../secret" }
"#,
    );
    let debug = format!("{fragment:?}");
    assert!(!debug.contains("secret/main.rs"));
    assert!(
        fragment.diagnostics.iter().any(|diagnostic| {
            diagnostic.code.as_str() == "cargo.target_path_outside_repository"
        })
    );
    assert!(fragment.diagnostics.iter().any(|diagnostic| {
        diagnostic.code.as_str() == "cargo.dependency_path_outside_repository"
    }));
}

#[test]
fn fact_and_diagnostic_budgets_are_hard_bounded() {
    let content = br#"[package]
name = "app"

[dependencies]
a = []
b = []
c = []
"#;
    let (mut context, file) = fixture("Cargo.toml", content);
    context.max_facts_per_file = 2;
    context.max_diagnostics = 1;
    let fragment = CargoExtractor::new()
        .extract(FileExtractionInput {
            context: &context,
            file: &file,
            content,
        })
        .unwrap();
    assert!(fragment.nodes.len() + fragment.edges.len() <= 2);
    assert_eq!(fragment.diagnostics.len(), 1);
    assert_eq!(
        fragment.diagnostics[0].code.as_str(),
        "cargo.diagnostics_truncated"
    );
}

#[test]
fn duplicate_target_declarations_do_not_duplicate_ids_or_consume_facts() {
    let content = br#"[package]
name = "app"
autobins = false
build = false

[lib]
path = "src/lib.rs"

[[bin]]
name = "tool"
path = "src/tool.rs"

[[bin]]
name = "tool"
path = "src/tool.rs"
"#;
    let (mut context, file) = fixture("Cargo.toml", content);
    context.max_facts_per_file = 9;
    let fragment = CargoExtractor::new()
        .extract(FileExtractionInput {
            context: &context,
            file: &file,
            content,
        })
        .unwrap();
    assert_eq!(fragment.nodes.len() + fragment.edges.len(), 9);
    assert_eq!(
        fragment
            .edges
            .iter()
            .map(|edge| edge.id.clone())
            .collect::<BTreeSet<_>>()
            .len(),
        fragment.edges.len()
    );
    assert!(
        fragment
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.code.as_str() != "cargo.fact_limit")
    );
}

#[test]
fn dependency_field_types_are_validated() {
    let fragment = extract(
        "Cargo.toml",
        br#"[package]
name = "app"

[dependencies]
bad-package = { version = "1", package = 7 }
bad-registry = { version = "1", registry = false }
bad-optional = { version = "1", optional = "yes" }
bad-default = { version = "1", default-features = "yes" }
bad-features = { version = "1", features = ["ok", 7] }
bad-registry-source = { registry = "private" }
bad-workspace-version = { workspace = true, version = "1" }
bad-mixed-sources = { path = "local", registry = "private", version = "1" }
"#,
    );
    assert_eq!(nodes(&fragment, "declared_dependency").len(), 8);
    assert_eq!(
        fragment
            .diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.code.as_str() == "cargo.invalid_dependency_declaration"
            })
            .count(),
        8
    );
    assert!(
        fragment
            .edges
            .iter()
            .filter(|edge| edge.kind == "depends_on")
            .all(|edge| edge.provenance.resolution == ResolutionState::Unresolved)
    );
}

#[test]
fn source_derived_values_are_bounded_before_persistence() {
    let huge_version = "x".repeat(MAX_PROPERTY_STRING_BYTES + 1);
    let content = format!("[package]\nname = 'app'\nversion = '{huge_version}'\n");
    let fragment = extract("Cargo.toml", content.as_bytes());
    let package = nodes(&fragment, "cargo_package")[0];
    assert!(!package.properties.contains_key("version"));
    assert!(
        fragment
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.code.as_str() == "cargo.source_value_limit" })
    );
    assert!(!format!("{fragment:?}").contains(&huge_version));
}

#[test]
fn fact_traversal_observes_the_parser_deadline() {
    let content = b"[package]\nname = 'app'\n";
    let (context, file) = fixture("Cargo.toml", content);
    let input = FileExtractionInput {
        context: &context,
        file: &file,
        content,
    };
    let mut diagnostics = DiagnosticBuffer::new(input);
    let mut facts = FactBuffer::new(
        input,
        &mut diagnostics,
        Instant::now(),
        Duration::from_secs(1),
    );
    assert!(
        facts
            .node(
                "cargo_package",
                "cargo:package:test",
                None,
                ResolutionState::Resolved,
                Confidence::Exact,
                BTreeMap::new(),
            )
            .is_some()
    );
    facts.started = Instant::now() - Duration::from_millis(2);
    facts.budget = Duration::from_millis(1);
    assert!(!facts.active());
    facts.finish();
    assert!(facts.nodes.is_empty());
    assert!(facts.edges.is_empty());
    drop(facts);
    let diagnostics = diagnostics.finish();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code.as_str(), "cargo.parser_timeout");
}

#[test]
fn parser_deadline_kills_and_reaps_a_blocked_worker_process() {
    const CHILD_TEST: &str =
        "repository_graph::extractors::cargo::tests::parser_deadline_blocked_child";
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", CHILD_TEST, "--nocapture"])
        .env("FERRUS_CARGO_PARSER_BLOCK_TEST", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();

    let result = wait_for_child(&mut child, started, Duration::from_millis(50));

    assert!(matches!(result, ChildDeadline::TimedOut));
    assert!(child.try_wait().unwrap().is_some());
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn parser_deadline_blocked_child() {
    if std::env::var_os("FERRUS_CARGO_PARSER_BLOCK_TEST").is_some() {
        thread::sleep(Duration::from_secs(60));
    }
}

#[test]
fn parser_worker_protocol_preserves_manifest_values_and_error_spans() {
    let ParserOutput::Parsed { manifest } = parse_manifest("date = 2026-07-17\ninteger = 7\n")
    else {
        panic!("valid manifest was rejected");
    };
    let wire = serde_json::to_vec(&ParserOutput::Parsed { manifest }).unwrap();
    let ParserOutput::Parsed { manifest } = serde_json::from_slice(&wire).unwrap() else {
        panic!("valid parser response changed variants");
    };
    assert!(manifest["date"].is_datetime());
    assert_eq!(manifest["integer"].as_integer(), Some(7));

    let ParserOutput::Malformed { span } = parse_manifest("[package\n") else {
        panic!("invalid manifest was accepted");
    };
    assert!(span.is_some());
}

#[test]
fn output_is_deterministic_and_spans_are_half_open_one_based() {
    let content = b"\n  [package]\nname = 'app'\n";
    let first = extract("Cargo.toml", content);
    let second = extract("Cargo.toml", content);
    assert_eq!(
        serde_json::to_value(&first.nodes).unwrap(),
        serde_json::to_value(&second.nodes).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&first.edges).unwrap(),
        serde_json::to_value(&second.edges).unwrap()
    );
    let span = first
        .nodes
        .iter()
        .find_map(|node| node.provenance.evidence.as_ref()?.span.as_ref())
        .expect("the package declaration has source evidence");
    assert_eq!(span.start.line, Some(2));
    assert_eq!(span.start.column, Some(3));
    assert!(span.end.byte_offset > span.start.byte_offset);
}
