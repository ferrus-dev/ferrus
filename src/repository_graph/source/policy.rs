//! Apply hard exclusions and configured include/exclude patterns before source ingestion.

use super::*;

#[derive(Debug, Clone)]
pub(super) struct SourcePolicy {
    include: Vec<String>,
    rules: Vec<(bool, String)>,
    sensitive: Vec<String>,
    pub(super) include_untracked: bool,
    include_generated: bool,
    include_vendor: bool,
    has_negated_rules: bool,
}

impl SourcePolicy {
    pub(super) fn new(config: &SourceConfig) -> Result<Self, SourceError> {
        let include = config
            .include
            .iter()
            .map(|pattern| canonical_pattern_body(pattern).map_err(SourceError::from))
            .collect::<Result<Vec<_>, _>>()?;
        let rules = config
            .rules
            .iter()
            .map(|pattern| {
                let (negated, body) = pattern
                    .trim()
                    .strip_prefix('!')
                    .map_or((false, pattern.as_str()), |body| (true, body));
                Ok((negated, canonical_pattern_body(body)?))
            })
            .collect::<Result<Vec<_>, SourceError>>()?;
        let sensitive = config
            .sensitive
            .iter()
            .map(|pattern| canonical_pattern_body(pattern).map_err(SourceError::from))
            .collect::<Result<Vec<_>, _>>()?;
        let has_negated_rules = rules.iter().any(|(negated, _)| *negated);
        Ok(Self {
            include,
            rules,
            sensitive,
            include_untracked: config.include_untracked,
            include_generated: config.include_generated,
            include_vendor: config.include_vendor,
            has_negated_rules,
        })
    }

    pub(super) fn exclusion_for_file(&self, path: &RepoPath) -> Option<&'static str> {
        if hard_excluded(path) {
            return Some("runtime_path_excluded");
        }
        if self
            .sensitive
            .iter()
            .any(|pattern| sensitive_glob_matches(pattern, path.as_str()))
        {
            return Some("sensitive_path_excluded");
        }
        if !self.include_vendor && is_vendor(path) {
            return Some("vendor_path_excluded");
        }
        if !self.include_generated && is_generated(path) {
            return Some("generated_path_excluded");
        }
        if !self
            .include
            .iter()
            .any(|pattern| glob_matches(pattern, path.as_str()))
        {
            return Some("path_not_included");
        }
        let mut excluded = false;
        for (negated, pattern) in &self.rules {
            if glob_matches(pattern, path.as_str()) {
                excluded = !negated;
            }
        }
        excluded.then_some("source_rule_excluded")
    }

    pub(super) fn exclusion_for_directory(&self, path: &RepoPath) -> Option<&'static str> {
        if hard_excluded(path) {
            return Some("runtime_path_excluded");
        }
        if self
            .sensitive
            .iter()
            .any(|pattern| sensitive_glob_matches(pattern, path.as_str()))
        {
            return Some("sensitive_path_excluded");
        }
        if !self.include_vendor && is_vendor(path) {
            return Some("vendor_path_excluded");
        }
        if !self.include_generated && is_generated(path) {
            return Some("generated_path_excluded");
        }
        if !self
            .include
            .iter()
            .any(|pattern| glob_may_match_descendant(pattern, path.as_str()))
        {
            return Some("path_not_included");
        }
        if self.has_negated_rules {
            return None;
        }
        self.rules
            .iter()
            .rev()
            .find(|(_, pattern)| glob_matches(pattern, path.as_str()))
            .and_then(|(negated, _)| (!negated).then_some("source_rule_excluded"))
    }
}

fn hard_excluded(path: &RepoPath) -> bool {
    path.as_str().split('/').any(|component| {
        component.eq_ignore_ascii_case(".git") || component.eq_ignore_ascii_case(".ferrus")
    })
}

fn is_vendor(path: &RepoPath) -> bool {
    path.as_str().split('/').any(|component| {
        ["vendor", "node_modules", "third_party"]
            .iter()
            .any(|name| component.eq_ignore_ascii_case(name))
    })
}

