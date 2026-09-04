//! Cross-file resolution tests for Cargo and Rust evidence, ambiguity, and bounded work.

use sha2::{Digest as _, Sha256};

use super::*;
use crate::repository_graph::{
    domain::{
        BuildId, Digest, RepositoryId, RepositoryNamespace, RepositoryRef, SnapshotId, SourceKind,
        SourceRevision, SourceRevisionId,
    },
    extractors::{cargo::CargoExtractor, rust::RustSyntaxExtractor},
    ports::{
        ExtractionContext, Extractor, FileExtractionInput, ResolutionBudget,
        SourceDiscoveryMetrics, SourceFileMode, SourceManifest,
    },
};

struct Fixture {
    context: ExtractionContext,
    manifest: SourceManifest,
    fragment: GraphFragment,
}

impl Fixture {
    fn new(files: &[(&str, &[u8])]) -> Self {
        let repository = RepositoryRef {
            namespace: RepositoryNamespace::new("local").unwrap(),
            repository_id: RepositoryId::new("resolver-test").unwrap(),
        };
        let context = ExtractionContext {
            snapshot_id: SnapshotId::new("snapshot-resolver-test").unwrap(),
            build_id: BuildId::new("build-resolver-test").unwrap(),
            repository: repository.clone(),
            max_facts_per_file: 10_000,
            max_parser_duration_ms: 2_000,
            max_diagnostics: 1_000,
        };
        let descriptors = files
            .iter()
            .map(|(path, content)| SourceFileDescriptor {
                path: RepoPath::new(path).unwrap(),
                content_identity: content_digest(content),
                byte_len: content.len() as u64,
                file_mode: SourceFileMode::Regular,
            })
            .collect::<Vec<_>>();
        let manifest = SourceManifest {
            revision: SourceRevision {
                id: SourceRevisionId::new("revision-resolver-test").unwrap(),
                repository,
                source_kind: SourceKind::NonGitManifest,
                base_revision: None,
                manifest_digest: Digest::new("sha256", "00").unwrap(),
                analysis_config_digest: Digest::new("sha256", "11").unwrap(),
                dirty: false,
                includes_untracked: false,
            },
            extractor_set_digest: Digest::new("sha256", "22").unwrap(),
            files: descriptors.clone(),
            diagnostics: Vec::new(),
            metrics: SourceDiscoveryMetrics::default(),
        };
        let cargo = CargoExtractor::new();
        let rust = RustSyntaxExtractor::new();
        let mut fragment = GraphFragment::default();
        for ((_, content), file) in files.iter().zip(&descriptors) {
            let input = FileExtractionInput {
                context: &context,
                file,
                content,
            };
            if cargo.supports(file) {
                append(&mut fragment, cargo.extract(input).unwrap());
            }
            if rust.supports(file) {
                append(&mut fragment, rust.extract(input).unwrap());
            }
        }
        Self {
            context,
            manifest,
            fragment,
        }
    }

    fn resolve(self) -> GraphFragment {
        ConservativeResolver
            .resolve(CrossFileResolutionInput {
                context: &self.context,
                manifest: &self.manifest,
                fragment: self.fragment,
                budget: ResolutionBudget {
                    max_relationships: 10_000,
                    max_added_relationships: 10_000,
                    max_duration_ms: 2_000,
                    max_diagnostics: 1_000,
                },
            })
            .unwrap()
    }
}

fn content_digest(content: &[u8]) -> Digest {
    let bytes = Sha256::digest(content);
    let value = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Digest::new("sha256", value).unwrap()
}

fn append(target: &mut GraphFragment, mut fragment: GraphFragment) {
    target.nodes.append(&mut fragment.nodes);
    target.edges.append(&mut fragment.edges);
    target.diagnostics.append(&mut fragment.diagnostics);
}

