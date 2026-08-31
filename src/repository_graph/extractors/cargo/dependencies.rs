use super::*;

pub(super) fn extract_dependency_groups(
    manifest: &toml::Table,
    spans: &SpanIndex,
    facts: &mut FactBuffer<'_, '_>,
    package: &NodeId,
) {
    for (key, scope) in [
        ("dependencies", "normal"),
        ("dev-dependencies", "dev"),
        ("build-dependencies", "build"),
    ] {
        if !facts.active() {
            return;
        }
        if let Some(value) = manifest.get(key) {
            extract_dependency_table(value, scope, None, spans.header(key, 0), facts, package);
        }
    }

    let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) else {
        return;
    };
    for (condition, target) in targets {
        if !facts.active() {
            return;
        }
        let Some(target) = target.as_table() else {
            facts.diagnostic("cargo.invalid_target_dependency_table", None);
            continue;
        };
        for (key, scope) in [
            ("dependencies", "normal"),
            ("dev-dependencies", "dev"),
            ("build-dependencies", "build"),
        ] {
            if !facts.active() {
                return;
            }
            if let Some(value) = target.get(key) {
                extract_dependency_table(
                    value,
                    scope,
                    Some(condition),
                    spans.target_dependency_header(condition, key),
                    facts,
                    package,
                );
            }
        }
    }
}

pub(super) fn extract_workspace_dependencies(
    manifest: &toml::Table,
    spans: &SpanIndex,
    facts: &mut FactBuffer<'_, '_>,
    workspace: &NodeId,
) {
    let Some(workspace_table) = manifest.get("workspace").and_then(toml::Value::as_table) else {
        return;
    };
    let Some(dependencies) = workspace_table.get("dependencies") else {
        return;
    };
    extract_dependency_table(
        dependencies,
        "workspace",
        None,
        spans.header("workspace.dependencies", 0),
        facts,
        workspace,
    );
}

pub(super) fn extract_dependency_table(
    value: &toml::Value,
    scope: &str,
    target_condition: Option<&str>,
    span: Option<SourceSpan>,
    facts: &mut FactBuffer<'_, '_>,
    owner: &NodeId,
) {
    let Some(dependencies) = value.as_table() else {
        facts.diagnostic("cargo.invalid_dependency_table", span);
        return;
    };
    let mut dependencies = dependencies.iter().collect::<Vec<_>>();
    dependencies.sort_by(|left, right| left.0.cmp(right.0));

    for (alias, declaration) in dependencies {
        if !facts.active() {
            break;
        }
        let parsed = parse_dependency(facts.manifest_path(), alias, declaration);
        if parsed.invalid {
            facts.diagnostic("cargo.invalid_dependency_declaration", span.clone());
        }
        if parsed.path_outside_repository {
            facts.diagnostic("cargo.dependency_path_outside_repository", span.clone());
        }
        let condition = target_condition.unwrap_or("");
        let key = semantic_key(
            "dependency",
            &[facts.manifest_path(), scope, condition, alias],
        );
        let mut properties = BTreeMap::from([
            ("alias".to_string(), GraphValue::String(alias.clone())),
            (
                "package_name".to_string(),
                GraphValue::String(parsed.package_name.clone()),
            ),
            ("scope".to_string(), GraphValue::String(scope.to_string())),
            (
                "classification".to_string(),
                GraphValue::String(parsed.classification.to_string()),
            ),
        ]);
        if let Some(condition) = target_condition {
            properties.insert(
                "target_condition".to_string(),
                GraphValue::String(condition.to_string()),
            );
        }
        if let Some(version) = parsed.version {
            properties.insert("version".to_string(), GraphValue::String(version));
        }
        if let Some(registry) = parsed.registry {
            properties.insert("registry".to_string(), GraphValue::String(registry));
        }
        if let Some(optional) = parsed.optional {
            properties.insert("optional".to_string(), GraphValue::Boolean(optional));
        }
        if let Some(default_features) = parsed.default_features {
            properties.insert(
                "default_features".to_string(),
                GraphValue::Boolean(default_features),
            );
        }
        if !parsed.features.is_empty() {
            properties.insert(
                "features".to_string(),
                GraphValue::StringList(parsed.features),
            );
        }
        if parsed.git {
            // Deliberately do not persist a possibly credential-bearing Git URL.
            properties.insert("git".to_string(), GraphValue::Boolean(true));
        }

        let Some(dependency) = facts.node(
            "declared_dependency",
            &key,
            span.clone(),
            ResolutionState::Resolved,
            Confidence::Exact,
            properties,
        ) else {
            continue;
        };
        facts.edge(
            "declares_dependency",
            owner,
            EdgeTarget::Node(dependency.clone()),
            &key,
            span.clone(),
            ResolutionState::Resolved,
            Confidence::Exact,
            BTreeMap::new(),
        );
        facts.edge(
            "depends_on",
            &dependency,
            parsed.target,
            &key,
            span.clone(),
            parsed.resolution,
            parsed.confidence,
            BTreeMap::new(),
        );
    }
}

