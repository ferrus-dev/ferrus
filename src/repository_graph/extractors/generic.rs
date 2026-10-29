//! Generic repository structure and file-role extraction.
//!
//! The extractor derives facts solely from the immutable source manifest and
//! content-verified file bytes supplied by the coordinator. It never opens a
//! path, invokes a project tool, or retains source text in graph properties or
//! diagnostics.

use std::{collections::BTreeMap, convert::Infallible};

use super::{deterministic_edge_id, deterministic_node_id};
use crate::repository_graph::{
    EXTRACTOR_CONTRACT_VERSION,
    domain::{
        Confidence, DiagnosticCode, DiagnosticLocation, DiagnosticSeverity, EdgeTarget,
        ExtractorId, ExtractorIdentity, FactProvenance, GraphDiagnostic, GraphEdge, GraphNode,
        GraphValue, RepoPath, ResolutionState, SemanticKey, SourceEvidence, SourcePosition,
        SourceSpan,
    },
    ports::{
        ExtractionContext, Extractor, FileExtractionInput, GraphFragment, SourceFileDescriptor,
        SourceFileMode, SourceManifest,
    },
};

const EXTRACTOR_ID: &str = "builtin.generic-structure";
const EXTRACTOR_VERSION: &str = "1.0.0";

const REPOSITORY_KIND: &str = "repository";
const DIRECTORY_KIND: &str = "directory";
const FILE_KIND: &str = "file";
const DOCUMENT_KIND: &str = "document";
const MANIFEST_KIND: &str = "manifest";
const CONFIGURATION_KIND: &str = "configuration";
const ENTRY_POINT_KIND: &str = "entry_point";
const CONTAINS_EDGE: &str = "contains";
const CLASSIFIED_AS_EDGE: &str = "classified_as";

/// Stateless extractor for repository structure and conventional file roles.
#[derive(Debug, Clone, Copy, Default)]
pub struct GenericExtractor;

impl GenericExtractor {
    pub fn new() -> Self {
        Self
    }

    /// Emits the single repository node, the logical root directory, and their
    /// containment relationship. Directory ancestors below the root are
    /// emitted by per-file extraction, using the same deterministic IDs, so a
    /// coordinator can deduplicate fragments without shared mutable state.
    pub fn repository_fragment(
        &self,
        context: &ExtractionContext,
        manifest: &SourceManifest,
    ) -> GraphFragment {
        let identity = extractor_identity();
        let mut builder = FragmentBuilder::new(context, None);
        if manifest.revision.repository != context.repository {
            builder.push_diagnostic(
                const {
                    crate::repository_graph::domain::DiagnosticCode::from_static(
                        "generic.repository_mismatch",
                    )
                },
                None,
            );
            return builder.finish();
        }

        let repository_key = format!(
            "{}:{}:{}:{}",
            REPOSITORY_KIND,
            context.repository.namespace.as_str().len(),
            context.repository.namespace.as_str(),
            context.repository.repository_id.as_str()
        );
        let repository_id = deterministic_node_id(&identity, REPOSITORY_KIND, &repository_key);
        let mut repository_properties = BTreeMap::new();
        repository_properties.insert(
            "namespace".to_string(),
            GraphValue::String(context.repository.namespace.as_str().to_string()),
        );
        repository_properties.insert(
            "repository_id".to_string(),
            GraphValue::String(context.repository.repository_id.as_str().to_string()),
        );
        repository_properties.insert(
            "file_count".to_string(),
            GraphValue::Integer(saturating_i64(manifest.files.len() as u64)),
        );
        repository_properties.insert(
            "total_bytes".to_string(),
            GraphValue::Integer(saturating_i64(
                manifest
                    .files
                    .iter()
                    .fold(0_u64, |total, file| total.saturating_add(file.byte_len)),
            )),
        );
        let repository = graph_node(
            context,
            &identity,
            repository_id.clone(),
            REPOSITORY_KIND,
            &repository_key,
            repository_properties,
            None,
        );
        if !builder.push_group(vec![repository], Vec::new()) {
            return builder.finish();
        }

        let root_key = directory_key(".");
        let root_id = deterministic_node_id(&identity, DIRECTORY_KIND, &root_key);
        let root = graph_node(
            context,
            &identity,
            root_id.clone(),
            DIRECTORY_KIND,
            &root_key,
            BTreeMap::from([("path".to_string(), GraphValue::String(".".to_string()))]),
            None,
        );
        let edge = graph_edge(
            context,
            &identity,
            CONTAINS_EDGE,
            repository_id,
            EdgeTarget::Node(root_id),
            "repository-root",
            None,
        );
        builder.push_group(vec![root], vec![edge]);
        builder.finish()
    }

