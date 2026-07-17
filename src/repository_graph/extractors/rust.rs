//! Tolerant, syntax-only Rust extraction.
//!
//! This extractor deliberately uses only Tree-sitter's concrete syntax tree. It
//! never invokes rustc, Cargo, macro expansion, build scripts, language servers,
//! or repository-provided code. Import and module targets remain unresolved for
//! the conservative cross-file resolver.

use std::{
    collections::BTreeMap,
    ops::ControlFlow,
    time::{Duration, Instant},
};

use thiserror::Error;
use tree_sitter::{Node, ParseOptions, Parser, Point};

use super::super::{
    EXTRACTOR_CONTRACT_VERSION,
    domain::{
        Confidence, DiagnosticCode, DiagnosticLocation, DiagnosticSeverity, EdgeTarget,
        ExtractorId, ExtractorIdentity, FactProvenance, GraphDiagnostic, GraphEdge, GraphNode,
        GraphValue, NodeId, ResolutionState, SemanticKey, SourceEvidence, SourcePosition,
        SourceSpan,
    },
    ports::{Extractor, FileExtractionInput, GraphFragment, SourceFileDescriptor},
};
use super::{deterministic_edge_id, deterministic_node_id};

const EXTRACTOR_ID: &str = "builtin.rust-syntax";
const EXTRACTOR_VERSION: &str = "1.0.0+tree-sitter-0.26.11.rust-0.24.2";
const MAX_SIGNATURE_BYTES: usize = 4 * 1024;
const MAX_SOURCE_VALUE_BYTES: usize = 16 * 1024;

#[derive(Debug, Error)]
pub enum RustExtractorError {
    #[error("the bundled Rust grammar is incompatible with the parser runtime")]
    IncompatibleLanguage(#[source] tree_sitter::LanguageError),
}

/// Stateless Rust syntax extractor. A fresh parser is used for every call so an
/// instance can be shared by future parallel indexing workers without locks.
#[derive(Debug, Clone, Copy, Default)]
pub struct RustSyntaxExtractor;

impl RustSyntaxExtractor {
    pub fn new() -> Self {
        Self
    }
}

impl Extractor for RustSyntaxExtractor {
    type Error = RustExtractorError;

    fn identity(&self) -> ExtractorIdentity {
        extractor_identity()
    }

    fn supports(&self, file: &SourceFileDescriptor) -> bool {
        file.path.as_str().ends_with(".rs")
    }