pub(super) struct ParsedDependency {
    package_name: String,
    classification: &'static str,
    target: EdgeTarget,
    resolution: ResolutionState,
    confidence: Confidence,
    version: Option<String>,
    registry: Option<String>,
    optional: Option<bool>,
    default_features: Option<bool>,
    features: Vec<String>,
    git: bool,
    invalid: bool,
    path_outside_repository: bool,
}

pub(super) fn parse_dependency(
    manifest_path: &str,
    alias: &str,
    declaration: &toml::Value,
) -> ParsedDependency {
    if let Some(version) = declaration.as_str() {
        return ParsedDependency {
            package_name: alias.to_string(),
            classification: "external",
            target: EdgeTarget::External(format!("cargo-crate:{}", escape_component(alias))),
            resolution: ResolutionState::External,
            confidence: Confidence::Exact,
            version: Some(version.to_string()),
            registry: None,
            optional: None,
            default_features: None,
            features: Vec::new(),
            git: false,
            invalid: false,
            path_outside_repository: false,
        };
    }

    let Some(table) = declaration.as_table() else {
        return unresolved_dependency(alias, true);
    };
    let package_name = table
        .get("package")
        .and_then(toml::Value::as_str)
        .unwrap_or(alias)
        .to_string();
    let path = table.get("path").and_then(toml::Value::as_str);
    let workspace = table.get("workspace").and_then(toml::Value::as_bool);
    let git = table.get("git").and_then(toml::Value::as_str).is_some();
    let version = table
        .get("version")
        .and_then(toml::Value::as_str)
        .map(ToString::to_string);
    let registry = table
        .get("registry")
        .and_then(toml::Value::as_str)
        .map(ToString::to_string);
    let optional = table.get("optional").and_then(toml::Value::as_bool);
    let default_features = table.get("default-features").and_then(toml::Value::as_bool);
    let features_valid = table.get("features").is_none_or(|value| {
        value
            .as_array()
            .is_some_and(|values| values.iter().all(|value| value.as_str().is_some()))
    });
    let mut features = string_list(table.get("features")).unwrap_or_default();
    features.sort();
    features.dedup();

    let source_count = u8::from(path.is_some())
        + u8::from(git)
        + u8::from(workspace == Some(true))
        + u8::from(registry.is_some());
    let invalid_types = table.contains_key("path") && path.is_none()
        || table.contains_key("workspace") && workspace.is_none()
        || table.contains_key("git") && !git
        || table.contains_key("version") && version.is_none()
        || table.contains_key("package")
            && table.get("package").and_then(toml::Value::as_str).is_none()
        || table.contains_key("registry")
            && table
                .get("registry")
                .and_then(toml::Value::as_str)
                .is_none()
        || table.contains_key("optional")
            && table
                .get("optional")
                .and_then(toml::Value::as_bool)
                .is_none()
        || table.contains_key("default-features")
            && table
                .get("default-features")
                .and_then(toml::Value::as_bool)
                .is_none()
        || workspace == Some(true) && version.is_some()
        || registry.is_some() && version.is_none()
        || !features_valid;
    let mut common = ParsedDependency {
        package_name: package_name.clone(),
        classification: "unresolved",
        target: EdgeTarget::Unresolved(format!(
            "cargo-dependency:{}",
            escape_component(&package_name)
        )),
        resolution: ResolutionState::Unresolved,
        confidence: Confidence::Low,
        version,
        registry,
        optional,
        default_features,
        features,
        git,
        invalid: invalid_types || source_count > 1 || workspace == Some(false),
        path_outside_repository: false,
    };
    if common.invalid {
        return common;
    }
    if let Some(path) = path {
        let Some(package_dir) = normalize_directory_from_manifest(manifest_path, path) else {
            common.path_outside_repository = true;
            return common;
        };
        let candidate = if package_dir.is_empty() {
            "Cargo.toml".to_string()
        } else {
            format!("{package_dir}/Cargo.toml")
        };
        common.classification = "internal_candidate";
        common.target = EdgeTarget::Unresolved(format!(
            "cargo-package-path:{}",
            escape_component(&candidate)
        ));
        common.confidence = Confidence::High;
        return common;
    }
    if workspace == Some(true) {
        common.classification = "workspace_unresolved";
        common.target = EdgeTarget::Unresolved(format!(
            "cargo-workspace-dependency:{}",
            escape_component(&package_name)
        ));
        common.confidence = Confidence::High;
        return common;
    }
    if git || common.version.is_some() || common.registry.is_some() {
        common.classification = "external";
        common.target =
            EdgeTarget::External(format!("cargo-crate:{}", escape_component(&package_name)));
        common.resolution = ResolutionState::External;
        common.confidence = Confidence::Exact;
        return common;
    }
    common
}

