//! Rust module graph construction and declaration resolution.

use super::*;

pub(in super::super) fn resolve_module_declarations(
    input: &CrossFileResolutionInput<'_>,
    indexes: &Indexes<'_>,
    plan: &mut ResolutionPlan,
    diagnostics: &mut DiagnosticBuffer,
    started: Instant,
    duration: Duration,
) -> BTreeMap<super::super::super::domain::EdgeId, NodeId> {
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
            diagnostics.push(
                const {
                    crate::repository_graph::domain::DiagnosticCode::from_static(
                        "resolution.module_path_override",
                    )
                },
                edge,
            );
            continue;
        }

        let Some(base) = module_base(&edge.source, indexes, &containment_parents) else {
            diagnostics.push(
                const {
                    crate::repository_graph::domain::DiagnosticCode::from_static(
                        "resolution.module_scope_ambiguous",
                    )
                },
                edge,
            );
            continue;
        };
        let name = normalize_identifier(name);
        if name.is_empty() || name.contains('/') || name.contains("::") {
            diagnostics.push(
                const {
                    crate::repository_graph::domain::DiagnosticCode::from_static(
                        "resolution.module_target_unsupported",
                    )
                },
                edge,
            );
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
                let Some(target) = matches.into_iter().next() else {
                    continue;
                };
                let replacement =
                    resolved_edge(edge, EdgeTarget::Node(target.clone()), Confidence::High);
                if plan.replace(edge, replacement) {
                    resolved.insert(edge.id.clone(), target);
                }
            }
            0 => diagnostics.push(
                const {
                    crate::repository_graph::domain::DiagnosticCode::from_static(
                        "resolution.module_missing",
                    )
                },
                edge,
            ),
            _ => diagnostics.push(
                const {
                    crate::repository_graph::domain::DiagnosticCode::from_static(
                        "resolution.module_ambiguous",
                    )
                },
                edge,
            ),
        }
    }

    resolved
}

pub(super) fn containment_module_parents(
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

pub(super) fn module_base(
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

pub(in super::super) struct ModuleGraph {
    pub(super) children: BTreeMap<(NodeId, String), BTreeSet<NodeId>>,
    pub(super) parents: BTreeMap<NodeId, BTreeSet<NodeId>>,
}

impl ModuleGraph {
    pub(in super::super) fn new(
        fragment: &GraphFragment,
        indexes: &Indexes<'_>,
        resolved_modules: &BTreeMap<super::super::super::domain::EdgeId, NodeId>,
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
            if matches!(node.kind.as_str(), "mod_declaration" | "impl") {
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

    pub(super) fn child(&self, parent: &NodeId, name: &str) -> ChildLookup {
        match self
            .children
            .get(&(parent.clone(), normalize_identifier(name)))
        {
            Some(matches) if matches.len() == 1 => matches
                .iter()
                .next()
                .cloned()
                .map(ChildLookup::Unique)
                .unwrap_or(ChildLookup::Missing),
            Some(matches) if !matches.is_empty() => ChildLookup::Ambiguous,
            _ => ChildLookup::Missing,
        }
    }

    pub(super) fn parent(&self, child: &NodeId) -> ChildLookup {
        match self.parents.get(child) {
            Some(matches) if matches.len() == 1 => matches
                .iter()
                .next()
                .cloned()
                .map(ChildLookup::Unique)
                .unwrap_or(ChildLookup::Missing),
            Some(matches) if !matches.is_empty() => ChildLookup::Ambiguous,
            _ => ChildLookup::Missing,
        }
    }

    pub(super) fn root(&self, node: &NodeId) -> ChildLookup {
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

pub(super) enum ChildLookup {
    Unique(NodeId),
    Missing,
    Ambiguous,
}