    fn extract_file(&self, input: FileExtractionInput<'_>) -> GraphFragment {
        let identity = extractor_identity();
        let span = whole_file_span(input.content);
        let evidence = SourceEvidence {
            path: input.file.path.clone(),
            content_identity: input.file.content_identity.clone(),
            span: Some(span),
        };
        let mut builder = FragmentBuilder::new(input.context, Some(input.file.path.clone()));

        let root_key = directory_key(".");
        let mut parent_id = deterministic_node_id(&identity, DIRECTORY_KIND, &root_key);
        let root = graph_node(
            input.context,
            &identity,
            parent_id.clone(),
            DIRECTORY_KIND,
            &root_key,
            BTreeMap::from([("path".to_string(), GraphValue::String(".".to_string()))]),
            None,
        );
        if !builder.push_group(vec![root], Vec::new()) {
            return builder.finish();
        }

        for directory in ancestor_directories(input.file.path.as_str()) {
            let key = directory_key(directory);
            let directory_id = deterministic_node_id(&identity, DIRECTORY_KIND, &key);
            let node = graph_node(
                input.context,
                &identity,
                directory_id.clone(),
                DIRECTORY_KIND,
                &key,
                BTreeMap::from([(
                    "path".to_string(),
                    GraphValue::String(directory.to_string()),
                )]),
                None,
            );
            let edge = graph_edge(
                input.context,
                &identity,
                CONTAINS_EDGE,
                parent_id,
                EdgeTarget::Node(directory_id.clone()),
                directory,
                None,
            );
            if !builder.push_group(vec![node], vec![edge]) {
                return builder.finish();
            }

            parent_id = directory_id;
        }

        let file_key = file_key(input.file.path.as_str());
        let file_id = deterministic_node_id(&identity, FILE_KIND, &file_key);
        let mut file_properties = BTreeMap::new();
        file_properties.insert(
            "path".to_string(),
            GraphValue::String(input.file.path.as_str().to_string()),
        );
        file_properties.insert(
            "byte_len".to_string(),
            GraphValue::Integer(saturating_i64(input.file.byte_len)),
        );
        file_properties.insert(
            "executable".to_string(),
            GraphValue::Boolean(input.file.file_mode == SourceFileMode::Executable),
        );
        if let Some(extension) = extension(input.file.path.as_str()) {
            file_properties.insert(
                "extension".to_string(),
                GraphValue::String(extension.to_string()),
            );
        }

        let file_node = graph_node(
            input.context,
            &identity,
            file_id.clone(),
            FILE_KIND,
            &file_key,
            file_properties,
            Some(evidence.clone()),
        );
        let containment = graph_edge(
            input.context,
            &identity,
            CONTAINS_EDGE,
            parent_id,
            EdgeTarget::Node(file_id.clone()),
            input.file.path.as_str(),
            Some(evidence.clone()),
        );
        if !builder.push_group(vec![file_node], vec![containment]) {
            return builder.finish();
        }

        if input.content.contains(&0) {
            builder.push_diagnostic(
                const {
                    crate::repository_graph::domain::DiagnosticCode::from_static(
                        "generic.binary_content_skipped",
                    )
                },
                Some(evidence),
            );
            return builder.finish();
        }

        if std::str::from_utf8(input.content).is_err() {
            builder.push_diagnostic(
                const {
                    crate::repository_graph::domain::DiagnosticCode::from_static(
                        "generic.non_utf8_content_skipped",
                    )
                },
                Some(evidence),
            );
            return builder.finish();
        }

        for classification in classifications(input.file.path.as_str()) {
            let local_key = format!(
                "{}:{}:{}:{}",
                classification.kind,
                input.file.path.as_str().len(),
                input.file.path.as_str(),
                classification.value
            );
            let classified_id = deterministic_node_id(&identity, classification.kind, &local_key);
            let node = graph_node(
                input.context,
                &identity,
                classified_id.clone(),
                classification.kind,
                &local_key,
                BTreeMap::from([
                    (
                        "path".to_string(),
                        GraphValue::String(input.file.path.as_str().to_string()),
                    ),
                    (
                        classification.property.to_string(),
                        GraphValue::String(classification.value.to_string()),
                    ),
                ]),
                Some(evidence.clone()),
            );
            let edge = graph_edge(
                input.context,
                &identity,
                CLASSIFIED_AS_EDGE,
                file_id.clone(),
                EdgeTarget::Node(classified_id),
                classification.kind,
                Some(evidence.clone()),
            );
            if !builder.push_group(vec![node], vec![edge]) {
                break;
            }
        }

        builder.finish()
    }
}

impl Extractor for GenericExtractor {
    type Error = Infallible;

    fn identity(&self) -> ExtractorIdentity {
        extractor_identity()
    }