fn node_named<'a>(fragment: &'a GraphFragment, kind: &str, name: &str) -> &'a GraphNode {
    fragment
        .nodes
        .iter()
        .find(|node| node.kind == kind && string_property(node, "name") == Some(name))
        .unwrap_or_else(|| panic!("missing {kind} named {name}"))
}

#[test]
fn resolves_packages_modules_imports_reexports_and_external_crates() {
    let fixture = Fixture::new(&[
        (
            "Cargo.toml",
            br#"[package]
name = "app"
version = "0.1.0"

[dependencies]
dep = { path = "dep" }
serde = "1"
"#,
        ),
        (
            "dep/Cargo.toml",
            br#"[package]
name = "dep"
version = "0.1.0"
"#,
        ),
        (
            "src/lib.rs",
            br#"mod api;
use crate::api::Request;
pub use crate::api::serve;
use dep::Thing;
use serde::Serialize;
"#,
        ),
        ("src/api.rs", b"pub struct Request;\npub fn serve() {}\n"),
        ("dep/src/lib.rs", b"pub struct Thing;\n"),
    ]);
    let fragment = fixture.resolve();

    let app = node_named(&fragment, "cargo_package", "app");
    let dep = node_named(&fragment, "cargo_package", "dep");
    let request = node_named(&fragment, "struct", "Request");
    let serve = node_named(&fragment, "function", "serve");
    let thing = node_named(&fragment, "struct", "Thing");

    assert!(fragment.edges.iter().any(|edge| {
        edge.kind == "depends_on"
            && edge.target == EdgeTarget::Node(dep.id.clone())
            && edge.provenance.resolution == ResolutionState::Resolved
    }));
    for target in [&request.id, &serve.id, &thing.id] {
        assert!(fragment.edges.iter().any(|edge| {
            matches!(edge.kind.as_str(), "imports" | "re_exports")
                && edge.target == EdgeTarget::Node((*target).clone())
        }));
    }
    assert!(fragment.edges.iter().any(|edge| {
            edge.kind == "imports"
                && matches!(&edge.target, EdgeTarget::External(path) if path == "rust-path:serde::Serialize")
                && edge.provenance.resolution == ResolutionState::External
        }));
    assert!(fragment.edges.iter().any(|edge| {
        edge.kind == "belongs_to_package" && edge.target == EdgeTarget::Node(app.id.clone())
    }));
    assert!(fragment.edges.iter().all(|edge| edge.kind != "calls"));
    assert!(fragment.edges.iter().all(|edge| edge.kind != "implements"));
}

#[test]
fn resolves_hyphenated_dependency_aliases_with_rust_crate_names() {
    let fragment = Fixture::new(&[
        (
            "Cargo.toml",
            br#"[package]
name = "app"
version = "0.1.0"

[dependencies]
internal-dep = { path = "internal-dep" }
async-trait = "0.1"
"#,
        ),
        (
            "internal-dep/Cargo.toml",
            b"[package]\nname='internal-dep'\nversion='0.1.0'\n",
        ),
        (
            "src/lib.rs",
            b"use internal_dep::Thing;\nuse async_trait::async_trait;\n",
        ),
        ("internal-dep/src/lib.rs", b"pub struct Thing;\n"),
    ])
    .resolve();
    let thing = node_named(&fragment, "struct", "Thing");

    assert!(fragment.edges.iter().any(|edge| {
        edge.kind == "imports" && edge.target == EdgeTarget::Node(thing.id.clone())
    }));
    assert!(fragment.edges.iter().any(|edge| {
        edge.kind == "imports"
            && matches!(
                &edge.target,
                EdgeTarget::External(path)
                    if path == "rust-path:async_trait::async_trait"
            )
    }));
    assert!(
        !fragment
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code.as_str() == "resolution.import_missing")
    );
}