    fn extract(&self, input: FileExtractionInput<'_>) -> Result<GraphFragment, Self::Error> {
        let mut diagnostics = DiagnosticBuffer::new(input);
        if !self.supports(input.file) {
            return Ok(GraphFragment::default());
        }
        if std::str::from_utf8(input.content).is_err() {
            diagnostics.push("rust.invalid_utf8", None);
            return Ok(GraphFragment {
                diagnostics: diagnostics.finish(),
                ..GraphFragment::default()
            });
        }

        let started = Instant::now();
        let budget = Duration::from_millis(input.context.max_parser_duration_ms);
        if budget.is_zero() {
            diagnostics.push("rust.parser_timeout", None);
            return Ok(GraphFragment {
                diagnostics: diagnostics.finish(),
                ..GraphFragment::default()
            });
        }

        let mut parser = Parser::new();
        let language = tree_sitter_rust::LANGUAGE.into();
        parser
            .set_language(&language)
            .map_err(RustExtractorError::IncompatibleLanguage)?;

        let mut progress = |_state: &tree_sitter::ParseState| {
            if started.elapsed() >= budget {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let options = ParseOptions::new().progress_callback(&mut progress);
        let mut read =
            |offset: usize, _position: Point| input.content.get(offset..).unwrap_or_default();
        let Some(tree) = parser.parse_with_options(&mut read, None, Some(options)) else {
            diagnostics.push("rust.parser_timeout", None);
            return Ok(GraphFragment {
                diagnostics: diagnostics.finish(),
                ..GraphFragment::default()
            });
        };

        let mut facts = FactBuffer::new(input.context.max_facts_per_file);
        let root = tree.root_node();
        let root_id = node_id(input, "module", root.start_byte(), root.end_byte());
        let root_name = file_module_name(input.file.path.as_str());
        let mut root_properties = BTreeMap::from([
            ("name".to_string(), GraphValue::String(root_name.clone())),
            (
                "qualified_name".to_string(),
                GraphValue::String(input.file.path.as_str().to_string()),
            ),
            (
                "module_origin".to_string(),
                GraphValue::String("file".to_string()),
            ),
            (
                "visibility".to_string(),
                GraphValue::String("inherited".to_string()),
            ),
        ]);
        root_properties.insert(
            "language".to_string(),
            GraphValue::String("rust".to_string()),
        );
        let root_fact = graph_node(
            input,
            root_id.clone(),
            "module",
            Some(format!("rust:file-module:{}", input.file.path.as_str())),
            root_properties,
            span(root),
        );
        if !facts.push_root(root_fact) {
            diagnostics.push("rust.fact_limit", None);
            return Ok(GraphFragment {
                diagnostics: diagnostics.finish(),
                ..GraphFragment::default()
            });
        }

        let mut stack = Vec::new();
        let mut timed_out = !push_named_children(
            &mut stack,
            root,
            root_id.clone(),
            root_id,
            vec![root_name],
            started,
            budget,
        );

        while !timed_out {
            let Some(work) = stack.pop() else {
                break;
            };
            if started.elapsed() >= budget {
                timed_out = true;
                break;
            }

            if work.node.is_error() {
                diagnostics.push("rust.parse_error", Some(span(work.node)));
            } else if work.node.is_missing() {
                diagnostics.push("rust.missing_syntax", Some(span(work.node)));
            }

            let mut child_parent = work.parent_id.clone();
            let mut child_module = work.module_id.clone();
            let mut child_scope = work.scope.clone();
            if let Some(mut declaration) = declaration(work.node, input.content) {
                if declaration.enforce_source_value_limit() {
                    diagnostics.push("rust.source_value_limit", Some(span(work.node)));
                }
                let opens_module = declaration.kind == "module";
                let built = build_declaration(input, &work, declaration);
                if !facts.push_declaration(built.node, built.containment, built.relationship) {
                    break;
                }
                child_parent = built.id.clone();
                if opens_module {
                    child_module = built.id;
                }
                if let Some(scope_segment) = built.scope_segment {
                    child_scope.push(scope_segment);
                }
            } else if work.node.kind() == "block" {
                // Rust permits block-local items with the same name in separate
                // blocks. Preserve that otherwise-anonymous lexical scope.
                child_scope.push(format!("block@{}", work.node.start_byte()));
            }

            if !push_named_children(
                &mut stack,
                work.node,
                child_parent,
                child_module,
                child_scope,
                started,
                budget,
            ) {
                timed_out = true;
            }
        }

        if timed_out {
            let mut timeout = DiagnosticBuffer::new(input);
            timeout.push("rust.parser_timeout", None);
            return Ok(GraphFragment {
                diagnostics: timeout.finish(),
                ..GraphFragment::default()
            });
        }
        if facts.truncated {
            diagnostics.push("rust.fact_limit", None);
        }

        Ok(GraphFragment {
            nodes: facts.nodes,
            edges: facts.edges,
            diagnostics: diagnostics.finish(),
        })
    }
}

#[derive(Debug)]
struct WorkItem<'tree> {
    node: Node<'tree>,
    parent_id: NodeId,
    module_id: NodeId,
    scope: Vec<String>,
}

fn push_named_children<'tree>(
    stack: &mut Vec<WorkItem<'tree>>,
    node: Node<'tree>,
    parent_id: NodeId,
    module_id: NodeId,
    scope: Vec<String>,
    started: Instant,
    budget: Duration,
) -> bool {
    for index in (0..node.named_child_count()).rev() {
        if started.elapsed() >= budget {
            return false;
        }
        let Ok(index) = u32::try_from(index) else {
            continue;
        };
        if let Some(child) = node.named_child(index) {
            stack.push(WorkItem {
                node: child,
                parent_id: parent_id.clone(),
                module_id: module_id.clone(),
                scope: scope.clone(),
            });
        }
    }
    true
}

#[derive(Debug)]
struct Declaration {
    kind: &'static str,
    name: Option<String>,
    signature: String,
    visibility: String,
    relationship: Option<UnresolvedRelationship>,
    opens_scope: bool,
    path_override: bool,
}

#[derive(Debug)]
struct UnresolvedRelationship {
    kind: &'static str,
    target: String,
}

impl Declaration {
    /// Drops source-derived identity values that are too large to persist as a
    /// bounded fact. The declaration itself remains visible by source span.
    fn enforce_source_value_limit(&mut self) -> bool {
        let mut exceeded = false;
        if self
            .name
            .as_ref()
            .is_some_and(|name| name.len() > MAX_SOURCE_VALUE_BYTES)
        {
            self.name = None;
            exceeded = true;
        }
        if self
            .relationship
            .as_ref()
            .is_some_and(|relationship| relationship.target.len() > MAX_SOURCE_VALUE_BYTES)
        {
            self.relationship = None;
            exceeded = true;
        }
        if self.visibility.len() > MAX_SOURCE_VALUE_BYTES {
            self.visibility = "unavailable".to_string();
            exceeded = true;
        }
        exceeded
    }
}

fn declaration(node: Node<'_>, source: &[u8]) -> Option<Declaration> {
    let (kind, opens_scope) = match node.kind() {
        "struct_item" => ("struct", true),
        "enum_item" => ("enum", true),
        "trait_item" => ("trait", true),
        "function_item" | "function_signature_item" => ("function", true),
        "const_item" => ("constant", false),
        "type_item" => ("type_alias", false),
        "impl_item" => ("impl", true),
        "mod_item" if node.child_by_field_name("body").is_some() => ("module", true),
        "mod_item" => ("mod_declaration", false),
        "use_declaration" => {
            let visibility = visibility(node, source);
            let target = node
                .child_by_field_name("argument")
                .and_then(|argument| node_text(argument, source))
                .unwrap_or_default();
            let kind = if visibility.starts_with("pub") {
                "re_export"
            } else {
                "import"
            };
            let relationship = (!target.is_empty()).then_some(UnresolvedRelationship {
                kind: if kind == "re_export" {
                    "re_exports"
                } else {
                    "imports"
                },
                target,
            });
            return Some(Declaration {
                kind,
                name: None,
                signature: signature(node, source),
                visibility,
                relationship,
                opens_scope: false,
                path_override: false,
            });
        }
        _ => return None,
    };

    let name = if kind == "impl" {
        impl_name(node, source)
    } else {
        node.child_by_field_name("name")
            .and_then(|name| node_text(name, source))
    };
    let relationship = (kind == "mod_declaration")
        .then(|| name.clone())
        .flatten()
        .filter(|target| !target.is_empty())
        .map(|target| UnresolvedRelationship {
            kind: "declares_module",
            target,
        });
    Some(Declaration {
        kind,
        name,
        signature: signature(node, source),
        visibility: visibility(node, source),
        relationship,
        opens_scope,
        path_override: kind == "mod_declaration" && has_path_attribute(node, source),
    })
}

fn has_path_attribute(node: Node<'_>, source: &[u8]) -> bool {
    let child_attribute = (0..node.named_child_count()).any(|index| {
        u32::try_from(index)
            .ok()
            .and_then(|index| node.named_child(index))
            .is_some_and(|child| is_path_attribute(child, source))
    });
    if child_attribute {
        return true;
    }
    let mut sibling = node.prev_named_sibling();
    while let Some(attribute) = sibling.filter(|sibling| sibling.kind() == "attribute_item") {
        if is_path_attribute(attribute, source) {
            return true;
        }
        sibling = attribute.prev_named_sibling();
    }
    false
}

fn is_path_attribute(node: Node<'_>, source: &[u8]) -> bool {
    if node.kind() != "attribute_item" {
        return false;
    }
    node_text(node, source).is_some_and(|attribute| {
        let compact = attribute
            .chars()
            .filter(|character| !character.is_whitespace());
        compact.collect::<String>().starts_with("#[path=")
    })
}

fn impl_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let implemented_type = node
        .child_by_field_name("type")
        .and_then(|part| node_text(part, source));
    let implemented_trait = node
        .child_by_field_name("trait")
        .and_then(|part| node_text(part, source));
    match (implemented_trait, implemented_type) {
        (Some(trait_name), Some(type_name)) => Some(format!("{trait_name} for {type_name}")),
        (None, Some(type_name)) => Some(type_name),
        _ => None,
    }
}