pub(super) fn unresolved_dependency(alias: &str, invalid: bool) -> ParsedDependency {
    ParsedDependency {
        package_name: alias.to_string(),
        classification: "unresolved",
        target: EdgeTarget::Unresolved(format!("cargo-dependency:{}", escape_component(alias))),
        resolution: ResolutionState::Unresolved,
        confidence: Confidence::Low,
        version: None,
        registry: None,
        optional: None,
        default_features: None,
        features: Vec::new(),
        git: false,
        invalid,
        path_outside_repository: false,
    }
}

pub(super) fn normalize_workspace_patterns(
    manifest_path: &str,
    patterns: Vec<String>,
) -> (Vec<String>, bool) {
    let mut rejected = false;
    let mut normalized = patterns
        .into_iter()
        .filter_map(
            |pattern| match normalize_directory_from_manifest(manifest_path, &pattern) {
                Some(path) if path.is_empty() => Some(".".to_string()),
                Some(path) => Some(path),
                None => {
                    rejected = true;
                    None
                }
            },
        )
        .collect::<Vec<_>>();
    normalized.sort();
    normalized.dedup();
    (normalized, rejected)
}

pub(super) fn normalize_from_manifest(manifest_path: &str, relative: &str) -> Option<String> {
    let normalized = normalize_directory_from_manifest(manifest_path, relative)?;
    (!normalized.is_empty()).then_some(normalized)
}

pub(super) fn normalize_directory_from_manifest(
    manifest_path: &str,
    relative: &str,
) -> Option<String> {
    let relative = relative.replace('\\', "/");
    if relative.starts_with('/')
        || relative.starts_with("//")
        || relative.as_bytes().get(1).is_some_and(|byte| *byte == b':')
        || relative.contains('\0')
    {
        return None;
    }
    let mut components = manifest_path.split('/').collect::<Vec<_>>();
    components.pop();
    for component in relative.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop()?;
            }
            component => components.push(component),
        }
    }
    Some(components.join("/"))
}

pub(super) fn file_stem(path: &str) -> Option<String> {
    let file = path.rsplit(['/', '\\']).next()?;
    let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
    (!stem.is_empty()).then(|| stem.to_string())
}

pub(super) fn insert_string(
    table: &toml::Table,
    key: &str,
    properties: &mut BTreeMap<String, GraphValue>,
) {
    if let Some(value) = table.get(key).and_then(toml::Value::as_str) {
        properties.insert(key.replace('-', "_"), GraphValue::String(value.to_string()));
    }
}

pub(super) fn insert_bool(
    table: &toml::Table,
    key: &str,
    properties: &mut BTreeMap<String, GraphValue>,
) {
    if let Some(value) = table.get(key).and_then(toml::Value::as_bool) {
        properties.insert(key.replace('-', "_"), GraphValue::Boolean(value));
    }
}

pub(super) fn string_list(value: Option<&toml::Value>) -> Option<Vec<String>> {
    value?
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(ToString::to_string))
        .collect()
}

pub(super) fn semantic_key(kind: &str, parts: &[&str]) -> String {
    let parts = parts
        .iter()
        .map(|part| escape_component(part))
        .collect::<Vec<_>>()
        .join(":");
    format!("cargo:{kind}:{parts}")
}

pub(super) fn escape_component(value: &str) -> String {
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

pub(super) fn extractor_identity() -> ExtractorIdentity {
    ExtractorIdentity {
        id: ExtractorId::new(EXTRACTOR_ID).expect("built-in extractor ID is non-empty"),
        version: EXTRACTOR_VERSION.to_string(),
        contract_version: EXTRACTOR_CONTRACT_VERSION,
    }
}