#[test]
fn resolves_block_local_imports_from_the_enclosing_module() {
    let fragment = Fixture::new(&[
        ("Cargo.toml", b"[package]\nname='app'\nversion='0.1.0'\n"),
        (
            "src/lib.rs",
            b"mod api;\nfn load() { use crate::api::Api; }\n",
        ),
        ("src/api.rs", b"pub struct Api;\n"),
    ])
    .resolve();
    let api = node_named(&fragment, "struct", "Api");
    let load = node_named(&fragment, "function", "load");
    let import = fragment
        .nodes
        .iter()
        .find(|node| node.kind == "import")
        .expect("block-local import node");

    let import_edge = fragment
        .edges
        .iter()
        .find(|edge| edge.kind == "imports" && edge.target == EdgeTarget::Node(api.id.clone()))
        .expect("resolved block-local import edge");
    assert!(
        fragment
            .nodes
            .iter()
            .any(|node| node.id == import_edge.source && node.kind == "module")
    );
    assert!(fragment.edges.iter().any(|edge| {
        edge.kind == "contains"
            && edge.source == load.id
            && edge.target == EdgeTarget::Node(import.id.clone())
    }));
    assert!(
        !fragment
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.code.as_str() == "resolution.import_missing" })
    );
}

#[test]
fn resolves_imports_without_treating_impl_blocks_as_named_children() {
    let fragment = Fixture::new(&[(
        "src/lib.rs",
        br#"pub struct Foo;
impl Foo { pub fn new() -> Self { Self } }
mod consumer { use crate::Foo; }
"#,
    )])
    .resolve();
    let foo = node_named(&fragment, "struct", "Foo");

    assert!(fragment.nodes.iter().any(|node| node.kind == "impl"));
    assert!(
        fragment.edges.iter().any(|edge| {
            edge.kind == "imports" && edge.target == EdgeTarget::Node(foo.id.clone())
        })
    );
    assert!(
        !fragment
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code.as_str() == "resolution.import_ambiguous")
    );
}

#[test]
fn resolves_nested_inline_module_layout_and_super_imports() {
    let fragment = Fixture::new(&[
        ("Cargo.toml", b"[package]\nname='app'\nversion='0.1.0'\n"),
        (
            "src/lib.rs",
            br#"mod outer {
    pub struct Shared;
    mod nested;
    mod inline { use super::Shared; }
}
"#,
        ),
        ("src/outer/nested.rs", b"pub struct Nested;\n"),
    ])
    .resolve();
    let nested = node_named(&fragment, "module", "nested");
    let shared = node_named(&fragment, "struct", "Shared");
    assert!(fragment.edges.iter().any(|edge| {
        edge.kind == "declares_module" && edge.target == EdgeTarget::Node(nested.id.clone())
    }));
    assert!(fragment.edges.iter().any(|edge| {
        edge.kind == "imports" && edge.target == EdgeTarget::Node(shared.id.clone())
    }));
}

#[test]
fn resolves_workspace_membership_and_inherited_dependencies() {
    let fragment = Fixture::new(&[
        (
            "Cargo.toml",
            br#"[workspace]
members = ["crates/*"]
exclude = ["crates/excluded"]

[workspace.dependencies]
shared = { path = "crates/shared" }
serde = "1"
"#,
        ),
        (
            "crates/app/Cargo.toml",
            br#"[package]
name = "app"
version = "0.1.0"

[dependencies]
shared.workspace = true
serde.workspace = true
"#,
        ),
        (
            "crates/shared/Cargo.toml",
            b"[package]\nname='shared'\nversion='0.1.0'\n",
        ),
        (
            "crates/excluded/Cargo.toml",
            b"[package]\nname='excluded'\nversion='0.1.0'\n",
        ),
        (
            "crates/app/src/lib.rs",
            b"use shared::Shared;\nuse serde::Serialize;\n",
        ),
        ("crates/shared/src/lib.rs", b"pub struct Shared;\n"),
        ("crates/excluded/src/lib.rs", b"pub struct Excluded;\n"),
    ])
    .resolve();

    let workspace = fragment
        .nodes
        .iter()
        .find(|node| node.kind == "cargo_workspace")
        .unwrap();
    let app = node_named(&fragment, "cargo_package", "app");
    let shared_package = node_named(&fragment, "cargo_package", "shared");
    let excluded = node_named(&fragment, "cargo_package", "excluded");
    for package in [app, shared_package] {
        assert!(fragment.edges.iter().any(|edge| {
            edge.kind == "workspace_contains_package"
                && edge.source == workspace.id
                && edge.target == EdgeTarget::Node(package.id.clone())
        }));
    }
    assert!(!fragment.edges.iter().any(|edge| {
        edge.kind == "workspace_contains_package"
            && edge.target == EdgeTarget::Node(excluded.id.clone())
    }));
    assert!(fragment.edges.iter().any(|edge| {
        edge.kind == "depends_on"
            && edge.target == EdgeTarget::Node(shared_package.id.clone())
            && indexes_node(&fragment, &edge.source)
                .is_some_and(|node| string_property(node, "scope") == Some("normal"))
    }));
    assert!(fragment.edges.iter().any(|edge| {
            edge.kind == "imports"
                && matches!(&edge.target, EdgeTarget::External(path) if path == "rust-path:serde::Serialize")
        }));
}

