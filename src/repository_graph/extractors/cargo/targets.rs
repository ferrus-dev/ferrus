//! Cargo target discovery and fact emission.

use super::*;

pub(super) fn extract_targets(
    manifest: &toml::Table,
    spans: &SpanIndex,
    facts: &mut FactBuffer<'_, '_>,
    package: &NodeId,
) {
    let package_table = manifest.get("package").and_then(toml::Value::as_table);
    let Some(package_name) = package_table
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
    else {
        return;
    };

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
            facts.diagnostic(
                const {
                    crate::repository_graph::domain::DiagnosticCode::from_static(
                        "cargo.invalid_target_table",
                    )
                },
                spans.header("lib", 0),
            );
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
            facts.diagnostic(
                const {
                    crate::repository_graph::domain::DiagnosticCode::from_static(
                        "cargo.invalid_target_array",
                    )
                },
                spans.header(manifest_key, 0),
            );
            continue;
        };
        for (index, target) in targets.iter().enumerate() {
            if !facts.active() {
                return;
            }

            let span = spans.header(manifest_key, index);
            let Some(table) = target.as_table() else {
                facts.diagnostic(
                    const {
                        crate::repository_graph::domain::DiagnosticCode::from_static(
                            "cargo.invalid_target_table",
                        )
                    },
                    span,
                );
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
                facts.diagnostic(
                    const {
                        crate::repository_graph::domain::DiagnosticCode::from_static(
                            "cargo.target_name_missing",
                        )
                    },
                    span,
                );
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

pub(super) fn extract_build_target(
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
                facts.diagnostic(
                    const {
                        crate::repository_graph::domain::DiagnosticCode::from_static(
                            "cargo.invalid_build_target",
                        )
                    },
                    span,
                );
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

pub(super) fn target_tables<'a>(
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

pub(super) fn emit_conventional_target(
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
pub(super) fn emit_target(
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
pub(super) fn emit_target_with_path(
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
        facts.diagnostic(
            const {
                crate::repository_graph::domain::DiagnosticCode::from_static(
                    "cargo.target_path_outside_repository",
                )
            },
            span.clone(),
        );
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

pub(super) fn conventional_path(
    target_kind: &str,
    name: &str,
    package_name: &str,
) -> Option<String> {
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
