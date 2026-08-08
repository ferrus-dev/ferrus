//! Conservative, storage-independent cross-file graph resolution.
//!
//! Resolution consumes one complete extractor fragment and the exact immutable
//! source manifest from which it was produced. It performs no filesystem or
//! SQLite reads and deliberately leaves relationships unresolved when syntax
//! alone cannot prove a unique target.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

use thiserror::Error;

use super::{
    EXTRACTOR_CONTRACT_VERSION,
    domain::{
        Confidence, DiagnosticCode, DiagnosticLocation, DiagnosticSeverity, EdgeTarget,
        ExtractorId, ExtractorIdentity, FactProvenance, GraphDiagnostic, GraphEdge, GraphNode,
        GraphValue, NodeId, RepoPath, ResolutionState, SourceEvidence,
    },
    extractors::deterministic_edge_id,
    ports::{CrossFileResolutionInput, CrossFileResolver, GraphFragment, SourceFileDescriptor},
};

const RESOLVER_ID: &str = "builtin.rust-cargo-resolver";
const RESOLVER_VERSION: &str = "1.0.0";

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ResolutionError {
    #[error("the source manifest belongs to a different repository")]
    RepositoryMismatch,
    #[error("a graph fact belongs to a different snapshot")]
    SnapshotMismatch,
    #[error("a graph diagnostic belongs to a different build")]
    BuildMismatch,
    #[error("duplicate graph node id: {0}")]
    DuplicateNode(NodeId),
    #[error("duplicate graph edge id: {0}")]
    DuplicateEdge(super::domain::EdgeId),
    #[error("graph edge source is absent from the fragment: {0}")]
    MissingSource(NodeId),
    #[error("graph edge target is absent from the fragment: {0}")]
    MissingTarget(NodeId),
    #[error("fact evidence is absent from or inconsistent with the source manifest: {0}")]
    InvalidEvidence(RepoPath),
}

/// Stateless resolver suitable for both the local coordinator and a future
/// distributed worker.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConservativeResolver;

impl ConservativeResolver {
    pub fn new() -> Self {
        Self
    }
}

impl CrossFileResolver for ConservativeResolver {
    type Error = ResolutionError;

    fn identity(&self) -> ExtractorIdentity {
        resolver_identity()
    }

    fn resolve(&self, input: CrossFileResolutionInput<'_>) -> Result<GraphFragment, Self::Error> {
        validate_input(&input)?;

        let started = Instant::now();
        let duration = Duration::from_millis(input.budget.max_duration_ms);
        let indexes = Indexes::new(&input.fragment);
        let mut diagnostics = DiagnosticBuffer::new(&input);
        let mut plan = ResolutionPlan::new(input.budget.max_relationships, &input.fragment);

        let resolved_modules = resolve_module_declarations(
            &input,
            &indexes,
            &mut plan,
            &mut diagnostics,
            started,
            duration,
        );
        if expired(started, duration) {
            return Ok(timeout_fragment(input, diagnostics));
        }

        let module_graph = ModuleGraph::new(&input.fragment, &indexes, &resolved_modules);
        let cargo = resolve_cargo_membership(
            &input,
            &indexes,
            &module_graph,
            &mut plan,
            &mut diagnostics,
            started,
            duration,
        );
        if expired(started, duration) {
            return Ok(timeout_fragment(input, diagnostics));
        }

        let resolved_dependencies = resolve_cargo_dependencies(
            &input,
            &indexes,
            &cargo,
            &mut plan,
            &mut diagnostics,
            started,
            duration,
        );
        if expired(started, duration) {
            return Ok(timeout_fragment(input, diagnostics));
        }

        resolve_imports(
            &input,
            &indexes,
            &module_graph,
            &cargo,
            &resolved_dependencies,
            &mut plan,
            &mut diagnostics,
            started,
            duration,
        );
        if expired(started, duration) {
            return Ok(timeout_fragment(input, diagnostics));
        }

        Ok(apply_plan(input.fragment, plan, diagnostics))
    }
}

pub fn resolver_identity() -> ExtractorIdentity {
    ExtractorIdentity {
        id: ExtractorId::new(RESOLVER_ID).expect("built-in resolver ID is non-empty"),
        version: RESOLVER_VERSION.to_string(),
        contract_version: EXTRACTOR_CONTRACT_VERSION,
    }
}