fn visibility(node: Node<'_>, source: &[u8]) -> String {
    for index in 0..node.named_child_count() {
        let Ok(index) = u32::try_from(index) else {
            continue;
        };
        let Some(child) = node.named_child(index) else {
            continue;
        };
        if child.kind() == "visibility_modifier" {
            return node_text(child, source).unwrap_or_else(|| "inherited".to_string());
        }
    }
    "inherited".to_string()
}

fn signature(node: Node<'_>, source: &[u8]) -> String {
    let body = node.child_by_field_name("body");
    let initializer = (node.kind() == "const_item")
        .then(|| node.child_by_field_name("value"))
        .flatten();
    let end = body
        .or(initializer)
        .map_or_else(|| node.end_byte(), |body| body.start_byte());
    let bytes = source.get(node.start_byte()..end).unwrap_or_default();
    let text = std::str::from_utf8(bytes).unwrap_or_default().trim();
    truncate_utf8(text, MAX_SIGNATURE_BYTES)
        .trim_end_matches(&[';', '='][..])
        .trim_end()
        .to_string()
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    &value[..boundary]
}

fn node_text(node: Node<'_>, source: &[u8]) -> Option<String> {
    std::str::from_utf8(source.get(node.byte_range())?)
        .ok()
        .map(ToString::to_string)
}

