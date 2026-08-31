//! Syntax-only Cargo manifest extraction.
//!
//! The extractor parses the verified `Cargo.toml` bytes with the bundled TOML
//! parser. It never invokes Cargo, rustc, build scripts, workspace hooks, or
//! any repository-provided code. Path and workspace dependencies deliberately
//! remain unresolved candidates for the cross-file resolver.

use std::{
    collections::{BTreeMap, BTreeSet},
    convert::Infallible,
    ffi::OsStr,
    io::{self, Read, Write},
    ops::Range,
    process::{Child, Command, ExitStatus, Stdio},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use super::{deterministic_edge_id, deterministic_node_id};
use crate::repository_graph::{
    EXTRACTOR_CONTRACT_VERSION,
    domain::{
        Confidence, DiagnosticCode, DiagnosticLocation, DiagnosticSeverity, EdgeId, EdgeTarget,
        ExtractorId, ExtractorIdentity, FactProvenance, GraphDiagnostic, GraphEdge, GraphNode,
        GraphValue, NodeId, ResolutionState, SemanticKey, SourceEvidence, SourcePosition,
        SourceSpan,
    },
    ports::{Extractor, FileExtractionInput, GraphFragment, SourceFileDescriptor},
};

const EXTRACTOR_ID: &str = "builtin.cargo-manifest";
const EXTRACTOR_VERSION: &str = "1.0.0";
const MAX_SEMANTIC_KEY_BYTES: usize = 16 * 1024;
const MAX_PROPERTY_STRING_BYTES: usize = 4 * 1024;
const MAX_PROPERTY_LIST_ITEMS: usize = 256;
const MAX_PROPERTY_LIST_BYTES: usize = 32 * 1024;
const PARSER_WORKER_ARGUMENT: &str = "__ferrus-cargo-parser";
const PARSER_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Stateless Cargo manifest extractor.
#[derive(Debug, Clone, Copy)]
pub struct CargoExtractor {
    parser_execution: ParserExecution,
}

#[derive(Debug, Clone, Copy, Default)]
enum ParserExecution {
    #[default]
    IsolatedProcess,
    InProcessSandbox,
}

impl Default for CargoExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl CargoExtractor {
    pub fn new() -> Self {
        Self {
            parser_execution: ParserExecution::IsolatedProcess,
        }
    }

    /// Uses the bundled parser without spawning a child process.
    ///
    /// This mode is for callers already running inside a hard CPU, memory, and
    /// wall-clock sandbox, such as the distributed worker. The caller must
    /// bound source bytes and enforce the outer deadline. Parser and fact
    /// budgets are still checked before and after parsing and during traversal.
    pub(crate) fn new_in_process_sandboxed() -> Self {
        Self {
            parser_execution: ParserExecution::InProcessSandbox,
        }
    }
}

mod parser;
pub use parser::run_parser_worker_if_requested;
use parser::*;

impl Extractor for CargoExtractor {
    type Error = Infallible;

    fn identity(&self) -> ExtractorIdentity {
        extractor_identity()
    }

    fn supports(&self, file: &SourceFileDescriptor) -> bool {
        file.path.as_str() == "Cargo.toml" || file.path.as_str().ends_with("/Cargo.toml")
    }

    fn extract(&self, input: FileExtractionInput<'_>) -> Result<GraphFragment, Self::Error> {
        if !self.supports(input.file) {
            return Ok(GraphFragment::default());
        }

        let mut diagnostics = DiagnosticBuffer::new(input);
        let source = match std::str::from_utf8(input.content) {
            Ok(source) => source,
            Err(_) => {
                diagnostics.push("cargo.invalid_utf8", None);
                return Ok(GraphFragment {
                    diagnostics: diagnostics.finish(),
                    ..GraphFragment::default()
                });
            }
        };
        let spans = SpanIndex::new(source);
        let budget = Duration::from_millis(input.context.max_parser_duration_ms);
        if budget.is_zero() {
            diagnostics.push("cargo.parser_timeout", None);
            return Ok(GraphFragment {
                diagnostics: diagnostics.finish(),
                ..GraphFragment::default()
            });
        }

        let started = Instant::now();
        let parsed = match self.parser_execution {
            ParserExecution::IsolatedProcess => {
                run_parser_with_deadline(started, budget, source.to_owned())
            }
            ParserExecution::InProcessSandbox => {
                run_parser_in_process_with_deadline(started, budget, source)
            }
        };
        let parsed = match parsed {
            ParserDeadline::Completed(ParserOutput::Parsed { manifest }) => manifest,
            ParserDeadline::Completed(ParserOutput::Malformed { span }) => {
                diagnostics.push(
                    "cargo.malformed_manifest",
                    span.map(ParserSpan::into_range)
                        .map(|range| spans.span(range)),
                );
                return Ok(GraphFragment {
                    diagnostics: diagnostics.finish(),
                    ..GraphFragment::default()
                });
            }
            ParserDeadline::TimedOut => {
                diagnostics.push("cargo.parser_timeout", None);
                return Ok(GraphFragment {
                    diagnostics: diagnostics.finish(),
                    ..GraphFragment::default()
                });
            }
            ParserDeadline::Unavailable => {
                diagnostics.push("cargo.parser_unavailable", None);
                return Ok(GraphFragment {
                    diagnostics: diagnostics.finish(),
                    ..GraphFragment::default()
                });
            }
        };
        let mut facts = FactBuffer::new(input, &mut diagnostics, started, budget);
        extract_manifest(&parsed, &spans, &mut facts);
        facts.finish();

        Ok(GraphFragment {
            nodes: facts.nodes,
            edges: facts.edges,
            diagnostics: diagnostics.finish(),
        })
    }
}

fn extract_manifest(manifest: &toml::Table, spans: &SpanIndex, facts: &mut FactBuffer<'_, '_>) {
    let workspace = extract_workspace(manifest, spans, facts);
    if !facts.active() {
        return;
    }
    let package = extract_package(manifest, spans, facts);

    if let (Some(workspace), Some(package)) = (&workspace, &package) {
        facts.edge(
            "contains",
            workspace,
            EdgeTarget::Node(package.clone()),
            "workspace-package",
            spans.header("workspace", 0),
            ResolutionState::Resolved,
            Confidence::Exact,
            BTreeMap::new(),
        );
    }

    if let Some(package) = &package {
        extract_targets(manifest, spans, facts, package);
        if !facts.active() {
            return;
        }
        extract_dependency_groups(manifest, spans, facts, package);
    }
    if facts.active()
        && let Some(workspace) = &workspace
    {
        extract_workspace_dependencies(manifest, spans, facts, workspace);
    }
}

fn extract_workspace(
    manifest: &toml::Table,
    spans: &SpanIndex,
    facts: &mut FactBuffer<'_, '_>,
) -> Option<NodeId> {
    let value = manifest.get("workspace")?;
    let span = spans.header("workspace", 0);
    let Some(workspace) = value.as_table() else {
        facts.diagnostic("cargo.invalid_workspace_table", span);
        return None;
    };

    let mut properties = BTreeMap::from([
        (
            "manifest_path".to_string(),
            GraphValue::String(facts.manifest_path().to_string()),
        ),
        (
            "virtual".to_string(),
            GraphValue::Boolean(!manifest.contains_key("package")),
        ),
    ]);
    insert_string(workspace, "resolver", &mut properties);
    for (key, property) in [
        ("members", "member_patterns"),
        ("default-members", "default_member_patterns"),
        ("exclude", "exclude_patterns"),
    ] {
        if !facts.active() {
            return None;
        }
        if let Some(patterns) = string_list(workspace.get(key)) {
            let (patterns, rejected) =
                normalize_workspace_patterns(facts.manifest_path(), patterns);
            if rejected {
                facts.diagnostic("cargo.workspace_path_outside_repository", span.clone());
            }
            properties.insert(property.to_string(), GraphValue::StringList(patterns));
        } else if workspace.contains_key(key) {
            facts.diagnostic("cargo.invalid_workspace_paths", span.clone());
        }
    }

    let key = semantic_key("workspace", &[facts.manifest_path()]);
    facts.node(
        "cargo_workspace",
        &key,
        span,
        ResolutionState::Resolved,
        Confidence::Exact,
        properties,
    )
}

fn extract_package(
    manifest: &toml::Table,
    spans: &SpanIndex,
    facts: &mut FactBuffer<'_, '_>,
) -> Option<NodeId> {
    let value = manifest.get("package")?;
    let span = spans.header("package", 0);
    let Some(package) = value.as_table() else {
        facts.diagnostic("cargo.invalid_package_table", span);
        return None;
    };
    let Some(name) = package.get("name").and_then(toml::Value::as_str) else {
        facts.diagnostic("cargo.package_name_missing", span);
        return None;
    };

    let mut properties = BTreeMap::from([
        ("name".to_string(), GraphValue::String(name.to_string())),
        (
            "manifest_path".to_string(),
            GraphValue::String(facts.manifest_path().to_string()),
        ),
    ]);
    for key in ["version", "edition", "rust-version"] {
        insert_string(package, key, &mut properties);
    }
    for key in [
        "publish",
        "autolib",
        "autobins",
        "autoexamples",
        "autotests",
        "autobenches",
    ] {
        insert_bool(package, key, &mut properties);
    }

    let key = semantic_key("package", &[facts.manifest_path(), name]);
    facts.node(
        "cargo_package",
        &key,
        span,
        ResolutionState::Resolved,
        Confidence::Exact,
        properties,
    )
}

fn extract_targets(
    manifest: &toml::Table,
    spans: &SpanIndex,
    facts: &mut FactBuffer<'_, '_>,
    package: &NodeId,
) {
    let package_table = manifest.get("package").and_then(toml::Value::as_table);
    let package_name = package_table
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
        .expect("package facts require a package name");

    let explicit_lib = if let Some(value) = manifest.get("lib") {
        if let Some(table) = value.as_table() {
            let name = table
                .get("name")
                .and_then(toml::Value::as_str)
                .map(ToString::to_string)
                .unwrap_or_else(|| package_name.replace('-', "_"));
            emit_target(
                facts,
                package,
                "lib",
                &name,
                package_name,
                table,
                spans.header("lib", 0),
                true,
            );
            true
        } else {
            facts.diagnostic("cargo.invalid_target_table", spans.header("lib", 0));
            false
        }
    } else {
        false
    };

    for (manifest_key, target_kind) in [
        ("bin", "bin"),
        ("example", "example"),
        ("test", "test"),
        ("bench", "bench"),
    ] {
        if !facts.active() {
            return;
        }
        let Some(value) = manifest.get(manifest_key) else {
            continue;
        };
        let Some(targets) = value.as_array() else {
            facts.diagnostic("cargo.invalid_target_array", spans.header(manifest_key, 0));
            continue;
        };
        for (index, target) in targets.iter().enumerate() {
            if !facts.active() {
                return;
            }
            let span = spans.header(manifest_key, index);
            let Some(table) = target.as_table() else {
                facts.diagnostic("cargo.invalid_target_table", span);
                continue;
            };
            let name = table
                .get("name")
                .and_then(toml::Value::as_str)
                .map(ToString::to_string)
                .or_else(|| {
                    table
                        .get("path")
                        .and_then(toml::Value::as_str)
                        .and_then(file_stem)
                });
            let Some(name) = name else {
                facts.diagnostic("cargo.target_name_missing", span);
                continue;
            };
            emit_target(
                facts,
                package,
                target_kind,
                &name,
                package_name,
                table,
                span,
                true,
            );
        }
    }

    // Cargo discovers these conventional roots from the package directory.
    // This file-local extractor cannot prove that the source files exist, so
    // it emits honest unresolved candidates for RG1.4 to resolve or discard.
    let autolib = package_table
        .and_then(|package| package.get("autolib"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(true);
    if autolib && !explicit_lib {
        emit_conventional_target(facts, package, "lib", &package_name.replace('-', "_"));
    }
    extract_build_target(package_table, spans.header("package", 0), facts, package);
    let autobins = package_table
        .and_then(|package| package.get("autobins"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(true);
    let has_package_bin = target_tables(manifest, "bin").any(|target| {
        target
            .get("name")
            .and_then(toml::Value::as_str)
            .is_some_and(|name| name == package_name)
    });
    if autobins && !has_package_bin {
        emit_conventional_target(facts, package, "bin", package_name);
    }
}

fn extract_build_target(
    package: Option<&toml::Table>,
    span: Option<SourceSpan>,
    facts: &mut FactBuffer<'_, '_>,
    package_id: &NodeId,
) {
    let declaration = package.and_then(|package| package.get("build"));
    if declaration.is_some_and(|value| value.as_bool() == Some(false)) {
        return;
    }
    let (path, explicit) = match declaration {
        None => ("build.rs".to_string(), false),
        Some(value) => match value.as_str() {
            Some(path) => (path.to_string(), true),
            None => {
                facts.diagnostic("cargo.invalid_build_target", span);
                return;
            }
        },
    };
    emit_target_with_path(
        facts,
        package_id,
        "custom_build",
        "build-script-build",
        &toml::Table::new(),
        span,
        explicit,
        Some(path),
    );
}

fn target_tables<'a>(
    manifest: &'a toml::Table,
    key: &str,
) -> impl Iterator<Item = &'a toml::Table> {
    manifest
        .get(key)
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_table)
}

fn emit_conventional_target(
    facts: &mut FactBuffer<'_, '_>,
    package: &NodeId,
    target_kind: &str,
    name: &str,
) {
    let path = match target_kind {
        "lib" => "src/lib.rs".to_string(),
        "bin" => "src/main.rs".to_string(),
        _ => return,
    };
    let table = toml::Table::new();
    emit_target_with_path(
        facts,
        package,
        target_kind,
        name,
        &table,
        None,
        false,
        Some(path),
    );
}

#[allow(clippy::too_many_arguments)]
fn emit_target(
    facts: &mut FactBuffer<'_, '_>,
    package: &NodeId,
    target_kind: &str,
    name: &str,
    package_name: &str,
    table: &toml::Table,
    span: Option<SourceSpan>,
    explicit: bool,
) {
    let declared_path = table
        .get("path")
        .and_then(toml::Value::as_str)
        .map(ToString::to_string)
        .or_else(|| conventional_path(target_kind, name, package_name));
    emit_target_with_path(
        facts,
        package,
        target_kind,
        name,
        table,
        span,
        explicit,
        declared_path,
    );
}

#[allow(clippy::too_many_arguments)]
fn emit_target_with_path(
    facts: &mut FactBuffer<'_, '_>,
    package: &NodeId,
    target_kind: &str,
    name: &str,
    table: &toml::Table,
    span: Option<SourceSpan>,
    explicit: bool,
    declared_path: Option<String>,
) {
    let key = semantic_key("target", &[facts.manifest_path(), target_kind, name]);
    let mut properties = BTreeMap::from([
        ("name".to_string(), GraphValue::String(name.to_string())),
        (
            "target_kind".to_string(),
            GraphValue::String(target_kind.to_string()),
        ),
        ("explicit".to_string(), GraphValue::Boolean(explicit)),
        (
            "origin".to_string(),
            GraphValue::String(if explicit {
                "manifest".to_string()
            } else {
                "conventional_candidate".to_string()
            }),
        ),
    ]);
    for table_key in ["crate-type", "required-features"] {
        if let Some(values) = string_list(table.get(table_key)) {
            properties.insert(table_key.replace('-', "_"), GraphValue::StringList(values));
        }
    }
    for table_key in ["test", "bench", "doc", "doctest", "harness", "proc-macro"] {
        insert_bool(table, table_key, &mut properties);
    }

    let resolution = if explicit {
        ResolutionState::Resolved
    } else {
        ResolutionState::Unresolved
    };
    let confidence = if explicit {
        Confidence::Exact
    } else {
        Confidence::Low
    };
    let Some(target) = facts.node(
        "cargo_target",
        &key,
        span.clone(),
        resolution,
        confidence,
        properties,
    ) else {
        return;
    };
    facts.edge(
        "declares_target",
        package,
        EdgeTarget::Node(target.clone()),
        &key,
        span.clone(),
        ResolutionState::Resolved,
        Confidence::Exact,
        BTreeMap::new(),
    );

    let Some(path) = declared_path else {
        facts.edge(
            "has_entry_point",
            &target,
            EdgeTarget::Unresolved(format!("cargo-entry:{key}")),
            &key,
            span,
            ResolutionState::Unresolved,
            Confidence::Low,
            BTreeMap::new(),
        );
        return;
    };
    let Some(path) = normalize_from_manifest(facts.manifest_path(), &path) else {
        facts.diagnostic("cargo.target_path_outside_repository", span.clone());
        facts.edge(
            "has_entry_point",
            &target,
            EdgeTarget::Unresolved(format!("cargo-entry:{key}")),
            &key,
            span,
            ResolutionState::Unresolved,
            Confidence::Low,
            BTreeMap::new(),
        );
        return;
    };
    let entry_key = semantic_key("entry-point", &[&path]);
    let Some(entry) = facts.node(
        "entry_point",
        &entry_key,
        span.clone(),
        ResolutionState::Unresolved,
        if explicit {
            Confidence::High
        } else {
            Confidence::Low
        },
        BTreeMap::from([
            ("path".to_string(), GraphValue::String(path)),
            (
                "language".to_string(),
                GraphValue::String("rust".to_string()),
            ),
        ]),
    ) else {
        return;
    };
    facts.edge(
        "has_entry_point",
        &target,
        EdgeTarget::Node(entry),
        &entry_key,
        span,
        ResolutionState::Unresolved,
        if explicit {
            Confidence::High
        } else {
            Confidence::Low
        },
        BTreeMap::new(),
    );
}

fn conventional_path(target_kind: &str, name: &str, package_name: &str) -> Option<String> {
    match target_kind {
        "lib" => Some("src/lib.rs".to_string()),
        "bin" if name == package_name => Some("src/main.rs".to_string()),
        "bin" => Some(format!("src/bin/{name}.rs")),
        "example" => Some(format!("examples/{name}.rs")),
        "test" => Some(format!("tests/{name}.rs")),
        "bench" => Some(format!("benches/{name}.rs")),
        _ => None,
    }
}

mod dependencies;
use dependencies::*;

struct FactBuffer<'input, 'diagnostics> {
    input: FileExtractionInput<'input>,
    diagnostics: &'diagnostics mut DiagnosticBuffer<'input>,
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    node_ids: BTreeSet<NodeId>,
    edge_ids: BTreeSet<EdgeId>,
    used: u64,
    truncated: bool,
    started: Instant,
    budget: Duration,
    timed_out: bool,
    source_limit_reported: bool,
}

impl<'input, 'diagnostics> FactBuffer<'input, 'diagnostics> {
    fn new(
        input: FileExtractionInput<'input>,
        diagnostics: &'diagnostics mut DiagnosticBuffer<'input>,
        started: Instant,
        budget: Duration,
    ) -> Self {
        Self {
            input,
            diagnostics,
            nodes: Vec::new(),
            edges: Vec::new(),
            node_ids: BTreeSet::new(),
            edge_ids: BTreeSet::new(),
            used: 0,
            truncated: false,
            started,
            budget,
            timed_out: false,
            source_limit_reported: false,
        }
    }

    fn manifest_path(&self) -> &str {
        self.input.file.path.as_str()
    }

    #[allow(clippy::too_many_arguments)]
    fn node(
        &mut self,
        kind: &str,
        semantic_key: &str,
        span: Option<SourceSpan>,
        resolution: ResolutionState,
        confidence: Confidence,
        properties: BTreeMap<String, GraphValue>,
    ) -> Option<NodeId> {
        if !self.active() {
            return None;
        }
        if semantic_key.len() > MAX_SEMANTIC_KEY_BYTES {
            self.source_limit();
            return None;
        }
        let properties = self.bounded_properties(properties);
        let identity = extractor_identity();
        let id = deterministic_node_id(&identity, kind, semantic_key);
        if self.node_ids.contains(&id) {
            return Some(id);
        }
        if !self.reserve() {
            return None;
        }
        self.node_ids.insert(id.clone());
        self.nodes.push(GraphNode {
            snapshot_id: self.input.context.snapshot_id.clone(),
            id: id.clone(),
            kind: kind.to_string(),
            semantic_key: Some(
                SemanticKey::new(semantic_key.to_string())
                    .expect("Cargo semantic keys are always non-empty"),
            ),
            provenance: provenance(self.input, span, resolution, confidence),
            properties,
        });
        Some(id)
    }

    #[allow(clippy::too_many_arguments)]
    fn edge(
        &mut self,
        kind: &str,
        source: &NodeId,
        target: EdgeTarget,
        local_key: &str,
        span: Option<SourceSpan>,
        resolution: ResolutionState,
        confidence: Confidence,
        properties: BTreeMap<String, GraphValue>,
    ) {
        if !self.active()
            || local_key.len() > MAX_SEMANTIC_KEY_BYTES
            || edge_target_len(&target) > MAX_SEMANTIC_KEY_BYTES
        {
            if local_key.len() > MAX_SEMANTIC_KEY_BYTES
                || edge_target_len(&target) > MAX_SEMANTIC_KEY_BYTES
            {
                self.source_limit();
            }
            return;
        }
        let properties = self.bounded_properties(properties);
        let identity = extractor_identity();
        let id = deterministic_edge_id(&identity, kind, source, &target, local_key);
        if self.edge_ids.contains(&id) {
            return;
        }
        if !self.reserve() {
            return;
        }
        self.edge_ids.insert(id.clone());
        self.edges.push(GraphEdge {
            snapshot_id: self.input.context.snapshot_id.clone(),
            id,
            kind: kind.to_string(),
            source: source.clone(),
            target,
            provenance: provenance(self.input, span, resolution, confidence),
            properties,
        });
    }

    fn reserve(&mut self) -> bool {
        if !self.active() {
            return false;
        }
        if self.used >= self.input.context.max_facts_per_file {
            self.truncated = true;
            false
        } else {
            self.used += 1;
            true
        }
    }

    fn diagnostic(&mut self, code: &'static str, span: Option<SourceSpan>) {
        self.diagnostics.push(code, span);
    }

    fn finish(&mut self) {
        self.active();
        if self.timed_out {
            self.nodes.clear();
            self.edges.clear();
            self.node_ids.clear();
            self.edge_ids.clear();
            self.diagnostics.replace_with("cargo.parser_timeout");
        } else if self.truncated {
            self.diagnostics.push("cargo.fact_limit", None);
        }
        self.nodes.sort_by(|left, right| left.id.cmp(&right.id));
        self.edges.sort_by(|left, right| left.id.cmp(&right.id));
    }

    fn active(&mut self) -> bool {
        if !self.timed_out && self.started.elapsed() >= self.budget {
            self.timed_out = true;
        }
        !self.timed_out
    }

    fn source_limit(&mut self) {
        if !self.source_limit_reported {
            self.source_limit_reported = true;
            self.diagnostics.push("cargo.source_value_limit", None);
        }
    }

    fn bounded_properties(
        &mut self,
        mut properties: BTreeMap<String, GraphValue>,
    ) -> BTreeMap<String, GraphValue> {
        let before = properties.len();
        properties.retain(|_, value| match value {
            GraphValue::String(value) => value.len() <= MAX_PROPERTY_STRING_BYTES,
            GraphValue::StringList(values) => {
                values.len() <= MAX_PROPERTY_LIST_ITEMS
                    && values
                        .iter()
                        .all(|value| value.len() <= MAX_PROPERTY_STRING_BYTES)
                    && values
                        .iter()
                        .try_fold(0usize, |total, value| total.checked_add(value.len()))
                        .is_some_and(|total| total <= MAX_PROPERTY_LIST_BYTES)
            }
            GraphValue::Boolean(_) | GraphValue::Integer(_) | GraphValue::Float(_) => true,
        });
        if properties.len() != before {
            self.source_limit();
        }
        properties
    }
}

fn edge_target_len(target: &EdgeTarget) -> usize {
    match target {
        EdgeTarget::Node(node) => node.as_str().len(),
        EdgeTarget::External(target) | EdgeTarget::Unresolved(target) => target.len(),
    }
}

fn provenance(
    input: FileExtractionInput<'_>,
    span: Option<SourceSpan>,
    resolution: ResolutionState,
    confidence: Confidence,
) -> FactProvenance {
    FactProvenance {
        extractor: extractor_identity(),
        evidence: Some(SourceEvidence {
            path: input.file.path.clone(),
            content_identity: input.file.content_identity.clone(),
            span,
        }),
        resolution,
        confidence,
    }
}

struct DiagnosticBuffer<'a> {
    input: FileExtractionInput<'a>,
    diagnostics: Vec<GraphDiagnostic>,
    suppressed: u64,
}

impl<'a> DiagnosticBuffer<'a> {
    fn new(input: FileExtractionInput<'a>) -> Self {
        Self {
            input,
            diagnostics: Vec::new(),
            suppressed: 0,
        }
    }

    fn push(&mut self, code: &'static str, span: Option<SourceSpan>) {
        if self.diagnostics.len() as u64 >= self.input.context.max_diagnostics {
            self.suppressed = self.suppressed.saturating_add(1);
            return;
        }
        self.diagnostics.push(self.diagnostic(code, span));
    }

    fn replace_with(&mut self, code: &'static str) {
        self.diagnostics.clear();
        self.suppressed = 0;
        self.push(code, None);
    }

    fn finish(mut self) -> Vec<GraphDiagnostic> {
        if self.suppressed > 0 && self.input.context.max_diagnostics > 0 {
            let replaced = u64::from(!self.diagnostics.is_empty());
            let suppressed = self.suppressed.saturating_add(replaced);
            let mut summary = self.diagnostic("cargo.diagnostics_truncated", None);
            summary.metrics.insert(
                DiagnosticCode::new("suppressed").expect("static diagnostic metric is valid"),
                i64::try_from(suppressed).unwrap_or(i64::MAX),
            );
            if let Some(last) = self.diagnostics.last_mut() {
                *last = summary;
            } else {
                self.diagnostics.push(summary);
            }
        }
        self.diagnostics
    }

    fn diagnostic(&self, code: &'static str, span: Option<SourceSpan>) -> GraphDiagnostic {
        GraphDiagnostic {
            build_id: self.input.context.build_id.clone(),
            snapshot_id: Some(self.input.context.snapshot_id.clone()),
            severity: DiagnosticSeverity::Warning,
            code: DiagnosticCode::new(code).expect("static Cargo diagnostic code is valid"),
            location: Some(DiagnosticLocation {
                path: self.input.file.path.clone(),
                span,
            }),
            metrics: BTreeMap::new(),
        }
    }
}

struct SpanIndex<'a> {
    source: &'a str,
    lines: Vec<(usize, usize, &'a str)>,
}

impl<'a> SpanIndex<'a> {
    fn new(source: &'a str) -> Self {
        let mut lines = Vec::new();
        let mut start = 0;
        for line in source.split_inclusive('\n') {
            let content_end = start + line.trim_end_matches(['\r', '\n']).len();
            lines.push((start, content_end, &source[start..content_end]));
            start += line.len();
        }
        if source.is_empty() || start < source.len() {
            lines.push((start, source.len(), &source[start..]));
        }
        Self { source, lines }
    }

    fn header(&self, name: &str, occurrence: usize) -> Option<SourceSpan> {
        let table = format!("[{name}]");
        let array = format!("[[{name}]]");
        self.lines
            .iter()
            .filter(|(_, _, line)| {
                let line = line.trim();
                line == table || line == array
            })
            .nth(occurrence)
            .map(|(start, end, line)| {
                let leading = line.len() - line.trim_start().len();
                self.span(start + leading..*end)
            })
    }

    fn target_dependency_header(&self, condition: &str, group: &str) -> Option<SourceSpan> {
        self.lines
            .iter()
            .find(|(_, _, line)| {
                let line = line.trim();
                line.starts_with("[target.")
                    && line.ends_with(&format!(".{group}]"))
                    && line.contains(condition)
            })
            .map(|(start, end, line)| {
                let leading = line.len() - line.trim_start().len();
                self.span(start + leading..*end)
            })
    }

    fn span(&self, range: Range<usize>) -> SourceSpan {
        let start = range.start.min(self.source.len());
        let end = range.end.min(self.source.len()).max(start);
        SourceSpan {
            start: self.position(start),
            end: self.position(end),
        }
    }

    fn position(&self, offset: usize) -> SourcePosition {
        let line_index = self
            .lines
            .partition_point(|(_, end, _)| *end < offset)
            .min(self.lines.len().saturating_sub(1));
        let line_start = self.lines.get(line_index).map_or(0, |(start, _, _)| *start);
        SourcePosition {
            byte_offset: offset as u64,
            line: Some(line_index.saturating_add(1) as u32),
            column: Some(offset.saturating_sub(line_start).saturating_add(1) as u32),
        }
    }
}

#[cfg(test)]
#[path = "cargo_tests.rs"]
mod tests;