fn validate_input(input: &CrossFileResolutionInput<'_>) -> Result<(), ResolutionError> {
    if input.manifest.revision.repository != input.context.repository {
        return Err(ResolutionError::RepositoryMismatch);
    }

    let manifest_files = input
        .manifest
        .files
        .iter()
        .map(|file| (file.path.clone(), file))
        .collect::<BTreeMap<_, _>>();
    let mut nodes = BTreeSet::new();
    for node in &input.fragment.nodes {
        if node.snapshot_id != input.context.snapshot_id {
            return Err(ResolutionError::SnapshotMismatch);
        }
        if !nodes.insert(node.id.clone()) {
            return Err(ResolutionError::DuplicateNode(node.id.clone()));
        }
        validate_evidence(node.provenance.evidence.as_ref(), &manifest_files)?;
    }

    let mut edges = BTreeSet::new();
    for edge in &input.fragment.edges {
        if edge.snapshot_id != input.context.snapshot_id {
            return Err(ResolutionError::SnapshotMismatch);
        }
        if !edges.insert(edge.id.clone()) {
            return Err(ResolutionError::DuplicateEdge(edge.id.clone()));
        }
        if !nodes.contains(&edge.source) {
            return Err(ResolutionError::MissingSource(edge.source.clone()));
        }
        if let EdgeTarget::Node(target) = &edge.target
            && !nodes.contains(target)
        {
            return Err(ResolutionError::MissingTarget(target.clone()));
        }
        validate_evidence(edge.provenance.evidence.as_ref(), &manifest_files)?;
    }
    for diagnostic in &input.fragment.diagnostics {
        if diagnostic.build_id != input.context.build_id {
            return Err(ResolutionError::BuildMismatch);
        }
        if diagnostic
            .snapshot_id
            .as_ref()
            .is_some_and(|snapshot| snapshot != &input.context.snapshot_id)
        {
            return Err(ResolutionError::SnapshotMismatch);
        }
    }
    Ok(())
}

fn validate_evidence(
    evidence: Option<&SourceEvidence>,
    files: &BTreeMap<RepoPath, &SourceFileDescriptor>,
) -> Result<(), ResolutionError> {
    let Some(evidence) = evidence else {
        return Ok(());
    };
    let Some(file) = files.get(&evidence.path) else {
        return Err(ResolutionError::InvalidEvidence(evidence.path.clone()));
    };
    let span_valid = evidence.span.as_ref().is_none_or(|span| {
        span.start.byte_offset <= span.end.byte_offset && span.end.byte_offset <= file.byte_len
    });
    if evidence.content_identity != file.content_identity || !span_valid {
        return Err(ResolutionError::InvalidEvidence(evidence.path.clone()));
    }
    Ok(())
}

struct Indexes<'a> {
    nodes: BTreeMap<NodeId, &'a GraphNode>,
    rust_roots_by_path: BTreeMap<String, Vec<NodeId>>,
    packages_by_manifest_candidate: BTreeMap<String, Vec<NodeId>>,
    entry_paths: BTreeMap<NodeId, String>,
    target_packages: BTreeMap<NodeId, BTreeSet<NodeId>>,
    target_entries: BTreeMap<NodeId, BTreeSet<NodeId>>,
    dependency_owners: BTreeMap<NodeId, BTreeSet<NodeId>>,
    path_override_evidence: BTreeSet<(String, u64, u64)>,
}

impl<'a> Indexes<'a> {
    fn new(fragment: &'a GraphFragment) -> Self {
        let nodes = fragment
            .nodes
            .iter()
            .map(|node| (node.id.clone(), node))
            .collect::<BTreeMap<_, _>>();
        let mut rust_roots_by_path: BTreeMap<String, Vec<NodeId>> = BTreeMap::new();
        let mut packages_by_manifest_candidate: BTreeMap<String, Vec<NodeId>> = BTreeMap::new();
        let mut entry_paths = BTreeMap::new();
        let mut path_override_evidence = BTreeSet::new();

        for node in &fragment.nodes {
            if is_file_module(node)
                && let Some(path) = evidence_path(node)
            {
                rust_roots_by_path
                    .entry(path.to_string())
                    .or_default()
                    .push(node.id.clone());
            }
            if node.kind == "cargo_package"
                && let Some(path) = string_property(node, "manifest_path")
            {
                packages_by_manifest_candidate
                    .entry(format!("cargo-package-path:{}", escape_component(path)))
                    .or_default()
                    .push(node.id.clone());
            }
            if node.kind == "entry_point"
                && string_property(node, "language") == Some("rust")
                && let Some(path) = string_property(node, "path")
            {
                entry_paths.insert(node.id.clone(), path.to_string());
            }
            if node.kind == "mod_declaration"
                && boolean_property(node, "path_override") == Some(true)
                && let Some(key) = evidence_key(node.provenance.evidence.as_ref())
            {
                path_override_evidence.insert(key);
            }
        }
        sort_values(&mut rust_roots_by_path);
        sort_values(&mut packages_by_manifest_candidate);

        let mut target_packages = BTreeMap::new();
        let mut target_entries = BTreeMap::new();
        let mut dependency_owners = BTreeMap::new();
        for edge in &fragment.edges {
            let EdgeTarget::Node(target) = &edge.target else {
                continue;
            };
            match edge.kind.as_str() {
                "declares_target" => insert_set(&mut target_packages, target, &edge.source),
                "has_entry_point" => insert_set(&mut target_entries, &edge.source, target),
                "declares_dependency" => insert_set(&mut dependency_owners, target, &edge.source),
                _ => {}
            }
        }

        Self {
            nodes,
            rust_roots_by_path,
            packages_by_manifest_candidate,
            entry_paths,
            target_packages,
            target_entries,
            dependency_owners,
            path_override_evidence,
        }
    }
}