struct BuiltDeclaration {
    id: NodeId,
    node: GraphNode,
    containment: GraphEdge,
    relationship: Option<GraphEdge>,
    scope_segment: Option<String>,
}

fn build_declaration(
    input: FileExtractionInput<'_>,
    work: &WorkItem<'_>,
    declaration: Declaration,
) -> BuiltDeclaration {
    let id = node_id(
        input,
        declaration.kind,
        work.node.start_byte(),
        work.node.end_byte(),
    );
    let display_name = declaration
        .name
        .clone()
        .unwrap_or_else(|| format!("{}@{}", declaration.kind, work.node.start_byte()));
    let identity_name = if declaration.kind == "impl" {
        format!("impl<{display_name}>@{}", work.node.start_byte())
    } else {
        display_name.clone()
    };
    let qualified_name = work
        .scope
        .iter()
        .chain(std::iter::once(&identity_name))
        .cloned()
        .collect::<Vec<_>>()
        .join("::");
    let semantic_key = format!(
        "rust:{}:{}:{}",
        declaration.kind,
        input.file.path.as_str(),
        qualified_name
    );
    let mut properties = BTreeMap::from([
        (
            "qualified_name".to_string(),
            GraphValue::String(qualified_name),
        ),
        (
            "signature".to_string(),
            GraphValue::String(declaration.signature),
        ),
        (
            "visibility".to_string(),
            GraphValue::String(declaration.visibility),
        ),
    ]);
    if let Some(name) = &declaration.name {
        properties.insert("name".to_string(), GraphValue::String(name.clone()));
    }
    if declaration.kind == "module" {
        properties.insert(
            "module_origin".to_string(),
            GraphValue::String("inline".to_string()),
        );
    } else if declaration.kind == "mod_declaration" {
        properties.insert(
            "module_origin".to_string(),
            GraphValue::String("external_declaration".to_string()),
        );
        if declaration.path_override {
            properties.insert("path_override".to_string(), GraphValue::Boolean(true));
        }
    }
    if declaration.kind == "impl" {
        properties.insert(
            "implementation_target".to_string(),
            GraphValue::String(display_name.clone()),
        );
    }
    if let Some(relationship) = &declaration.relationship {
        properties.insert(
            "target".to_string(),
            GraphValue::String(relationship.target.clone()),
        );
    }

    let declaration_span = span(work.node);
    let graph_node = graph_node(
        input,
        id.clone(),
        declaration.kind,
        Some(semantic_key),
        properties,
        declaration_span.clone(),
    );
    let containment = graph_edge(
        input,
        "contains",
        work.parent_id.clone(),
        EdgeTarget::Node(id.clone()),
        declaration_span.clone(),
        ResolutionState::Resolved,
    );
    let relationship = declaration.relationship.map(|relationship| {
        graph_edge(
            input,
            relationship.kind,
            work.module_id.clone(),
            EdgeTarget::Unresolved(relationship.target),
            declaration_span,
            ResolutionState::Unresolved,
        )
    });
    BuiltDeclaration {
        id,
        node: graph_node,
        containment,
        relationship,
        scope_segment: declaration.opens_scope.then_some(identity_name),
    }
}

