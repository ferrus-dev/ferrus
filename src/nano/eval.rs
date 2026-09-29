//! Offline evaluation of pinned headless Executor attempts.
//! No provider, task, graph, or workspace operation is performed here.

use super::{
    replay::Replay,
    session::{EndReason, GraphPeerMode, Record, SessionEvent, effective_settings_sha256},
    tools::ToolOutcome,
};
use crate::{
    config::Config,
    project::{self, CanonicalGraphStatus},
    repository_graph::config::RepositoryGraphConfig,
    repository_graph::domain::{Availability, Freshness},
    repository_graph_runtime::LocalGraphContext,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MANIFEST_BYTES: u64 = 256 * 1024;
const JOURNAL_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum Variant {
    ExternalMcp,
    NanoMcp,
    NanoNative,
    NanoWorkingSet,
    NanoGraphDisabled,
}

impl Variant {
    fn needs_journal(&self) -> bool {
        !matches!(self, Self::ExternalMcp)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum CacheState {
    Cold,
    Warm,
    Disabled,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    attempts: Vec<Attempt>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Attempt {
    case_id: String,
    variant: Variant,
    cache: CacheState,
    sample: u32,
    start_tree: String,
    model: String,
    settings_sha256: String,
    native_context_enabled: Option<bool>,
    working_set_enabled: Option<bool>,
    database: PathBuf,
    task_id: String,
    run_id: String,
    journal: Option<PathBuf>,
    external_usage: Option<ExternalUsage>,
    timing: Timing,
    harness_notes: Vec<String>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalUsage {
    source: String,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    cost_usd: Option<f64>,
    model_turns: Option<u64>,
    tool_calls: Option<u64>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Timing {
    source: String,
    total_ms: u64,
    index_ms: Option<u64>,
    refresh_ms: Option<u64>,
    peak_rss_bytes: Option<u64>,
}

#[derive(Default, Serialize)]
struct Metrics {
    input_tokens_reported: Option<u64>,
    output_tokens_reported: Option<u64>,
    input_tokens_estimated: Option<u64>,
    output_tokens_estimated: Option<u64>,
    cached_tokens: Option<u64>,
    cost_usd: Option<f64>,
    usage_source: Option<String>,
    model_turns: Option<u64>,
    tool_calls: Option<u64>,
    duplicate_source_bytes: Option<u64>,
    graph_tool_calls: Option<u64>,
    fallback_calls: Option<u64>,
    stale_events: Option<u64>,
    context_assembly_ms: Option<Distribution>,
    tool_latency_ms: Option<Distribution>,
}

#[derive(Default, Serialize)]
struct Distribution {
    samples: usize,
    min: u64,
    p50: u64,
    p95: u64,
    max: u64,
}

impl Distribution {
    fn from_samples(mut values: Vec<u64>) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        values.sort_unstable();
        let rank = |percent: usize| (values.len() * percent).div_ceil(100).saturating_sub(1);
        Some(Self {
            samples: values.len(),
            min: values[0],
            p50: values[rank(50)],
            p95: values[rank(95)],
            max: *values.last().unwrap(),
        })
    }
}

#[derive(Serialize)]
struct ResultRow {
    case_id: String,
    variant: Variant,
    cache: CacheState,
    sample: u32,
    start_tree: String,
    model: String,
    settings_sha256: String,
    native_context_enabled: Option<bool>,
    working_set_enabled: Option<bool>,
    task_status: String,
    run_status: String,
    review_cycles: u32,
    check_passed_events: u64,
    final_check_gate_passed: bool,
    approved_events: u64,
    accepted: bool,
    nano_end_reason: Option<EndReason>,
    timing: Timing,
    metrics: Metrics,
    harness_notes: Vec<String>,
}

#[derive(Serialize)]
struct Group {
    case_id: String,
    variant: Variant,
    cache: CacheState,
    model: String,
    settings_sha256: String,
    samples: usize,
    accepted: usize,
    total_ms: Distribution,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct GroupKey {
    case_id: String,
    variant: Variant,
    cache: CacheState,
    model: String,
    settings_sha256: String,
}

#[derive(Default)]
struct GroupCount {
    accepted: usize,
    times: Vec<u64>,
}

#[derive(Serialize)]
struct Report {
    version: u32,
    suite: &'static str,
    attempts: Vec<ResultRow>,
    groups: Vec<Group>,
    comparability_notes: Vec<&'static str>,
}

#[derive(Deserialize)]
struct Suite {
    version: u32,
    cases: BTreeMap<String, String>,
    workloads: BTreeMap<String, Workload>,
}

#[derive(Deserialize)]
struct Workload {
    task: String,
    checks: Vec<String>,
    #[serde(default)]
    windows_checks: Option<Vec<String>>,
}

#[derive(Deserialize, Serialize)]
struct LaunchWorkloadEvidence {
    case_id: String,
    task_sha256: String,
    check_commands_sha256: String,
    cache: Option<CacheState>,
    graph_snapshot_id: Option<String>,
}

fn suite() -> Result<Suite> {
    let suite: Suite =
        serde_json::from_str(include_str!("../../tests/fixtures/nano_eval/suite.json"))?;
    ensure!(
        suite.version == 2
            && suite.cases.len() == 7
            && suite.cases.keys().eq(suite.workloads.keys())
            && suite.workloads.values().all(|workload| {
                !workload.task.is_empty()
                    && !workload.checks.is_empty()
                    && workload
                        .windows_checks
                        .as_ref()
                        .is_none_or(|checks| !checks.is_empty())
            }),
        "Invalid pinned suite"
    );
    Ok(suite)
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn task_sha256(workload: &Workload) -> String {
    sha256(format!("{}\n", workload.task).as_bytes())
}

fn expected_check_digests(workload: &Workload) -> Result<Vec<String>> {
    let mut digests = vec![crate::checks::commands_sha256(&workload.checks)?];
    if let Some(windows) = &workload.windows_checks {
        digests.push(crate::checks::commands_sha256(windows)?);
    }
    Ok(digests)
}

fn launch_cache(
    enabled: bool,
    status: CanonicalGraphStatus,
    has_snapshot: bool,
) -> Option<CacheState> {
    if !enabled {
        return Some(CacheState::Disabled);
    }
    match (status, has_snapshot) {
        (CanonicalGraphStatus::Unknown, false) => Some(CacheState::Cold),
        (CanonicalGraphStatus::Fresh, true) => Some(CacheState::Warm),
        _ => None,
    }
}

pub(crate) async fn capture_launch(
    case_id: &str,
    project_root: &Path,
    task_id: &str,
) -> Result<serde_json::Value> {
    let suite = suite()?;
    ensure!(suite.cases.contains_key(case_id), "Unknown evaluation case");
    ensure!(
        tokio::fs::canonicalize(project_root).await? == project::canonical_project_root().await?,
        "Evaluation project root does not match HQ project"
    );
    ensure!(
        !task_id.is_empty()
            && task_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "Invalid evaluation task ID"
    );
    let task_path = project_root.join(format!(".ferrus/tasks/{task_id}.md"));
    let task = bounded_file(&task_path, 64 * 1024)?;
    let config = Config::load_from(project_root).await?;
    let graph_toml = tokio::fs::read_to_string(project_root.join("ferrus.toml")).await?;
    let graph = RepositoryGraphConfig::from_ferrus_toml(&graph_toml)?;
    let reference = project::canonical_graph_reference().await?;
    let sidecar = if graph.enabled {
        Some(
            LocalGraphContext::load(false)
                .await?
                .status_with_freshness_comparison()
                .await?,
        )
    } else {
        None
    };
    let graph_snapshot_id = if graph.enabled {
        reference
            .snapshot_id
            .as_ref()
            .map(|id| id.as_str().to_owned())
    } else {
        None
    };
    let mut cache = launch_cache(graph.enabled, reference.status, graph_snapshot_id.is_some());
    if let Some(sidecar) = sidecar {
        let consistent = match cache {
            Some(CacheState::Cold) => {
                sidecar.data.availability == Availability::NotBuilt && sidecar.snapshot_id.is_none()
            }
            Some(CacheState::Warm) => {
                sidecar.data.availability == Availability::Available
                    && sidecar.snapshot_id == reference.snapshot_id
                    && sidecar.freshness.freshness == Freshness::Fresh
            }
            _ => false,
        };
        if !consistent {
            cache = None;
        }
    }
    Ok(serde_json::to_value(LaunchWorkloadEvidence {
        case_id: case_id.to_owned(),
        task_sha256: sha256(&task),
        check_commands_sha256: crate::checks::commands_sha256(&config.checks.commands)?,
        cache,
        graph_snapshot_id,
    })?)
}

fn bounded_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("Open {}", path.display()))?;
    ensure!(
        file.metadata()?.len() <= limit,
        "Evaluation input exceeds byte limit"
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "Evaluation input exceeds byte limit"
    );
    Ok(bytes)
}

fn digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn stale_observation(value: &serde_json::Value) -> bool {
    ["kind", "reason", "code"].into_iter().any(|field| {
        value[field]
            .as_str()
            .is_some_and(|code| code.contains("stale") || code.contains("source_changed"))
    })
}

fn task_state(
    attempt: &Attempt,
    workload: &Workload,
) -> Result<(String, String, u32, u64, bool, u64)> {
    let db = Connection::open_with_flags(&attempt.database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let baseline: String = db.query_row(
        "SELECT payload_json FROM events WHERE type = 'run_started' AND run_id = ?1",
        [&attempt.run_id],
        |row| row.get(0),
    )?;
    ensure!(baseline.len() <= 8192, "Run start payload exceeds limit");
    let baseline: serde_json::Value = serde_json::from_str(&baseline)?;
    ensure!(
        baseline["baseline_tree"].as_str() == Some(attempt.start_tree.as_str()),
        "Run baseline does not match pinned case tree"
    );
    let launch: LaunchWorkloadEvidence = serde_json::from_value(baseline["evaluation"].clone())
        .context("Run lacks evaluation launch evidence")?;
    let expected_checks = expected_check_digests(workload)?;
    ensure!(
        launch.case_id == attempt.case_id
            && launch.task_sha256 == task_sha256(workload)
            && expected_checks.contains(&launch.check_commands_sha256),
        "Run task or check configuration does not match pinned workload"
    );
    ensure!(
        launch.cache.as_ref() == Some(&attempt.cache)
            && (matches!(&attempt.cache, CacheState::Warm) == launch.graph_snapshot_id.is_some()),
        "Run cache condition does not match manifest"
    );
    let (task_status, cycles): (String, u32) = db.query_row(
        "SELECT status, review_cycles FROM tasks WHERE id = ?1",
        [&attempt.task_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let run_status: String = db.query_row(
        "SELECT status FROM runs WHERE id = ?1 AND task_id = ?2 AND lower(role) = 'executor'",
        params![attempt.run_id, attempt.task_id],
        |row| row.get(0),
    )?;
    let other_executor_runs: i64 = db.query_row(
        "SELECT count(*) FROM runs WHERE task_id = ?1 AND id != ?2 AND lower(role) = 'executor'",
        params![attempt.task_id, attempt.run_id],
        |row| row.get(0),
    )?;
    ensure!(
        other_executor_runs == 0,
        "Evaluation task has multiple Executor runs"
    );
    let count_run = |kind: &str| -> Result<u64> {
        let count: i64 = db.query_row(
            "SELECT count(*) FROM events WHERE type = ?1 AND run_id = ?2",
            params![kind, attempt.run_id],
            |row| row.get(0),
        )?;
        Ok(count.try_into()?)
    };
    let approvals: i64 = db.query_row(
        "SELECT count(*) FROM events WHERE type = 'approved' AND run_id IN
         (SELECT id FROM runs WHERE task_id = ?1)",
        [&attempt.task_id],
        |row| row.get(0),
    )?;
    let final_submission: Option<(String, String)> = db
        .query_row(
            "SELECT e.run_id, e.payload_json FROM events e
             JOIN runs r ON r.id = e.run_id
             WHERE r.task_id = ?1 AND e.type = 'submission_committed'
             ORDER BY e.id DESC LIMIT 1",
            [&attempt.task_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let final_gate_passed = if let Some((run_id, payload)) = &final_submission {
        ensure!(payload.len() <= 8192, "Submit event payload exceeds limit");
        let value: serde_json::Value = serde_json::from_str(payload)?;
        ensure!(
            value["check_commands_sha256"]
                .as_str()
                .is_some_and(|digest| expected_checks.iter().any(|expected| expected == digest)),
            "Submitted check commands do not match pinned workload"
        );
        run_id == &attempt.run_id
            && value["task_id"] == attempt.task_id
            && value["review_cycles"].as_u64() == Some(u64::from(cycles))
            && value["check_gate"] == "passed"
    } else {
        false
    };
    Ok((
        task_status,
        run_status,
        cycles,
        count_run("check_passed")?,
        final_gate_passed,
        approvals.try_into()?,
    ))
}

fn is_graph_call(name: &str, arguments: &str, graph_peer_id: Option<&str>) -> bool {
    if matches!(
        name,
        "repository_graph_status" | "repository_search" | "repository_context"
    ) {
        return true;
    }
    if let Some(id) = graph_peer_id {
        let prefix = format!("mcp_{id}_");
        if let Some(tool) = name.strip_prefix(&prefix)
            && matches!(
                tool,
                "repository_graph_status" | "repository_search" | "repository_context"
            )
        {
            return true;
        }
    }
    matches!(name, "project_context_search" | "project_context")
        && serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|args| args["domain"].as_str().map(str::to_owned))
            .is_some_and(|domain| matches!(domain.as_str(), "repository" | "all"))
}

fn nano_metrics(attempt: &Attempt) -> Result<(Metrics, Option<EndReason>)> {
    let path = attempt
        .journal
        .as_ref()
        .context("Nano attempt requires a journal")?;
    let bytes = bounded_file(path, JOURNAL_BYTES)?;
    ensure!(
        bytes.last() == Some(&b'\n'),
        "Incomplete evaluation journal"
    );
    let records: Vec<Record> = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(serde_json::from_slice)
        .collect::<serde_json::Result<_>>()?;
    let replay = Replay::from_records(&records)?;
    ensure!(replay.session_id == attempt.run_id, "Journal/run mismatch");
    ensure!(
        matches!(records.first().map(|record| &record.event),
        Some(SessionEvent::Started { identity, .. })
            if identity.task_id.as_deref() == Some(&attempt.task_id)
                && identity.run_id.as_deref() == Some(&attempt.run_id)),
        "Journal/task mismatch"
    );
    let (inherited, graph_peer_id) = match &records[0].event {
        SessionEvent::Started {
            inherited_budget,
            launch_evidence,
            provider,
            limits,
            ..
        } => {
            let evidence = launch_evidence
                .as_ref()
                .context("Nano journal lacks launch evidence")?;
            ensure!(
                evidence.baseline_tree == attempt.start_tree,
                "Nano journal baseline does not match pinned case tree"
            );
            ensure!(
                (
                    Some(evidence.native_context_enabled),
                    Some(evidence.working_set_enabled)
                ) == (attempt.native_context_enabled, attempt.working_set_enabled),
                "Nano journal ablation flags do not match manifest"
            );
            let expected_peer_mode = if matches!(attempt.variant, Variant::NanoMcp) {
                GraphPeerMode::Complete
            } else {
                GraphPeerMode::Absent
            };
            ensure!(
                evidence.graph_peer_mode == Some(expected_peer_mode),
                "Nano journal graph peer mode does not match variant"
            );
            // Complete identifies the three graph tools, while these counts
            // exclude every additional peer or advertised tool.
            let expected_catalog = if matches!(attempt.variant, Variant::NanoMcp) {
                (Some(1), Some(3))
            } else {
                (Some(0), Some(0))
            };
            ensure!(
                (evidence.mcp_peer_count, evidence.mcp_tool_count) == expected_catalog,
                "Nano journal MCP catalog does not match variant"
            );
            ensure!(
                match expected_peer_mode {
                    GraphPeerMode::Complete =>
                        evidence.graph_peer_id.as_deref().is_some_and(|id| {
                            !id.is_empty()
                                && id.len() <= 24
                                && id.bytes().all(|byte| {
                                    byte.is_ascii_lowercase()
                                        || byte.is_ascii_digit()
                                        || byte == b'_'
                                })
                        }),
                    GraphPeerMode::Absent => evidence.graph_peer_id.is_none(),
                    GraphPeerMode::Partial => false,
                },
                "Nano journal graph peer ID does not match variant"
            );
            ensure!(
                if matches!(attempt.variant, Variant::NanoMcp) {
                    evidence
                        .graph_peer_timeout_ms
                        .is_some_and(|timeout| (100..=120_000).contains(&timeout))
                } else {
                    evidence.graph_peer_timeout_ms.is_none()
                },
                "Nano journal graph peer timeout does not match variant"
            );
            let provider = provider
                .as_ref()
                .context("Nano journal lacks provider settings")?;
            ensure!(
                provider.model == attempt.model,
                "Nano journal model does not match manifest"
            );
            let settings_sha256 = effective_settings_sha256(provider, limits, evidence)?;
            ensure!(
                evidence.settings_sha256.as_deref() == Some(settings_sha256.as_str()),
                "Nano journal settings digest does not match effective settings"
            );
            ensure!(
                attempt.settings_sha256 == settings_sha256,
                "Nano settings digest does not match manifest"
            );
            (
                inherited_budget.clone().unwrap_or_default(),
                evidence.graph_peer_id.clone(),
            )
        }
        _ => unreachable!("Replay accepted only a Started first record"),
    };

    let mut fallback_calls = 0;
    let mut graph_tool_calls = 0;
    let mut stale_events = 0;
    let mut duplicate_source_bytes = 0;
    let mut seen_reads = BTreeSet::new();
    let mut calls = BTreeMap::new();
    let mut active_tools = BTreeMap::new();
    let mut tool_latencies = Vec::new();
    let mut context_latencies = Vec::new();
    for record in &records {
        match &record.event {
            SessionEvent::ToolIntent { call_id, call, .. } => {
                if call.name == "repository_fallback" {
                    fallback_calls += 1;
                }
                if is_graph_call(&call.name, &call.arguments, graph_peer_id.as_deref()) {
                    graph_tool_calls += 1;
                }
                calls.insert(call_id.clone(), call.name.as_str());
            }
            SessionEvent::ToolStarted { call_id } => {
                active_tools.insert(call_id.clone(), record.budget.elapsed_ms);
            }
            SessionEvent::ToolResult { call_id, outcome } => {
                if let Some(start) = active_tools.remove(call_id) {
                    tool_latencies.push(record.budget.elapsed_ms.saturating_sub(start));
                }
                if calls.get(call_id) == Some(&"read_file")
                    && let ToolOutcome::Success(value) = outcome
                    && let Some(text) = value["text"].as_str()
                {
                    let key = Sha256::digest(serde_json::to_vec(&(
                        &value["source"]["path"],
                        &value["start_line"],
                        text,
                    ))?);
                    if !seen_reads.insert(key) {
                        duplicate_source_bytes += text.len() as u64;
                    }
                }
            }
            SessionEvent::ContextPrepared { preparation } => {
                stale_events += preparation
                    .observations
                    .iter()
                    .filter(|item| stale_observation(item))
                    .count() as u64;
            }
            SessionEvent::ContextComposed { composition } => {
                if let Some(elapsed) = composition.assembly_elapsed_ms {
                    context_latencies.push(elapsed);
                }
            }
            _ => (),
        }
    }
    let budget = &replay.budget;
    let metrics = Metrics {
        input_tokens_reported: Some(budget.reported_input_tokens - inherited.reported_input_tokens),
        output_tokens_reported: Some(
            budget.reported_output_tokens - inherited.reported_output_tokens,
        ),
        input_tokens_estimated: Some(
            budget.estimated_input_tokens - inherited.estimated_input_tokens,
        ),
        output_tokens_estimated: Some(
            budget.estimated_output_tokens - inherited.estimated_output_tokens,
        ),
        usage_source: Some("nano_journal".into()),
        model_turns: Some(budget.model_turns - inherited.model_turns),
        tool_calls: Some(budget.tool_calls - inherited.tool_calls),
        duplicate_source_bytes: Some(duplicate_source_bytes),
        graph_tool_calls: Some(graph_tool_calls),
        fallback_calls: Some(fallback_calls),
        stale_events: Some(stale_events),
        context_assembly_ms: Distribution::from_samples(context_latencies),
        tool_latency_ms: Distribution::from_samples(tool_latencies),
        ..Default::default()
    };
    Ok((metrics, replay.end))
}

fn external_metrics(attempt: &Attempt) -> Result<Metrics> {
    ensure!(
        attempt.journal.is_none(),
        "External attempt cannot have a Nano journal"
    );
    let mut metrics = Metrics::default();
    if let Some(usage) = &attempt.external_usage {
        ensure!(
            !usage.source.trim().is_empty(),
            "External usage requires provenance"
        );
        if let Some(cost) = usage.cost_usd {
            ensure!(cost.is_finite() && cost >= 0.0, "Invalid provider cost");
        }
        metrics.input_tokens_reported = usage.input_tokens;
        metrics.output_tokens_reported = usage.output_tokens;
        metrics.cached_tokens = usage.cached_tokens;
        metrics.cost_usd = usage.cost_usd;
        metrics.model_turns = usage.model_turns;
        metrics.tool_calls = usage.tool_calls;
        metrics.usage_source = Some(usage.source.clone());
    }
    Ok(metrics)
}

fn build(manifest: Manifest) -> Result<Report> {
    ensure!(
        manifest.version == 1,
        "Unsupported evaluation manifest version"
    );
    ensure!(
        !manifest.attempts.is_empty() && manifest.attempts.len() <= 500,
        "Expected 1..500 attempts"
    );
    let suite = suite()?;
    let mut seen = BTreeSet::new();
    let mut seen_databases = BTreeSet::new();
    let mut rows = Vec::new();
    let mut groups: BTreeMap<GroupKey, GroupCount> = BTreeMap::new();
    for attempt in manifest.attempts {
        ensure!(attempt.sample > 0, "Sample indices start at one");
        ensure!(
            suite.cases.get(&attempt.case_id) == Some(&attempt.start_tree),
            "Unpinned case/tree pair"
        );
        ensure!(
            !attempt.model.trim().is_empty() && digest(&attempt.settings_sha256),
            "Missing model or settings digest"
        );
        ensure!(
            attempt.harness_notes.len() <= 16
                && attempt.harness_notes.iter().all(|n| n.len() <= 512),
            "Harness notes exceed limit"
        );
        ensure!(
            !attempt.timing.source.trim().is_empty()
                && attempt.timing.source.len() <= 128
                && attempt.timing.total_ms > 0,
            "Missing timing provenance or elapsed time"
        );
        ensure!(
            matches!(attempt.variant, Variant::NanoGraphDisabled)
                == matches!(attempt.cache, CacheState::Disabled),
            "Graph-disabled variant and disabled cache state must agree"
        );
        let group_key = GroupKey {
            case_id: attempt.case_id.clone(),
            variant: attempt.variant.clone(),
            cache: attempt.cache.clone(),
            model: attempt.model.clone(),
            settings_sha256: attempt.settings_sha256.clone(),
        };
        ensure!(
            seen.insert((group_key.clone(), attempt.sample)),
            "Duplicate evaluation sample"
        );
        let database = std::fs::canonicalize(&attempt.database)?;
        ensure!(
            seen_databases.insert(database.clone()),
            "Evaluation database reused across samples"
        );
        ensure!(
            attempt.variant.needs_journal() == attempt.journal.is_some(),
            "Variant/journal mismatch"
        );
        ensure!(
            !attempt.variant.needs_journal() || attempt.external_usage.is_none(),
            "Nano usage must come from its journal"
        );
        let expected_flags = match attempt.variant {
            Variant::ExternalMcp => (None, None),
            Variant::NanoMcp | Variant::NanoGraphDisabled => (Some(false), Some(false)),
            Variant::NanoNative => (Some(true), Some(false)),
            Variant::NanoWorkingSet => (Some(true), Some(true)),
        };
        ensure!(
            (attempt.native_context_enabled, attempt.working_set_enabled) == expected_flags,
            "Variant/ablation flags mismatch"
        );
        let workload = suite
            .workloads
            .get(&attempt.case_id)
            .context("Missing case workload")?;
        let (task_status, run_status, review_cycles, checks, final_gate_passed, approvals) =
            task_state(&attempt, workload)?;
        let (metrics, nano_end_reason) = if attempt.variant.needs_journal() {
            nano_metrics(&attempt)?
        } else {
            (external_metrics(&attempt)?, None)
        };
        let accepted = task_status == "complete"
            && final_gate_passed
            && (!attempt.variant.needs_journal() || nano_end_reason == Some(EndReason::Submitted));
        let entry = groups.entry(group_key).or_default();
        entry.accepted += usize::from(accepted);
        entry.times.push(attempt.timing.total_ms);
        rows.push(ResultRow {
            case_id: attempt.case_id,
            variant: attempt.variant,
            cache: attempt.cache,
            sample: attempt.sample,
            start_tree: attempt.start_tree,
            model: attempt.model,
            settings_sha256: attempt.settings_sha256,
            native_context_enabled: attempt.native_context_enabled,
            working_set_enabled: attempt.working_set_enabled,
            task_status,
            run_status,
            review_cycles,
            check_passed_events: checks,
            final_check_gate_passed: final_gate_passed,
            approved_events: approvals,
            accepted,
            nano_end_reason,
            timing: attempt.timing,
            metrics,
            harness_notes: attempt.harness_notes,
        });
    }
    let groups = groups
        .into_iter()
        .map(|(key, count)| Group {
            case_id: key.case_id,
            variant: key.variant,
            cache: key.cache,
            model: key.model,
            settings_sha256: key.settings_sha256,
            samples: count.times.len(),
            accepted: count.accepted,
            total_ms: Distribution::from_samples(count.times).unwrap(),
        })
        .collect();
    Ok(Report {
        version: 1,
        suite: "nano-headless-v1",
        attempts: rows,
        groups,
        comparability_notes: vec![
            "External agents may differ in hidden prompts, provider caching, and billing.",
            "Nano token estimates are separate from provider-reported usage.",
            "Null metrics mean the source did not expose a measurement.",
        ],
    })
}

pub(crate) fn report(path: &Path) -> Result<()> {
    let manifest: Manifest = serde_json::from_slice(&bounded_file(path, MANIFEST_BYTES)?)?;
    let report = build(manifest)?;
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    serde_json::to_writer_pretty(&mut lock, &report)?;
    writeln!(lock)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nano::{
        provider::ProviderSettings,
        session::{Budget, EndReason, LaunchEvidence, Limits, SessionIdentity},
    };
    use std::{fs, process::Command};

    #[test]
    fn graph_calls_use_the_discovered_peer_id() {
        for tool in [
            "repository_graph_status",
            "repository_search",
            "repository_context",
        ] {
            assert!(is_graph_call(
                &format!("mcp_repo_{tool}"),
                "{}",
                Some("repo")
            ));
            assert!(!is_graph_call(
                &format!("mcp_graph_{tool}"),
                "{}",
                Some("repo")
            ));
        }
        assert!(!is_graph_call(
            "mcp_repo_repository_unrelated",
            "{}",
            Some("repo")
        ));
        assert!(!is_graph_call("mcp_repo_repository_search", "{}", None));
    }

    fn write_records(path: &Path, records: &[Record]) -> Result<()> {
        let mut file = File::create(path)?;
        for record in records {
            serde_json::to_writer(&mut file, record)?;
            writeln!(file)?;
        }
        Ok(())
    }

    fn set_settings_digest(records: &mut [Record]) -> Result<String> {
        let SessionEvent::Started {
            provider: Some(provider),
            limits,
            launch_evidence: Some(evidence),
            ..
        } = &mut records[0].event
        else {
            anyhow::bail!("Expected a Nano start with launch evidence");
        };
        let digest = effective_settings_sha256(provider, limits, evidence)?;
        evidence.settings_sha256 = Some(digest.clone());
        Ok(digest)
    }

    fn workload_launch(case_id: &str, cache: CacheState) -> Result<serde_json::Value> {
        let suite = suite()?;
        let workload = &suite.workloads[case_id];
        Ok(serde_json::to_value(LaunchWorkloadEvidence {
            case_id: case_id.to_owned(),
            task_sha256: task_sha256(workload),
            check_commands_sha256: crate::checks::commands_sha256(&workload.checks)?,
            graph_snapshot_id: matches!(&cache, CacheState::Warm).then(|| "snapshot".into()),
            cache: Some(cache),
        })?)
    }

    fn submission_payload(task: &str, gate: &str, cycles: u32, case_id: &str) -> Result<String> {
        let suite = suite()?;
        let checks = crate::checks::commands_sha256(&suite.workloads[case_id].checks)?;
        Ok(serde_json::json!({"task_id": task, "check_gate": gate, "review_cycles": cycles, "check_commands_sha256": checks}).to_string())
    }

    fn accepted_external_database(path: &Path, task: &str, run: &str, tree: &str) -> Result<()> {
        let db = Connection::open(path)?;
        db.execute_batch(
            "CREATE TABLE tasks(id TEXT, status TEXT, review_cycles INTEGER);
             CREATE TABLE runs(id TEXT, task_id TEXT, status TEXT, role TEXT);
             CREATE TABLE events(id INTEGER PRIMARY KEY, type TEXT, run_id TEXT, payload_json TEXT);",
        )?;
        db.execute("INSERT INTO tasks VALUES (?1, 'complete', 0)", [task])?;
        db.execute(
            "INSERT INTO runs VALUES (?1, ?2, 'completed', 'executor')",
            params![run, task],
        )?;
        db.execute(
            "INSERT INTO events(type, run_id, payload_json) VALUES ('run_started', ?1, ?2)",
            params![run, serde_json::json!({"baseline_tree": tree, "evaluation": workload_launch("local_bug_fix", CacheState::Cold)?}).to_string()],
        )?;
        db.execute(
            "INSERT INTO events(type, run_id, payload_json) VALUES ('submission_committed', ?1, ?2)",
            params![run, submission_payload(task, "passed", 0, "local_bug_fix")?],
        )?;
        Ok(())
    }

    fn copy_tree(from: &Path, to: &Path) -> Result<()> {
        fs::create_dir_all(to)?;
        for item in fs::read_dir(from)? {
            let item = item?;
            let source = item.path();
            let target = to.join(item.file_name());
            if item.file_type()?.is_dir() {
                copy_tree(&source, &target)?;
            } else {
                fs::copy(source, target)?;
            }
        }
        Ok(())
    }

    #[test]
    fn fixture_git_trees_match_the_pinned_suite() -> Result<()> {
        let suite = suite()?;
        let task_table = include_str!("../../tests/fixtures/nano_eval/tasks.md");
        for (case, workload) in &suite.workloads {
            let row = task_table
                .lines()
                .find(|line| line.starts_with(&format!("| `{case}` |")))
                .context("Missing task table row")?;
            let cells: Vec<_> = row.split('|').map(str::trim).collect();
            assert_eq!(cells[2], workload.task);
            assert!(cells[3].contains(&format!("`{}`", workload.checks[0])));
            if let Some(windows) = &workload.windows_checks {
                assert!(cells[3].contains(&format!("`{}`", windows[0])));
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nano_eval/cases");
        for (case, expected) in suite.cases {
            let temp = tempfile::tempdir()?;
            copy_tree(&root.join(case), temp.path())?;
            ensure!(
                Command::new("git")
                    .args(["init", "-q", "--object-format=sha1"])
                    .env("GIT_DEFAULT_HASH", "sha256")
                    .current_dir(temp.path())
                    .status()?
                    .success()
            );
            ensure!(
                Command::new("git")
                    .args(["config", "core.autocrlf", "false"])
                    .current_dir(temp.path())
                    .status()?
                    .success()
            );
            for args in [&["add", "-A"][..]] {
                ensure!(
                    Command::new("git")
                        .args(args)
                        .current_dir(temp.path())
                        .status()?
                        .success()
                );
            }
            let output = Command::new("git")
                .arg("write-tree")
                .env("GIT_DEFAULT_HASH", "sha256")
                .current_dir(temp.path())
                .output()?;
            ensure!(output.status.success());
            assert_eq!(String::from_utf8(output.stdout)?.trim(), expected);
        }
        Ok(())
    }

    #[test]
    fn cache_label_requires_a_consistent_prelaunch_graph_state() {
        assert_eq!(
            launch_cache(false, CanonicalGraphStatus::Fresh, true),
            Some(CacheState::Disabled)
        );
        assert_eq!(
            launch_cache(true, CanonicalGraphStatus::Unknown, false),
            Some(CacheState::Cold)
        );
        assert_eq!(
            launch_cache(true, CanonicalGraphStatus::Fresh, true),
            Some(CacheState::Warm)
        );
        assert_eq!(launch_cache(true, CanonicalGraphStatus::Stale, true), None);
        assert_eq!(
            launch_cache(true, CanonicalGraphStatus::Unknown, true),
            None
        );
        assert_eq!(launch_cache(true, CanonicalGraphStatus::Fresh, false), None);
    }

    #[test]
    fn submission_alone_is_not_an_accepted_evaluation() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let db_path = temp.path().join("ferrus.db");
        let db = Connection::open(&db_path)?;
        db.execute_batch(
            r#"CREATE TABLE tasks(id TEXT, status TEXT, review_cycles INTEGER);
            CREATE TABLE runs(id TEXT, task_id TEXT, status TEXT, role TEXT);
            CREATE TABLE events(id INTEGER PRIMARY KEY, type TEXT, run_id TEXT, payload_json TEXT);
            INSERT INTO tasks VALUES ('task', 'reviewing', 0);
            INSERT INTO runs VALUES ('run', 'task', 'completed', 'executor');
            INSERT INTO events(type, run_id, payload_json) VALUES ('run_started', 'run', '{}');
            INSERT INTO events(type, run_id, payload_json) VALUES ('check_passed', 'run', '{}');
            INSERT INTO events(type, run_id, payload_json) VALUES ('submission_committed', 'run', '{"task_id":"task","check_gate":"passed","review_cycles":0}');
            INSERT INTO events(type, run_id, payload_json) VALUES ('submitted', 'run', '{"check_gate":"passed"}');"#,
        )?;
        let identity = SessionIdentity {
            session_id: "run".into(),
            project_id: "project".into(),
            task_id: Some("task".into()),
            run_id: Some("run".into()),
        };
        let journal = temp.path().join("events.jsonl");
        let start_tree = suite()?.cases["local_bug_fix"].clone();
        db.execute(
            "UPDATE events SET payload_json = ?1 WHERE type = 'run_started'",
            [serde_json::json!({"baseline_tree": start_tree, "evaluation": workload_launch("local_bug_fix", CacheState::Cold)?}).to_string()],
        )?;
        db.execute(
            "UPDATE events SET payload_json = ?1 WHERE type = 'submission_committed'",
            [submission_payload("task", "passed", 0, "local_bug_fix")?],
        )?;
        let mut records = [
            Record {
                version: 1,
                session_id: "run".into(),
                sequence: 1,
                budget: Budget::default(),
                event: SessionEvent::Started {
                    identity,
                    limits: Limits::default(),
                    input: "task".into(),
                    launch_evidence: Some(Box::new(LaunchEvidence {
                        baseline_tree: start_tree.clone(),
                        native_context_enabled: true,
                        working_set_enabled: false,
                        graph_peer_mode: Some(GraphPeerMode::Absent),
                        graph_peer_id: None,
                        mcp_peer_count: Some(0),
                        mcp_tool_count: Some(0),
                        graph_peer_timeout_ms: None,
                        settings_sha256: None,
                    })),
                    inherited_budget: None,
                    provider: Some(Box::new(ProviderSettings {
                        api: "openai_chat_completions_v1".into(),
                        base_url: "http://127.0.0.1:1234/v1".into(),
                        model: "mock-model".into(),
                        context_tokens: 32768,
                        max_output_tokens: 4096,
                        temperature: 0.0,
                        request_timeout_ms: 120000,
                        wire_bytes: 4 * 1024 * 1024,
                        event_bytes: 256 * 1024,
                        max_tool_calls: 64,
                        include_usage: true,
                    })),
                },
            },
            Record {
                version: 1,
                session_id: "run".into(),
                sequence: 2,
                budget: Budget::default(),
                event: SessionEvent::Ended {
                    reason: EndReason::Cancelled,
                },
            },
        ];
        let nano_digest = set_settings_digest(&mut records)?;
        write_records(&journal, &records)?;
        let attempt = Attempt {
            case_id: "local_bug_fix".into(),
            variant: Variant::NanoNative,
            cache: CacheState::Cold,
            sample: 1,
            start_tree: suite()?.cases["local_bug_fix"].clone(),
            model: "mock-model".into(),
            settings_sha256: nano_digest.clone(),
            native_context_enabled: Some(true),
            working_set_enabled: Some(false),
            database: db_path,
            task_id: "task".into(),
            run_id: "run".into(),
            journal: Some(journal.clone()),
            external_usage: None,
            timing: Timing {
                source: "fixture".into(),
                total_ms: 10,
                index_ms: None,
                refresh_ms: None,
                peak_rss_bytes: None,
            },
            harness_notes: vec![],
        };
        let baseline_attempt = attempt.clone();
        let report = build(Manifest {
            version: 1,
            attempts: vec![attempt],
        })?;
        assert!(!report.attempts[0].accepted);
        assert_eq!(report.groups[0].samples, 1);
        let cold_launch = workload_launch("local_bug_fix", CacheState::Cold)?;
        for field in ["task_sha256", "check_commands_sha256"] {
            let mut wrong = cold_launch.clone();
            wrong[field] = serde_json::json!("0".repeat(64));
            db.execute(
                "UPDATE events SET payload_json = ?1 WHERE type = 'run_started'",
                [
                    serde_json::json!({"baseline_tree": start_tree, "evaluation": wrong})
                        .to_string(),
                ],
            )?;
            assert!(
                build(Manifest {
                    version: 1,
                    attempts: vec![baseline_attempt.clone()]
                })
                .is_err()
            );
        }
        db.execute(
            "UPDATE events SET payload_json = ?1 WHERE type = 'run_started'",
            [serde_json::json!({"baseline_tree": start_tree, "evaluation": workload_launch("local_bug_fix", CacheState::Warm)?}).to_string()],
        )?;
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![baseline_attempt.clone()]
            })
            .is_err()
        );
        let mut warm_attempt = baseline_attempt.clone();
        warm_attempt.cache = CacheState::Warm;
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![warm_attempt]
            })
            .is_ok()
        );
        db.execute(
            "UPDATE events SET payload_json = ?1 WHERE type = 'run_started'",
            [
                serde_json::json!({"baseline_tree": start_tree, "evaluation": cold_launch})
                    .to_string(),
            ],
        )?;
        db.execute(
            "UPDATE events SET payload_json = ?1 WHERE type = 'submission_committed'",
            [serde_json::json!({"task_id":"task", "check_gate":"passed", "review_cycles":0, "check_commands_sha256":"0".repeat(64)}).to_string()],
        )?;
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![baseline_attempt.clone()]
            })
            .is_err()
        );
        db.execute(
            "UPDATE events SET payload_json = ?1 WHERE type = 'submission_committed'",
            [submission_payload("task", "passed", 0, "local_bug_fix")?],
        )?;
        let mut wrong_settings = baseline_attempt.clone();
        wrong_settings.settings_sha256 = "0".repeat(64);
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![wrong_settings]
            })
            .is_err()
        );
        if let SessionEvent::Started {
            provider: Some(provider),
            ..
        } = &mut records[0].event
        {
            provider.temperature = 0.5;
        }
        write_records(&journal, &records)?;
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![baseline_attempt.clone()]
            })
            .is_err()
        );
        if let SessionEvent::Started {
            provider: Some(provider),
            ..
        } = &mut records[0].event
        {
            provider.temperature = 0.0;
        }
        write_records(&journal, &records)?;
        let mut wrong_model = baseline_attempt.clone();
        wrong_model.model = "other-model".into();
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![wrong_model]
            })
            .is_err()
        );
        let mut mcp_attempt = baseline_attempt.clone();
        mcp_attempt.variant = Variant::NanoMcp;
        mcp_attempt.native_context_enabled = Some(false);
        if let SessionEvent::Started {
            launch_evidence: Some(evidence),
            ..
        } = &mut records[0].event
        {
            evidence.native_context_enabled = false;
        }
        mcp_attempt.settings_sha256 = set_settings_digest(&mut records)?;
        write_records(&journal, &records)?;
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![mcp_attempt.clone()]
            })
            .is_err()
        );
        if let SessionEvent::Started {
            launch_evidence: Some(evidence),
            ..
        } = &mut records[0].event
        {
            evidence.graph_peer_mode = Some(GraphPeerMode::Complete);
        }
        mcp_attempt.settings_sha256 = set_settings_digest(&mut records)?;
        write_records(&journal, &records)?;
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![mcp_attempt.clone()]
            })
            .is_err()
        );
        if let SessionEvent::Started {
            launch_evidence: Some(evidence),
            ..
        } = &mut records[0].event
        {
            evidence.graph_peer_id = Some("repo".into());
            evidence.mcp_peer_count = Some(2);
            evidence.mcp_tool_count = Some(4);
        }
        mcp_attempt.settings_sha256 = set_settings_digest(&mut records)?;
        write_records(&journal, &records)?;
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![mcp_attempt.clone()]
            })
            .is_err()
        );
        if let SessionEvent::Started {
            launch_evidence: Some(evidence),
            ..
        } = &mut records[0].event
        {
            evidence.mcp_peer_count = Some(1);
            evidence.mcp_tool_count = Some(3);
        }
        mcp_attempt.settings_sha256 = set_settings_digest(&mut records)?;
        write_records(&journal, &records)?;
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![mcp_attempt.clone()]
            })
            .is_err()
        );
        if let SessionEvent::Started {
            launch_evidence: Some(evidence),
            ..
        } = &mut records[0].event
        {
            evidence.graph_peer_timeout_ms = Some(10_000);
        }
        mcp_attempt.settings_sha256 = set_settings_digest(&mut records)?;
        write_records(&journal, &records)?;
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![mcp_attempt.clone()]
            })
            .is_ok()
        );
        let first_digest = mcp_attempt.settings_sha256.clone();
        if let SessionEvent::Started {
            launch_evidence: Some(evidence),
            ..
        } = &mut records[0].event
        {
            evidence.graph_peer_timeout_ms = Some(11_000);
        }
        let second_digest = set_settings_digest(&mut records)?;
        assert_ne!(first_digest, second_digest);
        write_records(&journal, &records)?;
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![mcp_attempt.clone()]
            })
            .is_err()
        );
        mcp_attempt.settings_sha256 = second_digest;
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![mcp_attempt.clone()]
            })
            .is_ok()
        );
        let mut disabled_attempt = baseline_attempt.clone();
        disabled_attempt.variant = Variant::NanoGraphDisabled;
        disabled_attempt.cache = CacheState::Disabled;
        disabled_attempt.native_context_enabled = Some(false);
        disabled_attempt.settings_sha256 = mcp_attempt.settings_sha256.clone();
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![disabled_attempt.clone()]
            })
            .is_err()
        );
        if let SessionEvent::Started {
            launch_evidence: Some(evidence),
            ..
        } = &mut records[0].event
        {
            evidence.graph_peer_mode = Some(GraphPeerMode::Absent);
            evidence.graph_peer_id = None;
            evidence.graph_peer_timeout_ms = None;
            evidence.mcp_peer_count = Some(1);
            evidence.mcp_tool_count = Some(1);
        }
        disabled_attempt.settings_sha256 = set_settings_digest(&mut records)?;
        write_records(&journal, &records)?;
        db.execute(
            "UPDATE events SET payload_json = ?1 WHERE type = 'run_started'",
            [serde_json::json!({"baseline_tree": start_tree, "evaluation": workload_launch("local_bug_fix", CacheState::Disabled)?}).to_string()],
        )?;
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![disabled_attempt.clone()]
            })
            .is_err()
        );
        if let SessionEvent::Started {
            launch_evidence: Some(evidence),
            ..
        } = &mut records[0].event
        {
            evidence.mcp_peer_count = Some(0);
            evidence.mcp_tool_count = Some(0);
        }
        disabled_attempt.settings_sha256 = set_settings_digest(&mut records)?;
        write_records(&journal, &records)?;
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![disabled_attempt]
            })
            .is_ok()
        );
        db.execute(
            "UPDATE events SET payload_json = ?1 WHERE type = 'run_started'",
            [serde_json::json!({"baseline_tree": start_tree, "evaluation": workload_launch("local_bug_fix", CacheState::Cold)?}).to_string()],
        )?;
        if let SessionEvent::Started {
            launch_evidence: Some(evidence),
            ..
        } = &mut records[0].event
        {
            evidence.native_context_enabled = true;
        }
        assert_eq!(set_settings_digest(&mut records)?, nano_digest);
        write_records(&journal, &records)?;
        let mut wrong_flags = baseline_attempt.clone();
        wrong_flags.variant = Variant::NanoMcp;
        wrong_flags.native_context_enabled = Some(false);
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![wrong_flags]
            })
            .is_err()
        );
        let wrong_tree = baseline_attempt.clone();
        if let SessionEvent::Started {
            launch_evidence: Some(evidence),
            ..
        } = &mut records[0].event
        {
            evidence.baseline_tree = "0".repeat(40);
        }
        let mut file = File::create(&journal)?;
        for record in &records {
            serde_json::to_writer(&mut file, record)?;
            writeln!(file)?;
        }
        drop(file);
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![wrong_tree.clone()]
            })
            .is_err()
        );
        if let SessionEvent::Started {
            launch_evidence: Some(evidence),
            ..
        } = &mut records[0].event
        {
            evidence.baseline_tree = start_tree.clone();
        }
        let mut file = File::create(&journal)?;
        for record in &records {
            serde_json::to_writer(&mut file, record)?;
            writeln!(file)?;
        }
        drop(file);
        db.execute("UPDATE tasks SET status='complete'", [])?;
        db.execute("DELETE FROM events WHERE type = 'submitted'", [])?;
        let attempt = Attempt {
            case_id: "local_bug_fix".into(),
            variant: Variant::ExternalMcp,
            cache: CacheState::Cold,
            sample: 1,
            start_tree: suite()?.cases["local_bug_fix"].clone(),
            model: "mock-model".into(),
            settings_sha256: "0".repeat(64),
            native_context_enabled: None,
            working_set_enabled: None,
            database: temp.path().join("ferrus.db"),
            task_id: "task".into(),
            run_id: "run".into(),
            journal: None,
            external_usage: None,
            timing: Timing {
                source: "fixture".into(),
                total_ms: 10,
                index_ms: None,
                refresh_ms: None,
                peak_rss_bytes: None,
            },
            harness_notes: vec![],
        };
        let accepted_attempt = attempt.clone();
        let mut other_model = attempt.clone();
        other_model.model = "other-model".into();
        let mut other_settings = attempt.clone();
        other_settings.settings_sha256 = "1".repeat(64);
        db.execute_batch(
            "INSERT INTO tasks VALUES ('task-2', 'complete', 0);
             INSERT INTO runs VALUES ('run-2', 'task-2', 'completed', 'executor');
             INSERT INTO tasks VALUES ('task-3', 'complete', 0);
             INSERT INTO runs VALUES ('run-3', 'task-3', 'completed', 'executor');",
        )?;
        for (task_id, run_id) in [("task-2", "run-2"), ("task-3", "run-3")] {
            db.execute(
                "INSERT INTO events(type, run_id, payload_json) VALUES ('run_started', ?1, ?2)",
                params![
                    run_id,
                    serde_json::json!({"baseline_tree": start_tree, "evaluation": workload_launch("local_bug_fix", CacheState::Cold)?}).to_string()
                ],
            )?;
            db.execute(
                "INSERT INTO events(type, run_id, payload_json) VALUES ('submission_committed', ?1, ?2)",
                params![run_id, submission_payload(task_id, "passed", 0, "local_bug_fix")?],
            )?;
        }
        other_model.task_id = "task-2".into();
        other_model.run_id = "run-2".into();
        other_settings.task_id = "task-3".into();
        other_settings.run_id = "run-3".into();
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![attempt.clone(), other_model.clone()],
            })
            .is_err()
        );
        other_model.database = temp.path().join("sample-2.db");
        other_settings.database = temp.path().join("sample-3.db");
        accepted_external_database(&other_model.database, "task-2", "run-2", &start_tree)?;
        accepted_external_database(&other_settings.database, "task-3", "run-3", &start_tree)?;
        let report = build(Manifest {
            version: 1,
            attempts: vec![attempt.clone(), other_model, other_settings],
        })?;
        assert_eq!(report.groups.len(), 3);
        assert!(report.attempts.iter().all(|row| row.accepted));
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![attempt.clone(), attempt.clone()],
            })
            .is_err()
        );
        let mut repeated = attempt.clone();
        repeated.sample = 2;
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![attempt.clone(), repeated],
            })
            .is_err()
        );
        db.execute(
            "UPDATE events SET payload_json = '{\"baseline_tree\":\"wrong\"}' WHERE type = 'run_started'",
            [],
        )?;
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![attempt.clone()],
            })
            .is_err()
        );
        db.execute(
            "UPDATE events SET payload_json = ?1 WHERE type = 'run_started'",
            [serde_json::json!({"baseline_tree": start_tree, "evaluation": workload_launch("local_bug_fix", CacheState::Cold)?}).to_string()],
        )?;
        let report = build(Manifest {
            version: 1,
            attempts: vec![attempt],
        })?;
        assert!(report.attempts[0].accepted);
        assert_eq!(report.attempts[0].approved_events, 0);
        db.execute(
            "UPDATE events SET payload_json = ?1 WHERE type = 'submission_committed'",
            [submission_payload("task", "skipped", 0, "local_bug_fix")?],
        )?;
        let report = build(Manifest {
            version: 1,
            attempts: vec![accepted_attempt.clone()],
        })?;
        assert!(!report.attempts[0].accepted);
        db.execute(
            "UPDATE events SET payload_json = ?1 WHERE type = 'submission_committed'",
            [submission_payload("task", "passed", 0, "local_bug_fix")?],
        )?;
        db.execute("UPDATE tasks SET review_cycles=1", [])?;
        let report = build(Manifest {
            version: 1,
            attempts: vec![accepted_attempt.clone()],
        })?;
        assert!(!report.attempts[0].accepted);
        db.execute_batch(
            r#"INSERT INTO runs VALUES ('later', 'task', 'completed', 'executor');
            INSERT INTO events(type, run_id, payload_json) VALUES ('rejected', 'run', '{}');
            INSERT INTO events(type, run_id, payload_json) VALUES ('approved', 'later', '{}');"#,
        )?;
        db.execute(
            "INSERT INTO events(type, run_id, payload_json) VALUES ('submission_committed', 'later', ?1)",
            [submission_payload("task", "passed", 1, "local_bug_fix")?],
        )?;
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![accepted_attempt.clone()],
            })
            .is_err()
        );
        db.execute("UPDATE runs SET role='reviewer' WHERE id='later'", [])?;
        let report = build(Manifest {
            version: 1,
            attempts: vec![accepted_attempt],
        })?;
        assert!(!report.attempts[0].accepted);
        assert!(!report.attempts[0].final_check_gate_passed);
        let inherited = Budget {
            model_turns: 3,
            tool_calls: 2,
            reported_input_tokens: 50,
            estimated_output_tokens: 10,
            ..Default::default()
        };
        records[0].budget = inherited.clone();
        records[1].budget = inherited.clone();
        if let SessionEvent::Started {
            inherited_budget, ..
        } = &mut records[0].event
        {
            *inherited_budget = Some(inherited);
        }
        let mut file = File::create(baseline_attempt.journal.as_ref().unwrap())?;
        for record in &records {
            serde_json::to_writer(&mut file, record)?;
            writeln!(file)?;
        }
        drop(file);
        let report = build(Manifest {
            version: 1,
            attempts: vec![baseline_attempt],
        })?;
        let metrics = &report.attempts[0].metrics;
        assert_eq!(metrics.model_turns, Some(0));
        assert_eq!(metrics.tool_calls, Some(0));
        assert_eq!(metrics.input_tokens_reported, Some(0));
        assert_eq!(metrics.output_tokens_estimated, Some(0));
        Ok(())
    }
}
