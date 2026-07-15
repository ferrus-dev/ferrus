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

fn resolve_cargo_dependencies(
    input: &CrossFileResolutionInput<'_>,
    indexes: &Indexes<'_>,
    cargo: &CargoResolution,
    plan: &mut ResolutionPlan,
    diagnostics: &mut DiagnosticBuffer,
    started: Instant,
    duration: Duration,
) -> ResolvedDependencies {
    let edges = sorted_edges(&input.fragment, "depends_on");
    let mut known_targets = BTreeMap::new();
    let mut resolved = ResolvedDependencies::default();

    // Resolve direct path candidates first; workspace inheritance below can
    // then reuse the exact target declared by its owning workspace.
    for edge in &edges {
        if expired(started, duration) {
            break;
        }
        match &edge.target {
            EdgeTarget::Node(package) => {
                known_targets.insert(edge.source.clone(), edge.target.clone());
                resolved
                    .internal
                    .insert(edge.source.clone(), package.clone());
            }
            EdgeTarget::External(_) => {
                known_targets.insert(edge.source.clone(), edge.target.clone());
                resolved.external.insert(edge.source.clone());
            }
            EdgeTarget::Unresolved(candidate) if candidate.starts_with("cargo-package-path:") => {
                let matches = indexes
                    .packages_by_manifest_candidate
                    .get(candidate)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                match matches {
                    [package] => {
                        let target = EdgeTarget::Node(package.clone());
                        let replacement = resolved_edge(edge, target.clone(), Confidence::Exact);
                        if plan.replace(edge, replacement) {
                            known_targets.insert(edge.source.clone(), target);
                            resolved
                                .internal
                                .insert(edge.source.clone(), package.clone());
                        }
                    }
                    [] => diagnostics.push("resolution.dependency_missing", edge),
                    _ => diagnostics.push("resolution.dependency_ambiguous", edge),
                }
            }
            _ => {}
        }
    }

    for edge in edges {
        if expired(started, duration) {
            break;
        }
        let EdgeTarget::Unresolved(candidate) = &edge.target else {
            continue;
        };
        if !candidate.starts_with("cargo-workspace-dependency:") {
            continue;
        }
        let alias = indexes
            .nodes
            .get(&edge.source)
            .and_then(|node| string_property(node, "alias"))
            .map(normalize_identifier);
        let Some(alias) = alias else {
            diagnostics.push("resolution.dependency_missing", edge);
            continue;
        };
        let workspaces = indexes
            .dependency_owners
            .get(&edge.source)
            .into_iter()
            .flatten()
            .filter_map(|package| cargo.package_workspaces.get(package))
            .flatten()
            .cloned()
            .collect::<BTreeSet<_>>();
        if workspaces.len() != 1 {
            diagnostics.push(
                if workspaces.is_empty() {
                    "resolution.dependency_missing"
                } else {
                    "resolution.dependency_ambiguous"
                },
                edge,
            );
            continue;
        }
        let workspace = workspaces.iter().next().expect("one workspace exists");
        let mut inherited = indexes
            .dependency_owners
            .iter()
            .filter(|(_, owners)| owners.contains(workspace))
            .filter_map(|(dependency, _)| {
                let node = indexes.nodes.get(dependency)?;
                (string_property(node, "scope") == Some("workspace")
                    && string_property(node, "alias").map(normalize_identifier)
                        == Some(alias.clone()))
                .then(|| known_targets.get(dependency))
                .flatten()
            })
            .cloned()
            .collect::<Vec<_>>();
        inherited.sort_by_key(edge_target_key);
        inherited.dedup();
        match inherited.len() {
            1 => {
                let target = inherited.into_iter().next().expect("one target exists");
                if plan.replace(edge, resolved_edge(edge, target.clone(), Confidence::Exact)) {
                    known_targets.insert(edge.source.clone(), target.clone());
                    if let EdgeTarget::Node(package) = target {
                        resolved.internal.insert(edge.source.clone(), package);
                    } else if matches!(target, EdgeTarget::External(_)) {
                        resolved.external.insert(edge.source.clone());
                    }
                }
            }
            0 => diagnostics.push("resolution.dependency_missing", edge),
            _ => diagnostics.push("resolution.dependency_ambiguous", edge),
        }
    }
    resolved
}

