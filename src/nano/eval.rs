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
            max: *values.last()?,
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
    ensure!(
        !records.iter().any(|record| matches!(
            record.event,
            SessionEvent::InteractionOpened
                | SessionEvent::UserInput { .. }
                | SessionEvent::InputRequested
        )),
        "Headless evaluation cannot include an interactive Nano attempt"
    );
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
        .map(|(key, count)| -> Result<Group> {
            Ok(Group {
                case_id: key.case_id,
                variant: key.variant,
                cache: key.cache,
                model: key.model,
                settings_sha256: key.settings_sha256,
                samples: count.times.len(),
                accepted: count.accepted,
                total_ms: Distribution::from_samples(count.times)
                    .context("Missing evaluation timing samples")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
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
#[path = "eval_tests.rs"]
mod tests;