mod pipeline;
use pipeline::*;

fn apply_plan(
    mut fragment: GraphFragment,
    plan: ResolutionPlan,
    mut diagnostics: DiagnosticBuffer,
) -> GraphFragment {
    for node in &mut fragment.nodes {
        if plan.resolved_nodes.contains(&node.id) {
            node.provenance.resolution = ResolutionState::Resolved;
            node.provenance.confidence = Confidence::High;
        }
    }
    fragment
        .edges
        .retain(|edge| !plan.replacements.contains_key(&edge.id));
    fragment.edges.extend(plan.replacements.into_values());
    fragment.edges.extend(plan.additions.into_values());
    if plan.truncated {
        diagnostics.push_without_location("resolution.relationship_limit");
    }
    fragment.diagnostics.extend(diagnostics.finish());
    sort_fragment(&mut fragment);
    fragment
}

fn timeout_fragment(
    input: CrossFileResolutionInput<'_>,
    mut diagnostics: DiagnosticBuffer,
) -> GraphFragment {
    diagnostics.replace_with("resolution.timeout");
    let mut fragment = input.fragment;
    fragment.diagnostics.extend(diagnostics.finish());
    sort_fragment(&mut fragment);
    fragment
}

fn sort_fragment(fragment: &mut GraphFragment) {
    fragment.nodes.sort_by(|left, right| left.id.cmp(&right.id));
    fragment.edges.sort_by(|left, right| left.id.cmp(&right.id));
    fragment.diagnostics.sort_by(|left, right| {
        left.code
            .cmp(&right.code)
            .then_with(|| diagnostic_path(left).cmp(&diagnostic_path(right)))
            .then_with(|| diagnostic_span(left).cmp(&diagnostic_span(right)))
    });
    fragment.diagnostics.dedup();
}

fn diagnostic_path(diagnostic: &GraphDiagnostic) -> Option<&str> {
    diagnostic
        .location
        .as_ref()
        .map(|location| location.path.as_str())
}

fn diagnostic_span(diagnostic: &GraphDiagnostic) -> Option<(u64, u64)> {
    diagnostic
        .location
        .as_ref()
        .and_then(|location| location.span.as_ref())
        .map(|span| (span.start.byte_offset, span.end.byte_offset))
}

struct DiagnosticBuffer {
    build_id: super::domain::BuildId,
    snapshot_id: super::domain::SnapshotId,
    limit: u64,
    diagnostics: Vec<GraphDiagnostic>,
    suppressed: u64,
}

impl DiagnosticBuffer {
    fn new(input: &CrossFileResolutionInput<'_>) -> Self {
        Self {
            build_id: input.context.build_id.clone(),
            snapshot_id: input.context.snapshot_id.clone(),
            limit: input.budget.max_diagnostics,
            diagnostics: Vec::new(),
            suppressed: 0,
        }
    }

    fn push(&mut self, code: &'static str, edge: &GraphEdge) {
        self.push_evidence(code, edge.provenance.evidence.as_ref());
    }

    fn push_evidence(&mut self, code: &'static str, evidence: Option<&SourceEvidence>) {
        let location = evidence.map(|evidence| DiagnosticLocation {
            path: evidence.path.clone(),
            span: evidence.span.clone(),
        });
        self.push_location(code, location);
    }