    fn supports(&self, _file: &SourceFileDescriptor) -> bool {
        true
    }

    fn extract(&self, input: FileExtractionInput<'_>) -> Result<GraphFragment, Self::Error> {
        Ok(self.extract_file(input))
    }
}

struct FragmentBuilder<'a> {
    context: &'a ExtractionContext,
    location: Option<RepoPath>,
    fragment: GraphFragment,
    facts: u64,
    limit_reached: bool,
}

impl<'a> FragmentBuilder<'a> {
    fn new(context: &'a ExtractionContext, location: Option<RepoPath>) -> Self {
        Self {
            context,
            location,
            fragment: GraphFragment::default(),
            facts: 0,
            limit_reached: false,
        }
    }

    fn push_group(&mut self, nodes: Vec<GraphNode>, edges: Vec<GraphEdge>) -> bool {
        let requested = (nodes.len() as u64).saturating_add(edges.len() as u64);
        if self.facts.saturating_add(requested) > self.context.max_facts_per_file {
            self.limit_reached = true;
            return false;
        }

        self.facts = self.facts.saturating_add(requested);
        self.fragment.nodes.extend(nodes);
        self.fragment.edges.extend(edges);
        true
    }

    fn push_diagnostic(&mut self, code: DiagnosticCode, evidence: Option<SourceEvidence>) {
        if self.fragment.diagnostics.len() as u64 >= self.context.max_diagnostics {
            return;
        }

        self.fragment.diagnostics.push(graph_diagnostic(
            self.context,
            code,
            evidence.map(|evidence| DiagnosticLocation {
                path: evidence.path,
                span: evidence.span,
            }),
        ));
    }

    fn finish(mut self) -> GraphFragment {
        if self.limit_reached {
            let location = self
                .location
                .take()
                .map(|path| DiagnosticLocation { path, span: None });
            if (self.fragment.diagnostics.len() as u64) < self.context.max_diagnostics {
                self.fragment.diagnostics.push(graph_diagnostic(
                    self.context,
                    const {
                        crate::repository_graph::domain::DiagnosticCode::from_static(
                            "generic.fact_limit_reached",
                        )
                    },
                    location,
                ));
            }
        }

        self.fragment
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Classification {
    kind: &'static str,
    property: &'static str,
    value: &'static str,
}

fn classifications(path: &str) -> Vec<Classification> {
    let file_name = file_name(path).to_ascii_lowercase();
    let extension = extension(&file_name).unwrap_or_default();
    let mut result = Vec::with_capacity(4);

    if let Some(format) = document_format(&file_name, extension) {
        result.push(Classification {
            kind: DOCUMENT_KIND,
            property: "format",
            value: format,
        });
    }

    if let Some(ecosystem) = manifest_ecosystem(&file_name) {
        result.push(Classification {
            kind: MANIFEST_KIND,
            property: "ecosystem",
            value: ecosystem,
        });
    }

    if let Some(format) = configuration_format(&file_name, extension) {
        result.push(Classification {
            kind: CONFIGURATION_KIND,
            property: "format",
            value: format,
        });
    }

    if let Some(role) = entry_point_role(&file_name) {
        result.push(Classification {
            kind: ENTRY_POINT_KIND,
            property: "role",
            value: role,
        });
    }

    result
}

fn document_format(file_name: &str, extension: &str) -> Option<&'static str> {
    match extension {
        "md" | "markdown" => Some("markdown"),
        "rst" => Some("restructured_text"),
        "adoc" | "asciidoc" => Some("asciidoc"),
        "txt" => Some("plain_text"),
        _ if file_name == "readme"
            || file_name == "changelog"
            || file_name == "contributing"
            || file_name == "license" =>
        {
            Some("plain_text")
        }
        _ => None,
    }
}

fn manifest_ecosystem(file_name: &str) -> Option<&'static str> {
    match file_name {
        "cargo.toml" | "cargo.lock" => Some("cargo"),
        "package.json"
        | "package-lock.json"
        | "npm-shrinkwrap.json"
        | "yarn.lock"
        | "pnpm-lock.yaml" => Some("javascript"),
        "pyproject.toml" | "requirements.txt" | "poetry.lock" | "pipfile" | "pipfile.lock" => {
            Some("python")
        }
        "go.mod" | "go.sum" => Some("go"),
        "pom.xml"
        | "build.gradle"
        | "build.gradle.kts"
        | "settings.gradle"
        | "settings.gradle.kts" => Some("jvm"),
        "gemfile" | "gemfile.lock" => Some("ruby"),
        "composer.json" | "composer.lock" => Some("php"),
        "mix.exs" | "mix.lock" => Some("elixir"),
        "deno.json" | "deno.jsonc" => Some("deno"),
        _ => None,
    }
}