#[derive(Default)]
struct ResolvedDependencies {
    internal: BTreeMap<NodeId, NodeId>,
    external: BTreeSet<NodeId>,
}

fn resolve_module_declarations(
    input: &CrossFileResolutionInput<'_>,
    indexes: &Indexes<'_>,
    plan: &mut ResolutionPlan,
    diagnostics: &mut DiagnosticBuffer,
    started: Instant,
    duration: Duration,
) -> BTreeMap<super::domain::EdgeId, NodeId> {
    let manifest_paths = input
        .manifest
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<BTreeSet<_>>();
    let containment_parents = containment_module_parents(&input.fragment, indexes);
    let mut resolved = BTreeMap::new();

    for edge in sorted_edges(&input.fragment, "declares_module") {
        if expired(started, duration) {
            break;
        }
        if let EdgeTarget::Node(target) = &edge.target {
            resolved.insert(edge.id.clone(), target.clone());
            continue;
        }
        let EdgeTarget::Unresolved(name) = &edge.target else {
            continue;
        };
        if evidence_key(edge.provenance.evidence.as_ref())
            .is_some_and(|key| indexes.path_override_evidence.contains(&key))
        {
            diagnostics.push("resolution.module_path_override", edge);
            continue;
        }
        let Some(base) = module_base(&edge.source, indexes, &containment_parents) else {
            diagnostics.push("resolution.module_scope_ambiguous", edge);
            continue;
        };
        let name = normalize_identifier(name);
        if name.is_empty() || name.contains('/') || name.contains("::") {
            diagnostics.push("resolution.module_target_unsupported", edge);
            continue;
        }
        let candidates = [
            join_path(&base, &format!("{name}.rs")),
            join_path(&base, &format!("{name}/mod.rs")),
        ];
        let matches = candidates
            .iter()
            .filter(|candidate| manifest_paths.contains(candidate.as_str()))
            .filter_map(|candidate| indexes.rust_roots_by_path.get(candidate))
            .flatten()
            .cloned()
            .collect::<BTreeSet<_>>();
        match matches.len() {
            1 => {
                let target = matches.into_iter().next().expect("one match exists");
                let replacement =
                    resolved_edge(edge, EdgeTarget::Node(target.clone()), Confidence::High);
                if plan.replace(edge, replacement) {
                    resolved.insert(edge.id.clone(), target);
                }
            }
            0 => diagnostics.push("resolution.module_missing", edge),
            _ => diagnostics.push("resolution.module_ambiguous", edge),
        }
    }
    resolved
}

fn containment_module_parents(
    fragment: &GraphFragment,
    indexes: &Indexes<'_>,
) -> BTreeMap<NodeId, BTreeSet<NodeId>> {
    let mut parents = BTreeMap::new();
    for edge in &fragment.edges {
        if edge.kind != "contains" {
            continue;
        }
        let EdgeTarget::Node(target) = &edge.target else {
            continue;
        };
        if indexes
            .nodes
            .get(target)
            .is_some_and(|node| node.kind == "module")
        {
            insert_set(&mut parents, target, &edge.source);
        }
    }
    parents
}

fn module_base(
    source: &NodeId,
    indexes: &Indexes<'_>,
    parents: &BTreeMap<NodeId, BTreeSet<NodeId>>,
) -> Option<String> {
    let mut current = source.clone();
    let mut inline: Vec<String> = Vec::new();
    let mut seen = BTreeSet::new();
    loop {
        if !seen.insert(current.clone()) {
            return None;
        }
        let node = indexes.nodes.get(&current)?;
        if is_file_module(node) {
            let path = evidence_path(node)?;
            let mut base = file_module_directory(path);
            for segment in inline.iter().rev() {
                base = join_path(&base, segment);
            }
            return Some(base);
        }
        if node.kind != "module" || string_property(node, "module_origin") != Some("inline") {
            return None;
        }
        inline.push(normalize_identifier(string_property(node, "name")?));
        let parent = parents.get(&current)?;
        if parent.len() != 1 {
            return None;
        }
        current = parent.iter().next()?.clone();
    }
}