    fn push_without_location(&mut self, code: &'static str) {
        self.push_location(code, None);
    }

    fn push_location(&mut self, code: &'static str, location: Option<DiagnosticLocation>) {
        if self.diagnostics.len() as u64 >= self.limit {
            self.suppressed = self.suppressed.saturating_add(1);
            return;
        }
        self.diagnostics.push(self.diagnostic(code, location));
    }

    fn replace_with(&mut self, code: &'static str) {
        self.diagnostics.clear();
        self.suppressed = 0;
        self.push_without_location(code);
    }

    fn finish(mut self) -> Vec<GraphDiagnostic> {
        if self.suppressed > 0 && self.limit > 0 {
            let replaced = u64::from(!self.diagnostics.is_empty());
            let mut summary = self.diagnostic("resolution.diagnostics_truncated", None);
            summary.metrics.insert(
                DiagnosticCode::new("suppressed").expect("static metric is canonical"),
                i64::try_from(self.suppressed.saturating_add(replaced)).unwrap_or(i64::MAX),
            );
            if let Some(last) = self.diagnostics.last_mut() {
                *last = summary;
            } else {
                self.diagnostics.push(summary);
            }
        }
        self.diagnostics
    }

    fn diagnostic(
        &self,
        code: &'static str,
        location: Option<DiagnosticLocation>,
    ) -> GraphDiagnostic {
        GraphDiagnostic {
            build_id: self.build_id.clone(),
            snapshot_id: Some(self.snapshot_id.clone()),
            severity: DiagnosticSeverity::Warning,
            code: DiagnosticCode::new(code).expect("static resolver diagnostic is canonical"),
            location,
            metrics: BTreeMap::new(),
        }
    }
}

fn resolved_edge(original: &GraphEdge, target: EdgeTarget, confidence: Confidence) -> GraphEdge {
    let resolution = match target {
        EdgeTarget::Node(_) => ResolutionState::Resolved,
        EdgeTarget::External(_) => ResolutionState::External,
        EdgeTarget::Unresolved(_) => ResolutionState::Unresolved,
    };
    let id = deterministic_edge_id(
        &resolver_identity(),
        &original.kind,
        &original.source,
        &target,
        original.id.as_str(),
    );
    GraphEdge {
        snapshot_id: original.snapshot_id.clone(),
        id,
        kind: original.kind.clone(),
        source: original.source.clone(),
        target,
        provenance: FactProvenance {
            extractor: resolver_identity(),
            evidence: original.provenance.evidence.clone(),
            resolution,
            confidence,
        },
        properties: original.properties.clone(),
    }
}

#[allow(clippy::too_many_arguments)]
fn new_edge(
    input: &CrossFileResolutionInput<'_>,
    kind: &str,
    source: NodeId,
    target: EdgeTarget,
    local_key: &str,
    evidence: Option<SourceEvidence>,
    confidence: Confidence,
) -> GraphEdge {
    let id = deterministic_edge_id(&resolver_identity(), kind, &source, &target, local_key);
    GraphEdge {
        snapshot_id: input.context.snapshot_id.clone(),
        id,
        kind: kind.to_string(),
        source,
        target,
        provenance: FactProvenance {
            extractor: resolver_identity(),
            evidence,
            resolution: ResolutionState::Resolved,
            confidence,
        },
        properties: BTreeMap::new(),
    }
}

fn sorted_edges<'a>(fragment: &'a GraphFragment, kind: &str) -> Vec<&'a GraphEdge> {
    let mut edges = fragment
        .edges
        .iter()
        .filter(|edge| edge.kind == kind)
        .collect::<Vec<_>>();
    edges.sort_by(|left, right| left.id.cmp(&right.id));
    edges
}

fn string_property<'a>(node: &'a GraphNode, name: &str) -> Option<&'a str> {
    match node.properties.get(name) {
        Some(GraphValue::String(value)) => Some(value),
        _ => None,
    }
}

fn boolean_property(node: &GraphNode, name: &str) -> Option<bool> {
    match node.properties.get(name) {
        Some(GraphValue::Boolean(value)) => Some(*value),
        _ => None,
    }
}

fn string_list_property<'a>(node: &'a GraphNode, name: &str) -> &'a [String] {
    match node.properties.get(name) {
        Some(GraphValue::StringList(values)) => values,
        _ => &[],
    }
}

fn evidence_key(evidence: Option<&SourceEvidence>) -> Option<(String, u64, u64)> {
    let evidence = evidence?;
    let span = evidence.span.as_ref()?;
    Some((
        evidence.path.as_str().to_string(),
        span.start.byte_offset,
        span.end.byte_offset,
    ))
}