fn graph_node(
    input: FileExtractionInput<'_>,
    id: NodeId,
    kind: &str,
    semantic_key: Option<String>,
    properties: BTreeMap<String, GraphValue>,
    source_span: SourceSpan,
) -> GraphNode {
    GraphNode {
        snapshot_id: input.context.snapshot_id.clone(),
        id,
        kind: kind.to_string(),
        semantic_key: semantic_key
            .map(|key| SemanticKey::new(key).expect("Rust semantic keys are always non-empty")),
        provenance: provenance(input, source_span, ResolutionState::Resolved),
        properties,
    }
}

fn graph_edge(
    input: FileExtractionInput<'_>,
    kind: &str,
    source: NodeId,
    target: EdgeTarget,
    source_span: SourceSpan,
    resolution: ResolutionState,
) -> GraphEdge {
    let id = deterministic_edge_id(
        &extractor_identity(),
        kind,
        &source,
        &target,
        &format!(
            "{}:{}",
            input.file.path.as_str(),
            source_span.start.byte_offset
        ),
    );
    GraphEdge {
        snapshot_id: input.context.snapshot_id.clone(),
        id,
        kind: kind.to_string(),
        source,
        target,
        provenance: provenance(input, source_span, resolution),
        properties: BTreeMap::new(),
    }
}

fn provenance(
    input: FileExtractionInput<'_>,
    source_span: SourceSpan,
    resolution: ResolutionState,
) -> FactProvenance {
    FactProvenance {
        extractor: extractor_identity(),
        evidence: Some(SourceEvidence {
            path: input.file.path.clone(),
            content_identity: input.file.content_identity.clone(),
            span: Some(source_span),
        }),
        resolution,
        confidence: Confidence::Exact,
    }
}

fn extractor_identity() -> ExtractorIdentity {
    ExtractorIdentity {
        id: ExtractorId::new(EXTRACTOR_ID).expect("built-in extractor ID is non-empty"),
        version: EXTRACTOR_VERSION.to_string(),
        contract_version: EXTRACTOR_CONTRACT_VERSION,
    }
}

fn node_id(input: FileExtractionInput<'_>, kind: &str, start: usize, end: usize) -> NodeId {
    deterministic_node_id(
        &extractor_identity(),
        kind,
        &format!("{}:{start}:{end}", input.file.path.as_str()),
    )
}

