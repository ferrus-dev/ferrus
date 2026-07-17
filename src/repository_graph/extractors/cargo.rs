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
#[derive(Debug, Clone, Copy, Default)]
pub struct CargoExtractor;

impl CargoExtractor {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ParserOutput {
    Parsed { manifest: toml::Table },
    Malformed { span: Option<ParserSpan> },
}

#[derive(Debug, Serialize, Deserialize)]
struct ParserSpan {
    start: usize,
    end: usize,
}

impl ParserSpan {
    fn into_range(self) -> Range<usize> {
        self.start..self.end
    }
}

enum ParserDeadline {
    Completed(ParserOutput),
    TimedOut,
    Unavailable,
}

enum ChildDeadline {
    Exited(ExitStatus),
    TimedOut,
    Unavailable,
}

fn parse_manifest(source: &str) -> ParserOutput {
    match toml::from_str::<toml::Table>(source) {
        Ok(manifest) => ParserOutput::Parsed { manifest },
        Err(error) => ParserOutput::Malformed {
            span: error.span().map(|span| ParserSpan {
                start: span.start,
                end: span.end,
            }),
        },
    }
}

/// Runs the isolated Cargo parser protocol before the public CLI is initialized.
///
/// This is an internal entry point used only by parser subprocesses spawned by
/// [`CargoExtractor`]. It is public so the `ferrus` binary can dispatch into
/// the library without exposing a user-facing CLI command.
#[doc(hidden)]
pub fn run_parser_worker_if_requested() -> io::Result<bool> {
    if std::env::args_os().nth(1).as_deref() != Some(OsStr::new(PARSER_WORKER_ARGUMENT)) {
        return Ok(false);
    }

    let mut source = String::new();
    io::stdin().read_to_string(&mut source)?;
    let output = parse_manifest(&source);
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, &output).map_err(io::Error::other)?;
    stdout.flush()?;
    Ok(true)
}

fn run_parser_with_deadline(started: Instant, budget: Duration, source: String) -> ParserDeadline {
    if budget.saturating_sub(started.elapsed()).is_zero() {
        return ParserDeadline::TimedOut;
    }

    if cfg!(test) {
        let output = parse_manifest(&source);
        return if started.elapsed() >= budget {
            ParserDeadline::TimedOut
        } else {
            ParserDeadline::Completed(output)
        };
    }

    run_parser_process(started, budget, source)
}

fn run_parser_process(started: Instant, budget: Duration, source: String) -> ParserDeadline {
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(_) => return ParserDeadline::Unavailable,
    };
    let mut child = match Command::new(executable)
        .arg(PARSER_WORKER_ARGUMENT)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return ParserDeadline::Unavailable,
    };
    let Some(mut stdin) = child.stdin.take() else {
        terminate_and_reap(&mut child);
        return ParserDeadline::Unavailable;
    };
    let Some(mut stdout) = child.stdout.take() else {
        terminate_and_reap(&mut child);
        return ParserDeadline::Unavailable;
    };
    let writer = match thread::Builder::new()
        .name("ferrus-cargo-parser-input".to_string())
        .spawn(move || stdin.write_all(source.as_bytes()))
    {
        Ok(writer) => writer,
        Err(_) => {
            terminate_and_reap(&mut child);
            return ParserDeadline::Unavailable;
        }
    };
    let reader = match thread::Builder::new()
        .name("ferrus-cargo-parser-output".to_string())
        .spawn(move || {
            let mut output = Vec::new();
            stdout.read_to_end(&mut output)?;
            Ok(output)
        }) {
        Ok(reader) => reader,
        Err(_) => {
            terminate_and_reap(&mut child);
            let _ = writer.join();
            return ParserDeadline::Unavailable;
        }
    };

    let status = wait_for_child(&mut child, started, budget);
    let output = finish_parser_io(writer, reader);
    match status {
        ChildDeadline::TimedOut => ParserDeadline::TimedOut,
        ChildDeadline::Unavailable => ParserDeadline::Unavailable,
        ChildDeadline::Exited(status) => {
            if !status.success() || started.elapsed() >= budget {
                return if started.elapsed() >= budget {
                    ParserDeadline::TimedOut
                } else {
                    ParserDeadline::Unavailable
                };
            }
            let Some(output) = output else {
                return ParserDeadline::Unavailable;
            };
            match serde_json::from_slice(&output) {
                Ok(parsed) if started.elapsed() < budget => ParserDeadline::Completed(parsed),
                Ok(_) => ParserDeadline::TimedOut,
                Err(_) => ParserDeadline::Unavailable,
            }
        }
    }
}