fn is_generated(path: &RepoPath) -> bool {
    path.as_str().split('/').any(|component| {
        [
            "target",
            "dist",
            "build",
            "out",
            "coverage",
            ".next",
            "generated",
        ]
        .iter()
        .any(|name| component.eq_ignore_ascii_case(name))
    })
}

pub(super) fn glob_matches(pattern: &str, path: &str) -> bool {
    let pattern_components = pattern.split('/').collect::<Vec<_>>();
    let path_components = path.split('/').collect::<Vec<_>>();
    if pattern_components.len() == 1 {
        return path_components
            .iter()
            .any(|component| component_matches(pattern_components[0], component));
    }
    components_match(&pattern_components, &path_components)
}

pub(super) fn glob_may_match_descendant(pattern: &str, directory: &str) -> bool {
    let pattern_components = pattern.split('/').collect::<Vec<_>>();
    if pattern_components.len() == 1 {
        return true;
    }
    let directory_components = directory.split('/').collect::<Vec<_>>();
    let mut reachable = BTreeSet::from([(0_usize, 0_usize)]);
    let mut visited = BTreeSet::new();
    while let Some(state) = reachable.pop_first() {
        if !visited.insert(state) {
            continue;
        }
        let (pattern_index, directory_index) = state;
        if directory_index == directory_components.len() && pattern_index < pattern_components.len()
        {
            return true;
        }
        let Some(component) = pattern_components.get(pattern_index) else {
            continue;
        };
        if *component == "**" {
            reachable.insert((pattern_index + 1, directory_index));
            if directory_index < directory_components.len() {
                reachable.insert((pattern_index, directory_index + 1));
            }
        } else if let Some(directory_component) = directory_components.get(directory_index)
            && component_matches(component, directory_component)
        {
            reachable.insert((pattern_index + 1, directory_index + 1));
        }
    }
    false
}

fn sensitive_glob_matches(pattern: &str, path: &str) -> bool {
    glob_matches(&pattern.to_ascii_lowercase(), &path.to_ascii_lowercase())
}

fn components_match(pattern: &[&str], path: &[&str]) -> bool {
    let mut reachable = BTreeSet::from([(0_usize, 0_usize)]);
    let mut visited = BTreeSet::new();
    while let Some(state) = reachable.pop_first() {
        if !visited.insert(state) {
            continue;
        }
        let (pattern_index, path_index) = state;
        if pattern_index == pattern.len() && path_index == path.len() {
            return true;
        }
        let Some(component) = pattern.get(pattern_index) else {
            continue;
        };
        if *component == "**" {
            reachable.insert((pattern_index + 1, path_index));
            if path_index < path.len() {
                reachable.insert((pattern_index, path_index + 1));
            }
        } else if let Some(path_component) = path.get(path_index)
            && component_matches(component, path_component)
        {
            reachable.insert((pattern_index + 1, path_index + 1));
        }
    }
    false
}

fn component_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let value = value.chars().collect::<Vec<_>>();
    let mut reachable = BTreeSet::from([(0_usize, 0_usize)]);
    let mut visited = BTreeSet::new();
    while let Some(state) = reachable.pop_first() {
        if !visited.insert(state) {
            continue;
        }
        let (pattern_index, value_index) = state;
        if pattern_index == pattern.len() && value_index == value.len() {
            return true;
        }
        match pattern.get(pattern_index) {
            Some('*') => {
                reachable.insert((pattern_index + 1, value_index));
                if value_index < value.len() {
                    reachable.insert((pattern_index, value_index + 1));
                }
            }
            Some('?') if value_index < value.len() => {
                reachable.insert((pattern_index + 1, value_index + 1));
            }
            Some(expected)
                if value
                    .get(value_index)
                    .is_some_and(|actual| actual == expected) =>
            {
                reachable.insert((pattern_index + 1, value_index + 1));
            }
            _ => {}
        }
    }
    false
}