#[test]
fn preserves_ambiguous_modules_and_unsupported_imports() {
    let fragment = Fixture::new(&[
        ("src/lib.rs", b"mod api;\nuse crate::api::{One, Two};\n"),
        ("src/api.rs", b"pub struct One;\n"),
        ("src/api/mod.rs", b"pub struct Two;\n"),
    ])
    .resolve();
    assert!(fragment.edges.iter().any(|edge| {
        edge.kind == "declares_module"
            && matches!(&edge.target, EdgeTarget::Unresolved(target) if target == "api")
    }));
    assert!(fragment.edges.iter().any(|edge| {
        edge.kind == "imports" && matches!(edge.target, EdgeTarget::Unresolved(_))
    }));
    let codes = fragment
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code.as_str())
        .collect::<BTreeSet<_>>();
    assert!(codes.contains("resolution.module_ambiguous"));
    assert!(codes.contains("resolution.import_unsupported"));
}

#[test]
fn preserves_path_overridden_modules_without_guessing() {
    let fragment = Fixture::new(&[
        ("src/lib.rs", b"#[path = \"different.rs\"]\nmod api;\n"),
        ("src/api.rs", b"pub struct WrongDefault;\n"),
        ("src/different.rs", b"pub struct Actual;\n"),
    ])
    .resolve();
    assert!(fragment.edges.iter().any(|edge| {
        edge.kind == "declares_module"
            && matches!(&edge.target, EdgeTarget::Unresolved(target) if target == "api")
    }));
    assert!(
        fragment
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.code.as_str() == "resolution.module_path_override" })
    );
}

#[test]
fn rejects_facts_from_another_manifest_and_snapshot() {
    let mut fixture = Fixture::new(&[("src/lib.rs", b"pub fn run() {}\n")]);
    fixture.manifest.files.clear();
    let error = ConservativeResolver
        .resolve(CrossFileResolutionInput {
            context: &fixture.context,
            manifest: &fixture.manifest,
            fragment: fixture.fragment,
            budget: ResolutionBudget {
                max_relationships: 10,
                max_added_relationships: 10,
                max_duration_ms: 10,
                max_diagnostics: 10,
            },
        })
        .unwrap_err();
    assert!(matches!(error, ResolutionError::InvalidEvidence(_)));
}