fn evidence_path(node: &GraphNode) -> Option<&str> {
    node.provenance
        .evidence
        .as_ref()
        .map(|evidence| evidence.path.as_str())
}

fn is_file_module(node: &GraphNode) -> bool {
    node.kind == "module" && string_property(node, "module_origin") == Some("file")
}

fn file_module_directory(path: &str) -> String {
    let (directory, file) = path.rsplit_once('/').map_or(("", path), |parts| parts);
    match file {
        "lib.rs" | "main.rs" | "mod.rs" => directory.to_string(),
        _ => path.strip_suffix(".rs").unwrap_or(path).to_string(),
    }
}

fn manifest_directory(path: &str) -> String {
    path.rsplit_once('/')
        .map_or_else(|| ".".to_string(), |(directory, _)| directory.to_string())
}

fn match_workspace_pattern(pattern: &str, path: &str) -> Option<bool> {
    if pattern.contains(['[', ']', '{', '}', '\\']) {
        return None;
    }
    if pattern == "." || path == "." {
        return Some(pattern == path);
    }
    let pattern = pattern.split('/').collect::<Vec<_>>();
    let path = path.split('/').collect::<Vec<_>>();
    Some(match_path_segments(&pattern, &path))
}

fn match_path_segments(pattern: &[&str], path: &[&str]) -> bool {
    let mut reachable = vec![vec![false; path.len() + 1]; pattern.len() + 1];
    reachable[0][0] = true;
    for pattern_index in 0..pattern.len() {
        for path_index in 0..=path.len() {
            if !reachable[pattern_index][path_index] {
                continue;
            }
            if pattern[pattern_index] == "**" {
                reachable[pattern_index + 1][path_index] = true;
                if path_index < path.len() {
                    reachable[pattern_index][path_index + 1] = true;
                }
            } else if path_index < path.len()
                && match_path_segment(pattern[pattern_index], path[path_index])
            {
                reachable[pattern_index + 1][path_index + 1] = true;
            }
        }
    }
    reachable[pattern.len()][path.len()]
}

fn match_path_segment(pattern: &str, value: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let value = value.chars().collect::<Vec<_>>();
    let mut reachable = vec![vec![false; value.len() + 1]; pattern.len() + 1];
    reachable[0][0] = true;
    for pattern_index in 0..pattern.len() {
        for value_index in 0..=value.len() {
            if !reachable[pattern_index][value_index] {
                continue;
            }
            match pattern[pattern_index] {
                '*' => {
                    reachable[pattern_index + 1][value_index] = true;
                    if value_index < value.len() {
                        reachable[pattern_index][value_index + 1] = true;
                    }
                }
                '?' if value_index < value.len() => {
                    reachable[pattern_index + 1][value_index + 1] = true;
                }
                expected if value.get(value_index) == Some(&expected) => {
                    reachable[pattern_index + 1][value_index + 1] = true;
                }
                _ => {}
            }
        }
    }
    reachable[pattern.len()][value.len()]
}

fn join_path(base: &str, child: &str) -> String {
    if base.is_empty() {
        child.to_string()
    } else {
        format!("{base}/{child}")
    }
}

fn normalize_identifier(value: &str) -> String {
    value.strip_prefix("r#").unwrap_or(value).to_string()
}

fn normalize_crate_name(value: &str) -> String {
    normalize_identifier(value).replace('-', "_")
}

fn escape_component(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._/".contains(&byte) {
            escaped.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(escaped, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    escaped
}

fn edge_target_key(target: &EdgeTarget) -> String {
    match target {
        EdgeTarget::Node(node) => format!("node:{}", node.as_str()),
        EdgeTarget::External(value) => format!("external:{value}"),
        EdgeTarget::Unresolved(value) => format!("unresolved:{value}"),
    }
}

fn insert_set(map: &mut BTreeMap<NodeId, BTreeSet<NodeId>>, key: &NodeId, value: &NodeId) {
    map.entry(key.clone()).or_default().insert(value.clone());
}

fn sort_values(map: &mut BTreeMap<String, Vec<NodeId>>) {
    for values in map.values_mut() {
        values.sort();
        values.dedup();
    }
}

fn expired(started: Instant, duration: Duration) -> bool {
    duration.is_zero() || started.elapsed() >= duration
}

#[cfg(test)]
#[path = "resolution_tests.rs"]
mod tests;