fn configuration_format(file_name: &str, extension: &str) -> Option<&'static str> {
    match extension {
        "toml" => Some("toml"),
        "yaml" | "yml" => Some("yaml"),
        "json" | "jsonc" => Some("json"),
        "ini" => Some("ini"),
        "cfg" | "conf" => Some("configuration"),
        "properties" => Some("properties"),
        _ => match file_name {
            ".editorconfig" => Some("editorconfig"),
            ".gitattributes" | ".gitignore" => Some("git"),
            ".dockerignore" => Some("docker"),
            "dockerfile" => Some("dockerfile"),
            "makefile" => Some("makefile"),
            _ => None,
        },
    }
}

fn entry_point_role(file_name: &str) -> Option<&'static str> {
    match file_name {
        "main.rs" => Some("binary"),
        "lib.rs" => Some("library"),
        "main.py" | "__main__.py" => Some("application"),
        "main.go" | "main.c" | "main.cc" | "main.cpp" | "main.cxx" | "main.java" => {
            Some("application")
        }
        "index.js" | "index.mjs" | "index.cjs" | "index.ts" | "index.mts" | "index.cts"
        | "main.js" | "main.ts" => Some("application"),
        _ => None,
    }
}

fn graph_node(
    context: &ExtractionContext,
    identity: &ExtractorIdentity,
    id: crate::repository_graph::domain::NodeId,
    kind: &str,
    semantic_key: &str,
    properties: BTreeMap<String, GraphValue>,
    evidence: Option<SourceEvidence>,
) -> GraphNode {
    GraphNode {
        snapshot_id: context.snapshot_id.clone(),
        id,
        kind: kind.to_string(),
        semantic_key: SemanticKey::new(semantic_key.to_string()).ok(),
        provenance: provenance(identity, evidence),
        properties,
    }
}

#[allow(clippy::too_many_arguments)]
fn graph_edge(
    context: &ExtractionContext,
    identity: &ExtractorIdentity,
    kind: &str,
    source: crate::repository_graph::domain::NodeId,
    target: EdgeTarget,
    local_key: &str,
    evidence: Option<SourceEvidence>,
) -> GraphEdge {
    let id = deterministic_edge_id(identity, kind, &source, &target, local_key);
    GraphEdge {
        snapshot_id: context.snapshot_id.clone(),
        id,
        kind: kind.to_string(),
        source,
        target,
        provenance: provenance(identity, evidence),
        properties: BTreeMap::new(),
    }
}

fn provenance(identity: &ExtractorIdentity, evidence: Option<SourceEvidence>) -> FactProvenance {
    FactProvenance {
        extractor: identity.clone(),
        evidence,
        resolution: ResolutionState::Resolved,
        confidence: Confidence::Exact,
    }
}

fn graph_diagnostic(
    context: &ExtractionContext,
    code: DiagnosticCode,
    location: Option<DiagnosticLocation>,
) -> GraphDiagnostic {
    GraphDiagnostic {
        build_id: context.build_id.clone(),
        snapshot_id: Some(context.snapshot_id.clone()),
        severity: DiagnosticSeverity::Warning,
        code,
        location,
        metrics: BTreeMap::new(),
    }
}

fn extractor_identity() -> ExtractorIdentity {
    ExtractorIdentity {
        id: const { ExtractorId::from_static(EXTRACTOR_ID) },
        version: EXTRACTOR_VERSION.to_string(),
        contract_version: EXTRACTOR_CONTRACT_VERSION,
    }
}

fn whole_file_span(bytes: &[u8]) -> SourceSpan {
    let (line, column) = match std::str::from_utf8(bytes) {
        Ok(text) => {
            let mut line = 1_u32;
            let mut column = 1_u32;
            for byte in text.bytes() {
                if byte == b'\n' {
                    line = line.saturating_add(1);
                    column = 1;
                } else {
                    column = column.saturating_add(1);
                }
            }

            (Some(line), Some(column))
        }
        Err(_) => (None, None),
    };
    SourceSpan {
        start: SourcePosition {
            byte_offset: 0,
            line: line.map(|_| 1),
            column: column.map(|_| 1),
        },
        end: SourcePosition {
            byte_offset: bytes.len() as u64,
            line,
            column,
        },
    }
}

fn ancestor_directories(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(|(index, _)| &path[..index])
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn extension(path: &str) -> Option<&str> {
    let file_name = file_name(path);
    let (_, extension) = file_name.rsplit_once('.')?;
    (!extension.is_empty()).then_some(extension)
}

fn directory_key(path: &str) -> String {
    format!("{DIRECTORY_KIND}:{path}")
}

fn file_key(path: &str) -> String {
    format!("{FILE_KIND}:{path}")
}

fn saturating_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
#[path = "generic_tests.rs"]
mod tests;