#[test]
fn zero_duration_preserves_facts_and_reports_a_bounded_timeout() {
    let fixture = Fixture::new(&[("src/lib.rs", b"mod missing;\n")]);
    let mut original_nodes = fixture.fragment.nodes.clone();
    let mut original_edges = fixture.fragment.edges.clone();
    original_nodes.sort_by(|left, right| left.id.cmp(&right.id));
    original_edges.sort_by(|left, right| left.id.cmp(&right.id));
    let fragment = ConservativeResolver
        .resolve(CrossFileResolutionInput {
            context: &fixture.context,
            manifest: &fixture.manifest,
            fragment: fixture.fragment,
            budget: ResolutionBudget {
                max_relationships: 10,
                max_added_relationships: 10,
                max_duration_ms: 0,
                max_diagnostics: 1,
            },
        })
        .unwrap();
    assert_eq!(fragment.nodes, original_nodes);
    assert_eq!(fragment.edges, original_edges);
    assert_eq!(fragment.diagnostics.len(), 1);
    assert_eq!(fragment.diagnostics[0].code.as_str(), "resolution.timeout");
}

#[test]
fn relationship_limit_preserves_unresolved_facts() {
    let fixture = Fixture::new(&[
        ("src/lib.rs", b"mod api;\n"),
        ("src/api.rs", b"pub struct Api;\n"),
    ]);
    let fragment = ConservativeResolver
        .resolve(CrossFileResolutionInput {
            context: &fixture.context,
            manifest: &fixture.manifest,
            fragment: fixture.fragment,
            budget: ResolutionBudget {
                max_relationships: 0,
                max_added_relationships: 0,
                max_duration_ms: 1_000,
                max_diagnostics: 1,
            },
        })
        .unwrap();
    assert!(fragment.edges.iter().any(|edge| {
        edge.kind == "declares_module" && matches!(edge.target, EdgeTarget::Unresolved(_))
    }));
    assert!(
        fragment
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.code.as_str() == "resolution.relationship_limit" })
    );
}

#[test]
fn addition_limit_blocks_new_edges_without_blocking_replacements() {
    let fixture = Fixture::new(&[
        ("Cargo.toml", b"[package]\nname='app'\nversion='0.1.0'\n"),
        ("src/lib.rs", b"mod api;\nuse crate::api::Api;\n"),
        ("src/api.rs", b"pub struct Api;\n"),
    ]);
    let original_edges = fixture.fragment.edges.len();
    let fragment = ConservativeResolver
        .resolve(CrossFileResolutionInput {
            context: &fixture.context,
            manifest: &fixture.manifest,
            fragment: fixture.fragment,
            budget: ResolutionBudget {
                max_relationships: 1_000,
                max_added_relationships: 0,
                max_duration_ms: 1_000,
                max_diagnostics: 10,
            },
        })
        .unwrap();

    assert!(fragment.edges.len() <= original_edges);
    assert!(fragment.edges.iter().any(|edge| {
        edge.kind == "declares_module" && matches!(edge.target, EdgeTarget::Node(_))
    }));
    assert!(
        fragment
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.code.as_str() == "resolution.relationship_limit" })
    );
}

#[test]
fn resolution_is_idempotent() {
    let fixture = Fixture::new(&[
        ("Cargo.toml", b"[package]\nname='app'\nversion='0.1.0'\n"),
        ("src/lib.rs", b"mod api;\nuse crate::api::Api;\n"),
        ("src/api.rs", b"pub struct Api;\n"),
    ]);
    let budget = ResolutionBudget {
        max_relationships: 1_000,
        max_added_relationships: 1_000,
        max_duration_ms: 1_000,
        max_diagnostics: 100,
    };
    let first = ConservativeResolver
        .resolve(CrossFileResolutionInput {
            context: &fixture.context,
            manifest: &fixture.manifest,
            fragment: fixture.fragment,
            budget,
        })
        .unwrap();
    let second = ConservativeResolver
        .resolve(CrossFileResolutionInput {
            context: &fixture.context,
            manifest: &fixture.manifest,
            fragment: first.clone(),
            budget,
        })
        .unwrap();
    assert_eq!(first.nodes, second.nodes);
    assert_eq!(first.edges, second.edges);
    assert_eq!(first.diagnostics, second.diagnostics);
}

fn indexes_node<'a>(fragment: &'a GraphFragment, id: &NodeId) -> Option<&'a GraphNode> {
    fragment.nodes.iter().find(|node| &node.id == id)
}