struct ModuleGraph {
    children: BTreeMap<(NodeId, String), BTreeSet<NodeId>>,
    parents: BTreeMap<NodeId, BTreeSet<NodeId>>,
}

impl ModuleGraph {
    fn new(
        fragment: &GraphFragment,
        indexes: &Indexes<'_>,
        resolved_modules: &BTreeMap<super::domain::EdgeId, NodeId>,
    ) -> Self {
        let mut children = BTreeMap::new();
        let mut parents = BTreeMap::new();
        for edge in &fragment.edges {
            let target = if edge.kind == "declares_module" {
                resolved_modules.get(&edge.id).cloned().or_else(|| {
                    if let EdgeTarget::Node(target) = &edge.target {
                        Some(target.clone())
                    } else {
                        None
                    }
                })
            } else if edge.kind == "contains" {
                if let EdgeTarget::Node(target) = &edge.target {
                    Some(target.clone())
                } else {
                    None
                }
            } else {
                None
            };
            let Some(target) = target else {
                continue;
            };
            let Some(node) = indexes.nodes.get(&target) else {
                continue;
            };
            if node.kind == "mod_declaration" {
                continue;
            }
            if let Some(name) = string_property(node, "name") {
                children
                    .entry((edge.source.clone(), normalize_identifier(name)))
                    .or_insert_with(BTreeSet::new)
                    .insert(target.clone());
            }
            if node.kind == "module" {
                insert_set(&mut parents, &target, &edge.source);
            }
        }
        Self { children, parents }
    }

    fn child(&self, parent: &NodeId, name: &str) -> ChildLookup {
        match self
            .children
            .get(&(parent.clone(), normalize_identifier(name)))
        {
            Some(matches) if matches.len() == 1 => {
                ChildLookup::Unique(matches.iter().next().expect("one child exists").clone())
            }
            Some(matches) if !matches.is_empty() => ChildLookup::Ambiguous,
            _ => ChildLookup::Missing,
        }
    }

    fn parent(&self, child: &NodeId) -> ChildLookup {
        match self.parents.get(child) {
            Some(matches) if matches.len() == 1 => {
                ChildLookup::Unique(matches.iter().next().expect("one parent exists").clone())
            }
            Some(matches) if !matches.is_empty() => ChildLookup::Ambiguous,
            _ => ChildLookup::Missing,
        }
    }

    fn root(&self, node: &NodeId) -> ChildLookup {
        let mut current = node.clone();
        let mut seen = BTreeSet::new();
        loop {
            if !seen.insert(current.clone()) {
                return ChildLookup::Ambiguous;
            }
            match self.parent(&current) {
                ChildLookup::Unique(parent) => current = parent,
                ChildLookup::Missing => return ChildLookup::Unique(current),
                ChildLookup::Ambiguous => return ChildLookup::Ambiguous,
            }
        }
    }
}

enum ChildLookup {
    Unique(NodeId),
    Missing,
    Ambiguous,
}

#[derive(Default)]
struct CargoResolution {
    module_packages: BTreeMap<NodeId, BTreeSet<NodeId>>,
    package_lib_roots: BTreeMap<NodeId, BTreeSet<NodeId>>,
    package_workspaces: BTreeMap<NodeId, BTreeSet<NodeId>>,
}