fn wait_for_child(child: &mut Child, started: Instant, budget: Duration) -> ChildDeadline {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return ChildDeadline::Exited(status),
            Ok(None) => {}
            Err(_) => {
                terminate_and_reap(child);
                return ChildDeadline::Unavailable;
            }
        }
        let remaining = budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            terminate_and_reap(child);
            return ChildDeadline::TimedOut;
        }
        thread::sleep(remaining.min(PARSER_WAIT_POLL_INTERVAL));
    }
}

fn terminate_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn finish_parser_io(
    writer: JoinHandle<io::Result<()>>,
    reader: JoinHandle<io::Result<Vec<u8>>>,
) -> Option<Vec<u8>> {
    writer.join().ok()?.ok()?;
    reader.join().ok()?.ok()
}

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
        let parsed = match run_parser_with_deadline(started, budget, source.to_owned()) {
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

fn extract_dependency_groups(
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

fn extract_workspace_dependencies(
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

fn extract_dependency_table(
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

struct ParsedDependency {
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

fn parse_dependency(
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

fn unresolved_dependency(alias: &str, invalid: bool) -> ParsedDependency {
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

fn normalize_workspace_patterns(manifest_path: &str, patterns: Vec<String>) -> (Vec<String>, bool) {
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

fn normalize_from_manifest(manifest_path: &str, relative: &str) -> Option<String> {
    let normalized = normalize_directory_from_manifest(manifest_path, relative)?;
    (!normalized.is_empty()).then_some(normalized)
}

fn normalize_directory_from_manifest(manifest_path: &str, relative: &str) -> Option<String> {
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

fn file_stem(path: &str) -> Option<String> {
    let file = path.rsplit(['/', '\\']).next()?;
    let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
    (!stem.is_empty()).then(|| stem.to_string())
}

fn insert_string(table: &toml::Table, key: &str, properties: &mut BTreeMap<String, GraphValue>) {
    if let Some(value) = table.get(key).and_then(toml::Value::as_str) {
        properties.insert(key.replace('-', "_"), GraphValue::String(value.to_string()));
    }
}

fn insert_bool(table: &toml::Table, key: &str, properties: &mut BTreeMap<String, GraphValue>) {
    if let Some(value) = table.get(key).and_then(toml::Value::as_bool) {
        properties.insert(key.replace('-', "_"), GraphValue::Boolean(value));
    }
}

fn string_list(value: Option<&toml::Value>) -> Option<Vec<String>> {
    value?
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(ToString::to_string))
        .collect()
}

fn semantic_key(kind: &str, parts: &[&str]) -> String {
    let parts = parts
        .iter()
        .map(|part| escape_component(part))
        .collect::<Vec<_>>()
        .join(":");
    format!("cargo:{kind}:{parts}")
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

fn extractor_identity() -> ExtractorIdentity {
    ExtractorIdentity {
        id: ExtractorId::new(EXTRACTOR_ID).expect("built-in extractor ID is non-empty"),
        version: EXTRACTOR_VERSION.to_string(),
        contract_version: EXTRACTOR_CONTRACT_VERSION,
    }
}

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
mod tests {
    use super::*;
    use crate::repository_graph::{
        domain::{
            BuildId, Digest, RepoPath, RepositoryId, RepositoryNamespace, RepositoryRef, SnapshotId,
        },
        ports::{ExtractionContext, SourceFileMode},
    };

    fn fixture(path: &str, content: &[u8]) -> (ExtractionContext, SourceFileDescriptor) {
        (
            ExtractionContext {
                snapshot_id: SnapshotId::new("snapshot-cargo-test").unwrap(),
                build_id: BuildId::new("build-cargo-test").unwrap(),
                repository: RepositoryRef {
                    namespace: RepositoryNamespace::new("local").unwrap(),
                    repository_id: RepositoryId::new("repo").unwrap(),
                },
                max_facts_per_file: 1_000,
                max_parser_duration_ms: 2_000,
                max_diagnostics: 100,
            },
            SourceFileDescriptor {
                path: RepoPath::new(path).unwrap(),
                content_identity: Digest::new("sha256", "00").unwrap(),
                byte_len: content.len() as u64,
                file_mode: SourceFileMode::Regular,
            },
        )
    }

    fn extract(path: &str, content: &[u8]) -> GraphFragment {
        let (context, file) = fixture(path, content);
        CargoExtractor
            .extract(FileExtractionInput {
                context: &context,
                file: &file,
                content,
            })
            .unwrap()
    }

    fn nodes<'a>(fragment: &'a GraphFragment, kind: &'a str) -> Vec<&'a GraphNode> {
        fragment
            .nodes
            .iter()
            .filter(|node| node.kind == kind)
            .collect()
    }

    #[test]
    fn supports_only_cargo_manifests() {
        let (_, root) = fixture("Cargo.toml", b"");
        let (_, nested) = fixture("crates/core/Cargo.toml", b"");
        let (_, lowercase) = fixture("cargo.toml", b"");
        let (_, other) = fixture("Cargo.lock", b"");
        assert!(CargoExtractor.supports(&root));
        assert!(CargoExtractor.supports(&nested));
        assert!(!CargoExtractor.supports(&lowercase));
        assert!(!CargoExtractor.supports(&other));
    }

    #[test]
    fn extracts_workspace_package_targets_and_entry_points() {
        let source = br#"
[workspace]
members = ["crates/b", "crates/a"]
resolver = "3"

[package]
name = "app-name"
version = "1.2.3"
edition = "2024"
autobins = false

[lib]
crate-type = ["rlib"]

[[bin]]
name = "server"
path = "cmd/server.rs"

[[example]]
name = "demo"
"#;
        let fragment = extract("Cargo.toml", source);
        assert_eq!(nodes(&fragment, "cargo_workspace").len(), 1);
        assert_eq!(nodes(&fragment, "cargo_package").len(), 1);
        assert_eq!(nodes(&fragment, "cargo_target").len(), 4);
        assert_eq!(nodes(&fragment, "entry_point").len(), 4);
        assert_eq!(
            nodes(&fragment, "cargo_workspace")[0]
                .properties
                .get("member_patterns"),
            Some(&GraphValue::StringList(vec![
                "crates/a".to_string(),
                "crates/b".to_string(),
            ]))
        );
        assert!(fragment.edges.iter().any(|edge| edge.kind == "contains"));
        assert_eq!(
            fragment
                .edges
                .iter()
                .filter(|edge| edge.kind == "declares_target")
                .count(),
            4
        );
        let server = nodes(&fragment, "entry_point")
            .into_iter()
            .find(|node| {
                node.properties.get("path")
                    == Some(&GraphValue::String("cmd/server.rs".to_string()))
            })
            .unwrap();
        assert_eq!(server.provenance.resolution, ResolutionState::Unresolved);
        assert!(server.provenance.evidence.as_ref().unwrap().span.is_some());
        assert!(fragment.diagnostics.is_empty());
    }

    #[test]
    fn emits_honest_conventional_candidates() {
        let fragment = extract(
            "crates/app/Cargo.toml",
            br#"[package]
name = "app"
version = "0.1.0"
"#,
        );
        let targets = nodes(&fragment, "cargo_target");
        assert_eq!(targets.len(), 3);
        assert!(targets.iter().all(|target| {
            target.properties.get("origin")
                == Some(&GraphValue::String("conventional_candidate".to_string()))
                && target.provenance.resolution == ResolutionState::Unresolved
        }));
        let paths = nodes(&fragment, "entry_point")
            .into_iter()
            .filter_map(|node| match node.properties.get("path") {
                Some(GraphValue::String(path)) => Some(path.as_str()),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            paths,
            BTreeSet::from([
                "crates/app/build.rs",
                "crates/app/src/lib.rs",
                "crates/app/src/main.rs",
            ])
        );
    }

    #[test]
    fn package_auto_target_switches_are_respected() {
        let fragment = extract(
            "Cargo.toml",
            br#"[package]
name = "app"
autolib = false
autobins = false
build = false
"#,
        );

        assert_eq!(nodes(&fragment, "cargo_package").len(), 1);
        assert!(nodes(&fragment, "cargo_target").is_empty());
        assert!(nodes(&fragment, "entry_point").is_empty());
    }

    #[test]
    fn custom_build_entry_points_are_described_but_never_executed() {
        let fragment = extract(
            "crates/app/Cargo.toml",
            br#"[package]
name = "app"
autolib = false
autobins = false
build = "tools/build.rs"
"#,
        );

        let targets = nodes(&fragment, "cargo_target");
        assert_eq!(targets.len(), 1);
        assert_eq!(
            targets[0].properties.get("target_kind"),
            Some(&GraphValue::String("custom_build".to_string()))
        );
        assert!(nodes(&fragment, "entry_point").iter().any(|node| {
            node.properties.get("path")
                == Some(&GraphValue::String("crates/app/tools/build.rs".to_string()))
        }));
    }

    #[test]
    fn explicit_bins_infer_cargo_conventional_paths_by_name() {
        let fragment = extract(
            "Cargo.toml",
            br#"[package]
name = "app"
autobins = false

[[bin]]
name = "app"

[[bin]]
name = "worker"
"#,
        );
        let paths = nodes(&fragment, "entry_point")
            .into_iter()
            .filter_map(|node| match node.properties.get("path") {
                Some(GraphValue::String(path)) => Some(path.as_str()),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        assert!(paths.contains("src/main.rs"));
        assert!(paths.contains("src/bin/worker.rs"));
    }

    #[test]
    fn classifies_dependency_sources_without_resolving_internal_candidates() {
        let fragment = extract(
            "crates/app/Cargo.toml",
            br#"[package]
name = "app"

[dependencies]
serde = "1"
renamed = { package = "actual", version = "2", registry = "private" }
core = { path = "../core", features = ["z", "a", "a"] }
shared = { workspace = true }
mystery = {}

[target.'cfg(unix)'.dev-dependencies]
tempfile = "3"

[build-dependencies]
build-helper = { git = "https://user:secret@example.invalid/repo" }
"#,
        );
        let dependencies = nodes(&fragment, "declared_dependency");
        assert_eq!(dependencies.len(), 7);
        let classification = dependencies
            .iter()
            .map(|node| {
                let GraphValue::String(alias) = &node.properties["alias"] else {
                    panic!("alias must be a string")
                };
                let GraphValue::String(classification) = &node.properties["classification"] else {
                    panic!("classification must be a string")
                };
                (alias.as_str(), classification.as_str())
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(classification["serde"], "external");
        assert_eq!(classification["renamed"], "external");
        assert_eq!(classification["core"], "internal_candidate");
        assert_eq!(classification["shared"], "workspace_unresolved");
        assert_eq!(classification["mystery"], "unresolved");
        let relations = fragment
            .edges
            .iter()
            .filter(|edge| edge.kind == "depends_on")
            .collect::<Vec<_>>();
        assert!(relations.iter().any(|edge| {
            matches!(&edge.target, EdgeTarget::Unresolved(target) if target.contains("crates/core/Cargo.toml"))
        }));
        assert!(relations.iter().any(|edge| {
            edge.provenance.resolution == ResolutionState::External
                && matches!(edge.target, EdgeTarget::External(_))
        }));
        assert!(!format!("{fragment:?}").contains("user:secret"));
        assert!(dependencies.iter().any(|node| {
            node.properties.get("scope") == Some(&GraphValue::String("dev".to_string()))
                && node.properties.get("target_condition")
                    == Some(&GraphValue::String("cfg(unix)".to_string()))
        }));
    }

    #[test]
    fn virtual_workspace_dependencies_are_emitted() {
        let fragment = extract(
            "Cargo.toml",
            br#"[workspace]
members = ["crates/*"]

[workspace.dependencies]
serde = "1"
local = { path = "crates/local" }
"#,
        );
        assert!(nodes(&fragment, "cargo_package").is_empty());
        assert_eq!(nodes(&fragment, "declared_dependency").len(), 2);
        assert!(nodes(&fragment, "declared_dependency").iter().all(|node| {
            node.properties.get("scope") == Some(&GraphValue::String("workspace".to_string()))
        }));
    }

    #[test]
    fn root_relative_directory_candidates_preserve_the_repository_root() {
        let fragment = extract(
            "Cargo.toml",
            br#"[workspace]
members = ["."]

[package]
name = "root"

[dependencies]
self-path = { path = "." }
"#,
        );
        assert_eq!(
            nodes(&fragment, "cargo_workspace")[0]
                .properties
                .get("member_patterns"),
            Some(&GraphValue::StringList(vec![".".to_string()]))
        );
        assert!(fragment.edges.iter().any(|edge| {
            matches!(&edge.target, EdgeTarget::Unresolved(target) if target == "cargo-package-path:Cargo.toml")
        }));
        assert!(fragment.diagnostics.iter().all(|diagnostic| {
            diagnostic.code.as_str() != "cargo.dependency_path_outside_repository"
                && diagnostic.code.as_str() != "cargo.workspace_path_outside_repository"
        }));
    }

    #[test]
    fn malformed_and_semantically_invalid_manifests_are_diagnostics() {
        let malformed = extract("Cargo.toml", b"[package\nname = 'oops'");
        assert!(malformed.nodes.is_empty());
        assert_eq!(
            malformed.diagnostics[0].code.as_str(),
            "cargo.malformed_manifest"
        );
        assert!(
            malformed.diagnostics[0]
                .location
                .as_ref()
                .unwrap()
                .span
                .is_some()
        );

        let invalid = extract(
            "Cargo.toml",
            br#"package = "not-a-table"
dependencies = ["not-a-table"]
"#,
        );
        assert!(invalid.nodes.is_empty());
        assert!(
            invalid
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code.as_str() == "cargo.invalid_package_table")
        );
    }

    #[test]
    fn outside_paths_are_not_persisted_as_repository_candidates() {
        let fragment = extract(
            "Cargo.toml",
            br#"[package]
name = "app"

[[bin]]
name = "outside"
path = "../secret/main.rs"

[dependencies]
outside = { path = "../secret" }
"#,
        );
        let debug = format!("{fragment:?}");
        assert!(!debug.contains("secret/main.rs"));
        assert!(fragment.diagnostics.iter().any(|diagnostic| {
            diagnostic.code.as_str() == "cargo.target_path_outside_repository"
        }));
        assert!(fragment.diagnostics.iter().any(|diagnostic| {
            diagnostic.code.as_str() == "cargo.dependency_path_outside_repository"
        }));
    }

    #[test]
    fn fact_and_diagnostic_budgets_are_hard_bounded() {
        let content = br#"[package]
name = "app"

[dependencies]
a = []
b = []
c = []
"#;
        let (mut context, file) = fixture("Cargo.toml", content);
        context.max_facts_per_file = 2;
        context.max_diagnostics = 1;
        let fragment = CargoExtractor
            .extract(FileExtractionInput {
                context: &context,
                file: &file,
                content,
            })
            .unwrap();
        assert!(fragment.nodes.len() + fragment.edges.len() <= 2);
        assert_eq!(fragment.diagnostics.len(), 1);
        assert_eq!(
            fragment.diagnostics[0].code.as_str(),
            "cargo.diagnostics_truncated"
        );
    }

    #[test]
    fn duplicate_target_declarations_do_not_duplicate_ids_or_consume_facts() {
        let content = br#"[package]
name = "app"
autobins = false
build = false

[lib]
path = "src/lib.rs"

[[bin]]
name = "tool"
path = "src/tool.rs"

[[bin]]
name = "tool"
path = "src/tool.rs"
"#;
        let (mut context, file) = fixture("Cargo.toml", content);
        context.max_facts_per_file = 9;
        let fragment = CargoExtractor
            .extract(FileExtractionInput {
                context: &context,
                file: &file,
                content,
            })
            .unwrap();
        assert_eq!(fragment.nodes.len() + fragment.edges.len(), 9);
        assert_eq!(
            fragment
                .edges
                .iter()
                .map(|edge| edge.id.clone())
                .collect::<BTreeSet<_>>()
                .len(),
            fragment.edges.len()
        );
        assert!(
            fragment
                .diagnostics
                .iter()
                .all(|diagnostic| diagnostic.code.as_str() != "cargo.fact_limit")
        );
    }

    #[test]
    fn dependency_field_types_are_validated() {
        let fragment = extract(
            "Cargo.toml",
            br#"[package]
name = "app"

[dependencies]
bad-package = { version = "1", package = 7 }
bad-registry = { version = "1", registry = false }
bad-optional = { version = "1", optional = "yes" }
bad-default = { version = "1", default-features = "yes" }
bad-features = { version = "1", features = ["ok", 7] }
bad-registry-source = { registry = "private" }
bad-workspace-version = { workspace = true, version = "1" }
bad-mixed-sources = { path = "local", registry = "private", version = "1" }
"#,
        );
        assert_eq!(nodes(&fragment, "declared_dependency").len(), 8);
        assert_eq!(
            fragment
                .diagnostics
                .iter()
                .filter(|diagnostic| {
                    diagnostic.code.as_str() == "cargo.invalid_dependency_declaration"
                })
                .count(),
            8
        );
        assert!(
            fragment
                .edges
                .iter()
                .filter(|edge| edge.kind == "depends_on")
                .all(|edge| edge.provenance.resolution == ResolutionState::Unresolved)
        );
    }

    #[test]
    fn source_derived_values_are_bounded_before_persistence() {
        let huge_version = "x".repeat(MAX_PROPERTY_STRING_BYTES + 1);
        let content = format!("[package]\nname = 'app'\nversion = '{huge_version}'\n");
        let fragment = extract("Cargo.toml", content.as_bytes());
        let package = nodes(&fragment, "cargo_package")[0];
        assert!(!package.properties.contains_key("version"));
        assert!(
            fragment
                .diagnostics
                .iter()
                .any(|diagnostic| { diagnostic.code.as_str() == "cargo.source_value_limit" })
        );
        assert!(!format!("{fragment:?}").contains(&huge_version));
    }

    #[test]
    fn fact_traversal_observes_the_parser_deadline() {
        let content = b"[package]\nname = 'app'\n";
        let (context, file) = fixture("Cargo.toml", content);
        let input = FileExtractionInput {
            context: &context,
            file: &file,
            content,
        };
        let mut diagnostics = DiagnosticBuffer::new(input);
        let mut facts = FactBuffer::new(
            input,
            &mut diagnostics,
            Instant::now(),
            Duration::from_secs(1),
        );
        assert!(
            facts
                .node(
                    "cargo_package",
                    "cargo:package:test",
                    None,
                    ResolutionState::Resolved,
                    Confidence::Exact,
                    BTreeMap::new(),
                )
                .is_some()
        );
        facts.started = Instant::now() - Duration::from_millis(2);
        facts.budget = Duration::from_millis(1);
        assert!(!facts.active());
        facts.finish();
        assert!(facts.nodes.is_empty());
        assert!(facts.edges.is_empty());
        drop(facts);
        let diagnostics = diagnostics.finish();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code.as_str(), "cargo.parser_timeout");
    }

    #[test]
    fn parser_deadline_kills_and_reaps_a_blocked_worker_process() {
        const CHILD_TEST: &str =
            "repository_graph::extractors::cargo::tests::parser_deadline_blocked_child";
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CHILD_TEST, "--nocapture"])
            .env("FERRUS_CARGO_PARSER_BLOCK_TEST", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let started = Instant::now();

        let result = wait_for_child(&mut child, started, Duration::from_millis(50));

        assert!(matches!(result, ChildDeadline::TimedOut));
        assert!(child.try_wait().unwrap().is_some());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn parser_deadline_blocked_child() {
        if std::env::var_os("FERRUS_CARGO_PARSER_BLOCK_TEST").is_some() {
            thread::sleep(Duration::from_secs(60));
        }
    }

    #[test]
    fn parser_worker_protocol_preserves_manifest_values_and_error_spans() {
        let ParserOutput::Parsed { manifest } = parse_manifest("date = 2026-07-17\ninteger = 7\n")
        else {
            panic!("valid manifest was rejected");
        };
        let wire = serde_json::to_vec(&ParserOutput::Parsed { manifest }).unwrap();
        let ParserOutput::Parsed { manifest } = serde_json::from_slice(&wire).unwrap() else {
            panic!("valid parser response changed variants");
        };
        assert!(manifest["date"].is_datetime());
        assert_eq!(manifest["integer"].as_integer(), Some(7));

        let ParserOutput::Malformed { span } = parse_manifest("[package\n") else {
            panic!("invalid manifest was accepted");
        };
        assert!(span.is_some());
    }

    #[test]
    fn output_is_deterministic_and_spans_are_half_open_one_based() {
        let content = b"\n  [package]\nname = 'app'\n";
        let first = extract("Cargo.toml", content);
        let second = extract("Cargo.toml", content);
        assert_eq!(
            serde_json::to_value(&first.nodes).unwrap(),
            serde_json::to_value(&second.nodes).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&first.edges).unwrap(),
            serde_json::to_value(&second.edges).unwrap()
        );
        let span = first
            .nodes
            .iter()
            .find_map(|node| node.provenance.evidence.as_ref()?.span.as_ref())
            .expect("the package declaration has source evidence");
        assert_eq!(span.start.line, Some(2));
        assert_eq!(span.start.column, Some(3));
        assert!(span.end.byte_offset > span.start.byte_offset);
    }
}
