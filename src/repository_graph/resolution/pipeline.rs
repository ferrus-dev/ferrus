//! Resolve Cargo dependencies and Rust cross-file relationships from extracted evidence.

use super::*;

pub(super) fn resolve_cargo_dependencies(
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
                    [] => diagnostics.push(
                        const {
                            crate::repository_graph::domain::DiagnosticCode::from_static(
                                "resolution.dependency_missing",
                            )
                        },
                        edge,
                    ),
                    _ => diagnostics.push(
                        const {
                            crate::repository_graph::domain::DiagnosticCode::from_static(
                                "resolution.dependency_ambiguous",
                            )
                        },
                        edge,
                    ),
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
            diagnostics.push(
                const {
                    crate::repository_graph::domain::DiagnosticCode::from_static(
                        "resolution.dependency_missing",
                    )
                },
                edge,
            );
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
                    const {
                        crate::repository_graph::domain::DiagnosticCode::from_static(
                            "resolution.dependency_missing",
                        )
                    }
                } else {
                    const {
                        crate::repository_graph::domain::DiagnosticCode::from_static(
                            "resolution.dependency_ambiguous",
                        )
                    }
                },
                edge,
            );
            continue;
        }

        let Some(workspace) = workspaces.iter().next() else {
            continue;
        };
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
                let Some(target) = inherited.into_iter().next() else {
                    continue;
                };
                if plan.replace(edge, resolved_edge(edge, target.clone(), Confidence::Exact)) {
                    known_targets.insert(edge.source.clone(), target.clone());
                    if let EdgeTarget::Node(package) = target {
                        resolved.internal.insert(edge.source.clone(), package);
                    } else if matches!(target, EdgeTarget::External(_)) {
                        resolved.external.insert(edge.source.clone());
                    }
                }
            }
            0 => diagnostics.push(
                const {
                    crate::repository_graph::domain::DiagnosticCode::from_static(
                        "resolution.dependency_missing",
                    )
                },
                edge,
            ),
            _ => diagnostics.push(
                const {
                    crate::repository_graph::domain::DiagnosticCode::from_static(
                        "resolution.dependency_ambiguous",
                    )
                },
                edge,
            ),
        }
    }

    resolved
}

#[derive(Default)]
pub(super) struct ResolvedDependencies {
    internal: BTreeMap<NodeId, NodeId>,
    external: BTreeSet<NodeId>,
}

#[derive(Default)]
pub(super) struct CargoResolution {
    module_packages: BTreeMap<NodeId, BTreeSet<NodeId>>,
    package_lib_roots: BTreeMap<NodeId, BTreeSet<NodeId>>,
    package_workspaces: BTreeMap<NodeId, BTreeSet<NodeId>>,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn resolve_cargo_membership(
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
                const {
                    crate::repository_graph::domain::DiagnosticCode::from_static(
                        "resolution.workspace_pattern_unsupported",
                    )
                },
                package.provenance.evidence.as_ref(),
            );
        }

        match matches.len() {
            1 => {
                let Some(workspace) = matches.iter().next().cloned() else {
                    continue;
                };
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
                const {
                    crate::repository_graph::domain::DiagnosticCode::from_static(
                        "resolution.workspace_membership_ambiguous",
                    )
                },
                package.provenance.evidence.as_ref(),
            ),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn resolve_imports(
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
                    diagnostics.push(
                        const {
                            crate::repository_graph::domain::DiagnosticCode::from_static(
                                "resolution.import_unsupported",
                            )
                        },
                        edge,
                    );
                    continue;
                }
                Err(UsePathError::Invalid) => {
                    diagnostics.push(
                        const {
                            crate::repository_graph::domain::DiagnosticCode::from_static(
                                "resolution.import_invalid",
                            )
                        },
                        edge,
                    );
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
                UseResolution::Missing => diagnostics.push(
                    const {
                        crate::repository_graph::domain::DiagnosticCode::from_static(
                            "resolution.import_missing",
                        )
                    },
                    edge,
                ),
                UseResolution::Ambiguous => diagnostics.push(
                    const {
                        crate::repository_graph::domain::DiagnosticCode::from_static(
                            "resolution.import_ambiguous",
                        )
                    },
                    edge,
                ),
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
                .entry(normalize_crate_name(alias))
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
            Some(roots) if roots.len() == 1 => roots
                .iter()
                .next()
                .cloned()
                .map(DependencyLookup::Internal)
                .unwrap_or(DependencyLookup::Missing),
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

pub(super) struct ResolutionPlan {
    pub(super) limit: u64,
    pub(super) used: u64,
    pub(super) addition_limit: u64,
    pub(super) additions_used: u64,
    pub(super) truncated: bool,
    pub(super) replacements: BTreeMap<super::super::domain::EdgeId, GraphEdge>,
    pub(super) additions: BTreeMap<super::super::domain::EdgeId, GraphEdge>,
    pub(super) resolved_nodes: BTreeSet<NodeId>,
    pub(super) existing_edges: BTreeSet<super::super::domain::EdgeId>,
}

impl ResolutionPlan {
    pub(super) fn new(limit: u64, addition_limit: u64, fragment: &GraphFragment) -> Self {
        Self {
            limit,
            used: 0,
            addition_limit,
            additions_used: 0,
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

        if self.additions_used >= self.addition_limit {
            self.truncated = true;
            return false;
        }

        if !self.reserve() {
            return false;
        }

        self.additions_used += 1;
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

mod modules;
pub(super) use modules::*;