#[allow(clippy::too_many_arguments)]
fn resolve_cargo_membership(
    input: &CrossFileResolutionInput<'_>,
    indexes: &Indexes<'_>,
    module_graph: &ModuleGraph,
    plan: &mut ResolutionPlan,
    diagnostics: &mut DiagnosticBuffer,
    started: Instant,
    duration: Duration,
) -> CargoResolution {
    let mut result = CargoResolution::default();
    let mut entry_roots = BTreeMap::new();

    resolve_workspace_membership(
        input,
        indexes,
        plan,
        diagnostics,
        &mut result,
        started,
        duration,
    );

    for (target, entries) in &indexes.target_entries {
        if expired(started, duration) {
            break;
        }
        for entry in entries {
            let Some(path) = indexes.entry_paths.get(entry) else {
                continue;
            };
            let Some(roots) = indexes.rust_roots_by_path.get(path) else {
                continue;
            };
            if roots.len() != 1 {
                continue;
            }
            let root = roots[0].clone();
            entry_roots.insert(entry.clone(), root.clone());
            if let Some(node) = indexes.nodes.get(entry) {
                let edge = new_edge(
                    input,
                    "resolves_to",
                    entry.clone(),
                    EdgeTarget::Node(root.clone()),
                    &format!("entry:{}", entry.as_str()),
                    node.provenance.evidence.clone(),
                    Confidence::High,
                );
                if plan.add(edge) {
                    plan.resolve_node(node.id.clone());
                    plan.resolve_node(target.clone());
                }
            }
            if let Some(edge) = input.fragment.edges.iter().find(|edge| {
                edge.kind == "has_entry_point"
                    && edge.source == *target
                    && edge.target == EdgeTarget::Node(entry.clone())
            }) && edge.provenance.resolution != ResolutionState::Resolved
            {
                plan.replace(
                    edge,
                    resolved_edge(edge, edge.target.clone(), Confidence::High),
                );
            }
            if let Some(packages) = indexes.target_packages.get(target) {
                for package in packages {
                    insert_set(&mut result.module_packages, &root, package);
                    if indexes
                        .nodes
                        .get(target)
                        .and_then(|node| string_property(node, "target_kind"))
                        == Some("lib")
                    {
                        insert_set(&mut result.package_lib_roots, package, &root);
                    }
                }
            }
        }
    }

    let mut changed = true;
    while changed && !expired(started, duration) {
        changed = false;
        for (child, parents) in &module_graph.parents {
            let inherited = parents
                .iter()
                .filter_map(|parent| result.module_packages.get(parent))
                .flatten()
                .cloned()
                .collect::<BTreeSet<_>>();
            let memberships = result.module_packages.entry(child.clone()).or_default();
            let before = memberships.len();
            memberships.extend(inherited);
            changed |= memberships.len() != before;
        }
    }

    for (module, packages) in &result.module_packages {
        if expired(started, duration) {
            break;
        }
        let evidence = indexes
            .nodes
            .get(module)
            .and_then(|node| node.provenance.evidence.clone());
        for package in packages {
            let edge = new_edge(
                input,
                "belongs_to_package",
                module.clone(),
                EdgeTarget::Node(package.clone()),
                &format!("module:{}:package:{}", module.as_str(), package.as_str()),
                evidence.clone(),
                Confidence::High,
            );
            plan.add(edge);
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn resolve_workspace_membership(
    input: &CrossFileResolutionInput<'_>,
    indexes: &Indexes<'_>,
    plan: &mut ResolutionPlan,
    diagnostics: &mut DiagnosticBuffer,
    result: &mut CargoResolution,
    started: Instant,
    duration: Duration,
) {
    let workspaces = indexes
        .nodes
        .values()
        .filter(|node| node.kind == "cargo_workspace")
        .copied()
        .collect::<Vec<_>>();
    let packages = indexes
        .nodes
        .values()
        .filter(|node| node.kind == "cargo_package")
        .copied()
        .collect::<Vec<_>>();

    for package in packages {
        if expired(started, duration) {
            return;
        }
        let Some(package_manifest) = string_property(package, "manifest_path") else {
            continue;
        };
        let package_directory = manifest_directory(package_manifest);
        let mut matches = BTreeSet::new();
        let mut unsupported = false;
        for workspace in &workspaces {
            let Some(workspace_manifest) = string_property(workspace, "manifest_path") else {
                continue;
            };
            if workspace_manifest == package_manifest {
                matches.insert(workspace.id.clone());
                continue;
            }
            let member_patterns = string_list_property(workspace, "member_patterns");
            let exclude_patterns = string_list_property(workspace, "exclude_patterns");
            let mut workspace_unsupported = false;
            let included = member_patterns.iter().any(|pattern| {
                match_workspace_pattern(pattern, &package_directory).unwrap_or_else(|| {
                    workspace_unsupported = true;
                    false
                })
            });
            let excluded = exclude_patterns.iter().any(|pattern| {
                match_workspace_pattern(pattern, &package_directory).unwrap_or_else(|| {
                    workspace_unsupported = true;
                    false
                })
            });
            unsupported |= workspace_unsupported;
            if !workspace_unsupported && included && !excluded {
                matches.insert(workspace.id.clone());
            }
        }
        if unsupported {
            diagnostics.push_evidence(
                "resolution.workspace_pattern_unsupported",
                package.provenance.evidence.as_ref(),
            );
        }
        match matches.len() {
            1 => {
                let workspace = matches.iter().next().expect("one workspace exists").clone();
                insert_set(&mut result.package_workspaces, &package.id, &workspace);
                let edge = new_edge(
                    input,
                    "workspace_contains_package",
                    workspace,
                    EdgeTarget::Node(package.id.clone()),
                    &format!("workspace-package:{}", package.id.as_str()),
                    package.provenance.evidence.clone(),
                    Confidence::High,
                );
                plan.add(edge);
            }
            0 => {}
            _ => diagnostics.push_evidence(
                "resolution.workspace_membership_ambiguous",
                package.provenance.evidence.as_ref(),
            ),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_imports(
    input: &CrossFileResolutionInput<'_>,
    indexes: &Indexes<'_>,
    module_graph: &ModuleGraph,
    cargo: &CargoResolution,
    resolved_dependencies: &ResolvedDependencies,
    plan: &mut ResolutionPlan,
    diagnostics: &mut DiagnosticBuffer,
    started: Instant,
    duration: Duration,
) {
    let dependency_aliases = dependency_aliases(indexes, resolved_dependencies);
    for kind in ["imports", "re_exports"] {
        for edge in sorted_edges(&input.fragment, kind) {
            if expired(started, duration) {
                return;
            }
            let EdgeTarget::Unresolved(raw) = &edge.target else {
                continue;
            };
            let path = match UsePath::parse(raw) {
                Ok(path) => path,
                Err(UsePathError::Unsupported) => {
                    diagnostics.push("resolution.import_unsupported", edge);
                    continue;
                }
                Err(UsePathError::Invalid) => {
                    diagnostics.push("resolution.import_invalid", edge);
                    continue;
                }
            };
            match resolve_use_path(
                &edge.source,
                &path,
                module_graph,
                cargo,
                &dependency_aliases,
            ) {
                UseResolution::Node(target) => {
                    plan.replace(
                        edge,
                        resolved_edge(edge, EdgeTarget::Node(target), Confidence::High),
                    );
                }
                UseResolution::External => {
                    plan.replace(
                        edge,
                        resolved_edge(
                            edge,
                            EdgeTarget::External(format!("rust-path:{}", path.canonical)),
                            Confidence::High,
                        ),
                    );
                }
                UseResolution::Missing => diagnostics.push("resolution.import_missing", edge),
                UseResolution::Ambiguous => diagnostics.push("resolution.import_ambiguous", edge),
            }
        }
    }
}

#[derive(Clone)]
enum DependencyTarget {
    Internal(NodeId),
    External,
    Unresolved,
}

fn dependency_aliases(
    indexes: &Indexes<'_>,
    resolved_dependencies: &ResolvedDependencies,
) -> BTreeMap<NodeId, BTreeMap<String, Vec<DependencyTarget>>> {
    let mut result: BTreeMap<NodeId, BTreeMap<String, Vec<DependencyTarget>>> = BTreeMap::new();
    for (dependency, owners) in &indexes.dependency_owners {
        let Some(node) = indexes.nodes.get(dependency) else {
            continue;
        };
        let Some(alias) = string_property(node, "alias") else {
            continue;
        };
        let target = if let Some(package) = resolved_dependencies.internal.get(dependency) {
            DependencyTarget::Internal(package.clone())
        } else if resolved_dependencies.external.contains(dependency)
            || string_property(node, "classification") == Some("external")
        {
            DependencyTarget::External
        } else {
            DependencyTarget::Unresolved
        };
        for owner in owners {
            result
                .entry(owner.clone())
                .or_default()
                .entry(normalize_identifier(alias))
                .or_default()
                .push(target.clone());
        }
    }
    result
}

fn resolve_use_path(
    source: &NodeId,
    path: &UsePath,
    graph: &ModuleGraph,
    cargo: &CargoResolution,
    aliases: &BTreeMap<NodeId, BTreeMap<String, Vec<DependencyTarget>>>,
) -> UseResolution {
    let (mut current, consumed) = match path.base {
        UseBase::Crate => match graph.root(source) {
            ChildLookup::Unique(root) => (root, 0),
            ChildLookup::Missing => return UseResolution::Missing,
            ChildLookup::Ambiguous => return UseResolution::Ambiguous,
        },
        UseBase::SelfModule => (source.clone(), 0),
        UseBase::Super(count) => {
            let mut current = source.clone();
            for _ in 0..count {
                current = match graph.parent(&current) {
                    ChildLookup::Unique(parent) => parent,
                    ChildLookup::Missing => return UseResolution::Missing,
                    ChildLookup::Ambiguous => return UseResolution::Ambiguous,
                };
            }
            (current, 0)
        }
        UseBase::Bare => {
            let first = &path.segments[0];
            let root = match graph.root(source) {
                ChildLookup::Unique(root) => root,
                ChildLookup::Missing => return UseResolution::Missing,
                ChildLookup::Ambiguous => return UseResolution::Ambiguous,
            };
            match graph.child(&root, first) {
                ChildLookup::Unique(child) => (child, 1),
                ChildLookup::Ambiguous => return UseResolution::Ambiguous,
                ChildLookup::Missing => match dependency_target(source, first, cargo, aliases) {
                    DependencyLookup::Internal(root) => (root, 1),
                    DependencyLookup::External => return UseResolution::External,
                    DependencyLookup::Missing => return UseResolution::Missing,
                    DependencyLookup::Ambiguous => return UseResolution::Ambiguous,
                },
            }
        }
    };

    for segment in path.segments.iter().skip(consumed) {
        current = match graph.child(&current, segment) {
            ChildLookup::Unique(child) => child,
            ChildLookup::Missing => return UseResolution::Missing,
            ChildLookup::Ambiguous => return UseResolution::Ambiguous,
        };
    }
    UseResolution::Node(current)
}

enum DependencyLookup {
    Internal(NodeId),
    External,
    Missing,
    Ambiguous,
}

fn dependency_target(
    module: &NodeId,
    alias: &str,
    cargo: &CargoResolution,
    aliases: &BTreeMap<NodeId, BTreeMap<String, Vec<DependencyTarget>>>,
) -> DependencyLookup {
    if matches!(alias, "std" | "core" | "alloc") {
        return DependencyLookup::External;
    }
    let Some(packages) = cargo.module_packages.get(module) else {
        return DependencyLookup::Missing;
    };
    let matches = packages
        .iter()
        .filter_map(|package| aliases.get(package))
        .filter_map(|by_alias| by_alias.get(alias))
        .flatten()
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return if matches.is_empty() {
            DependencyLookup::Missing
        } else {
            DependencyLookup::Ambiguous
        };
    }
    match matches[0] {
        DependencyTarget::External => DependencyLookup::External,
        DependencyTarget::Unresolved => DependencyLookup::Missing,
        DependencyTarget::Internal(package) => match cargo.package_lib_roots.get(package) {
            Some(roots) if roots.len() == 1 => DependencyLookup::Internal(
                roots
                    .iter()
                    .next()
                    .expect("one library root exists")
                    .clone(),
            ),
            Some(roots) if !roots.is_empty() => DependencyLookup::Ambiguous,
            _ => DependencyLookup::Missing,
        },
    }
}

struct UsePath {
    base: UseBase,
    segments: Vec<String>,
    canonical: String,
}

enum UseBase {
    Crate,
    SelfModule,
    Super(usize),
    Bare,
}

enum UsePathError {
    Unsupported,
    Invalid,
}

impl UsePath {
    fn parse(raw: &str) -> Result<Self, UsePathError> {
        let raw = raw.trim();
        if raw.contains(['{', '}', '*']) {
            return Err(UsePathError::Unsupported);
        }
        let target = raw
            .rsplit_once(" as ")
            .map_or(raw, |(target, _alias)| target)
            .trim();
        if target.is_empty() || target.starts_with("::") {
            return Err(UsePathError::Invalid);
        }
        let mut parts = target
            .split("::")
            .map(normalize_identifier)
            .collect::<Vec<_>>();
        if parts.iter().any(String::is_empty) {
            return Err(UsePathError::Invalid);
        }
        let base = match parts.first().map(String::as_str) {
            Some("crate") => {
                parts.remove(0);
                UseBase::Crate
            }
            Some("self") => {
                parts.remove(0);
                UseBase::SelfModule
            }
            Some("super") => {
                let count = parts
                    .iter()
                    .take_while(|part| part.as_str() == "super")
                    .count();
                parts.drain(..count);
                UseBase::Super(count)
            }
            Some(_) => UseBase::Bare,
            None => return Err(UsePathError::Invalid),
        };
        Ok(Self {
            base,
            canonical: target.to_string(),
            segments: parts,
        })
    }
}

enum UseResolution {
    Node(NodeId),
    External,
    Missing,
    Ambiguous,
}

struct ResolutionPlan {
    limit: u64,
    used: u64,
    truncated: bool,
    replacements: BTreeMap<super::domain::EdgeId, GraphEdge>,
    additions: BTreeMap<super::domain::EdgeId, GraphEdge>,
    resolved_nodes: BTreeSet<NodeId>,
    existing_edges: BTreeSet<super::domain::EdgeId>,
}

impl ResolutionPlan {
    fn new(limit: u64, fragment: &GraphFragment) -> Self {
        Self {
            limit,
            used: 0,
            truncated: false,
            replacements: BTreeMap::new(),
            additions: BTreeMap::new(),
            resolved_nodes: BTreeSet::new(),
            existing_edges: fragment.edges.iter().map(|edge| edge.id.clone()).collect(),
        }
    }

    fn replace(&mut self, original: &GraphEdge, replacement: GraphEdge) -> bool {
        if self.replacements.contains_key(&original.id) {
            return true;
        }
        if !self.reserve() {
            return false;
        }
        self.replacements.insert(original.id.clone(), replacement);
        true
    }

    fn add(&mut self, edge: GraphEdge) -> bool {
        if self.existing_edges.contains(&edge.id) || self.additions.contains_key(&edge.id) {
            return true;
        }
        if !self.reserve() {
            return false;
        }
        self.additions.insert(edge.id.clone(), edge);
        true
    }

    fn resolve_node(&mut self, node: NodeId) {
        self.resolved_nodes.insert(node);
    }

    fn reserve(&mut self) -> bool {
        if self.used >= self.limit {
            self.truncated = true;
            false
        } else {
            self.used += 1;
            true
        }
    }
}

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
        &original.snapshot_id,
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
    let id = deterministic_edge_id(
        &input.context.snapshot_id,
        &resolver_identity(),
        kind,
        &source,
        &target,
        local_key,
    );
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
mod tests {
    use sha2::{Digest as _, Sha256};

    use super::*;
    use crate::repository_graph::{
        domain::{
            BuildId, Digest, RepositoryId, RepositoryNamespace, RepositoryRef, SnapshotId,
            SourceKind, SourceRevision, SourceRevisionId,
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
            fragment.diagnostics.iter().any(|diagnostic| {
                diagnostic.code.as_str() == "resolution.module_path_override"
            })
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
    fn resolution_is_idempotent() {
        let fixture = Fixture::new(&[
            ("Cargo.toml", b"[package]\nname='app'\nversion='0.1.0'\n"),
            ("src/lib.rs", b"mod api;\nuse crate::api::Api;\n"),
            ("src/api.rs", b"pub struct Api;\n"),
        ]);
        let budget = ResolutionBudget {
            max_relationships: 1_000,
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
}