fn file_module_name(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    match file {
        "lib.rs" | "main.rs" => "crate".to_string(),
        "mod.rs" => path
            .rsplit_once('/')
            .and_then(|(parent, _)| parent.rsplit('/').next())
            .unwrap_or("crate")
            .to_string(),
        _ => file.strip_suffix(".rs").unwrap_or(file).to_string(),
    }
}

fn span(node: Node<'_>) -> SourceSpan {
    SourceSpan {
        start: position(node.start_byte(), node.start_position()),
        end: position(node.end_byte(), node.end_position()),
    }
}

fn position(byte_offset: usize, point: Point) -> SourcePosition {
    SourcePosition {
        byte_offset: byte_offset as u64,
        // Coordinates shown to users are one-based; byte spans remain half-open.
        line: Some(point.row.saturating_add(1) as u32),
        column: Some(point.column.saturating_add(1) as u32),
    }
}

struct FactBuffer {
    limit: u64,
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    truncated: bool,
}

impl FactBuffer {
    fn new(limit: u64) -> Self {
        Self {
            limit,
            nodes: Vec::new(),
            edges: Vec::new(),
            truncated: false,
        }
    }

    fn push_root(&mut self, node: GraphNode) -> bool {
        if !self.reserve(1) {
            return false;
        }
        self.nodes.push(node);
        true
    }

    fn push_declaration(
        &mut self,
        node: GraphNode,
        containment: GraphEdge,
        relationship: Option<GraphEdge>,
    ) -> bool {
        let required = 2 + u64::from(relationship.is_some());
        if !self.reserve(required) {
            return false;
        }
        self.nodes.push(node);
        self.edges.push(containment);
        if let Some(relationship) = relationship {
            self.edges.push(relationship);
        }
        true
    }

    fn reserve(&mut self, count: u64) -> bool {
        let used = self.nodes.len() as u64 + self.edges.len() as u64;
        if used.saturating_add(count) > self.limit {
            self.truncated = true;
            false
        } else {
            true
        }
    }
}

struct DiagnosticBuffer<'a> {
    input: FileExtractionInput<'a>,
    limit: u64,
    diagnostics: Vec<GraphDiagnostic>,
    suppressed: u64,
}

impl<'a> DiagnosticBuffer<'a> {
    fn new(input: FileExtractionInput<'a>) -> Self {
        Self {
            limit: input.context.max_diagnostics,
            input,
            diagnostics: Vec::new(),
            suppressed: 0,
        }
    }

    fn push(&mut self, code: &'static str, source_span: Option<SourceSpan>) {
        if self.diagnostics.len() as u64 >= self.limit {
            self.suppressed = self.suppressed.saturating_add(1);
            return;
        }
        self.diagnostics.push(self.diagnostic(code, source_span));
    }

    fn finish(mut self) -> Vec<GraphDiagnostic> {
        if self.suppressed > 0 && self.limit > 0 {
            let replaced = u64::from(!self.diagnostics.is_empty());
            let suppressed = self.suppressed.saturating_add(replaced);
            let mut summary = self.diagnostic("rust.diagnostics_truncated", None);
            summary.metrics.insert(
                DiagnosticCode::new("suppressed").expect("static diagnostic metric is valid"),
                i64::try_from(suppressed).unwrap_or(i64::MAX),
            );
            if self.diagnostics.is_empty() {
                self.diagnostics.push(summary);
            } else if let Some(last) = self.diagnostics.last_mut() {
                *last = summary;
            }
        }
        self.diagnostics
    }

    fn diagnostic(&self, code: &'static str, source_span: Option<SourceSpan>) -> GraphDiagnostic {
        GraphDiagnostic {
            build_id: self.input.context.build_id.clone(),
            snapshot_id: Some(self.input.context.snapshot_id.clone()),
            severity: DiagnosticSeverity::Warning,
            code: DiagnosticCode::new(code).expect("static Rust diagnostic code is valid"),
            location: source_span.map(|source_span| DiagnosticLocation {
                path: self.input.file.path.clone(),
                span: Some(source_span),
            }),
            metrics: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
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
}
