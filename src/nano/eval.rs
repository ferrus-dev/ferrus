//! Offline evaluation of pinned headless Executor attempts.
//! No provider, task, graph, or workspace operation is performed here.

use super::{
    replay::Replay,
    session::{EndReason, GraphPeerMode, Record, SessionEvent},
    tools::ToolOutcome,
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
}

fn suite() -> Result<Suite> {
    let suite: Suite =
        serde_json::from_str(include_str!("../../tests/fixtures/nano_eval/suite.json"))?;
    ensure!(
        suite.version == 1 && suite.cases.len() == 7,
        "Invalid pinned suite"
    );
    Ok(suite)
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

fn task_state(attempt: &Attempt) -> Result<(String, String, u32, u64, bool, u64)> {
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
    let inherited = match &records[0].event {
        SessionEvent::Started {
            inherited_budget,
            launch_evidence,
            provider,
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
            ensure!(
                provider.as_ref().map(|settings| settings.model.as_str())
                    == Some(attempt.model.as_str()),
                "Nano journal model does not match manifest"
            );
            inherited_budget.clone().unwrap_or_default()
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
    let mut active_context = None;
    let mut tool_latencies = Vec::new();
    let mut context_latencies = Vec::new();
    for record in &records {
        match &record.event {
            SessionEvent::ToolIntent { call_id, call, .. } => {
                if call.name == "repository_fallback" {
                    fallback_calls += 1;
                }
                if matches!(
                    call.name.as_str(),
                    "repository_graph_status" | "repository_search" | "repository_context"
                ) || call.name.starts_with("mcp_graph_repository_")
                    || (matches!(
                        call.name.as_str(),
                        "project_context_search" | "project_context"
                    ) && serde_json::from_str::<serde_json::Value>(&call.arguments)
                        .ok()
                        .and_then(|args| args["domain"].as_str().map(str::to_owned))
                        .is_some_and(|domain| matches!(domain.as_str(), "repository" | "all")))
                {
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
                active_context = Some(record.budget.elapsed_ms);
                stale_events += preparation
                    .observations
                    .iter()
                    .filter(|item| stale_observation(item))
                    .count() as u64;
            }
            SessionEvent::ContextComposed { .. } => {
                if let Some(start) = active_context.take() {
                    context_latencies.push(record.budget.elapsed_ms.saturating_sub(start));
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
    let mut seen_runs = BTreeSet::new();
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
            !matches!(attempt.variant, Variant::NanoGraphDisabled)
                || matches!(attempt.cache, CacheState::Disabled),
            "Graph-disabled attempt must report disabled cache state"
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
        ensure!(
            seen_runs.insert((
                std::fs::canonicalize(&attempt.database)?,
                attempt.task_id.clone(),
                attempt.run_id.clone(),
            )),
            "Evaluation run reused as another sample"
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
        let (task_status, run_status, review_cycles, checks, final_gate_passed, approvals) =
            task_state(&attempt)?;
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

    fn write_records(path: &Path, records: &[Record]) -> Result<()> {
        let mut file = File::create(path)?;
        for record in records {
            serde_json::to_writer(&mut file, record)?;
            writeln!(file)?;
        }
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
            [serde_json::json!({"baseline_tree": start_tree}).to_string()],
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
                    launch_evidence: Some(LaunchEvidence {
                        baseline_tree: start_tree.clone(),
                        native_context_enabled: true,
                        working_set_enabled: false,
                        graph_peer_mode: Some(GraphPeerMode::Absent),
                    }),
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
        let mut file = File::create(&journal)?;
        for record in &records {
            serde_json::to_writer(&mut file, record)?;
            writeln!(file)?;
        }
        let attempt = Attempt {
            case_id: "local_bug_fix".into(),
            variant: Variant::NanoNative,
            cache: CacheState::Cold,
            sample: 1,
            start_tree: suite()?.cases["local_bug_fix"].clone(),
            model: "mock-model".into(),
            settings_sha256: "0".repeat(64),
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
        write_records(&journal, &records)?;
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![mcp_attempt]
            })
            .is_ok()
        );
        let mut disabled_attempt = baseline_attempt.clone();
        disabled_attempt.variant = Variant::NanoGraphDisabled;
        disabled_attempt.cache = CacheState::Disabled;
        disabled_attempt.native_context_enabled = Some(false);
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
        }
        write_records(&journal, &records)?;
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![disabled_attempt]
            })
            .is_ok()
        );
        if let SessionEvent::Started {
            launch_evidence: Some(evidence),
            ..
        } = &mut records[0].event
        {
            evidence.native_context_enabled = true;
        }
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
                    serde_json::json!({"baseline_tree": start_tree}).to_string()
                ],
            )?;
            db.execute(
                "INSERT INTO events(type, run_id, payload_json) VALUES ('submission_committed', ?1, ?2)",
                params![run_id, serde_json::json!({"task_id": task_id, "check_gate": "passed", "review_cycles": 0}).to_string()],
            )?;
        }
        other_model.task_id = "task-2".into();
        other_model.run_id = "run-2".into();
        other_settings.task_id = "task-3".into();
        other_settings.run_id = "run-3".into();
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
            [serde_json::json!({"baseline_tree": start_tree}).to_string()],
        )?;
        let report = build(Manifest {
            version: 1,
            attempts: vec![attempt],
        })?;
        assert!(report.attempts[0].accepted);
        assert_eq!(report.attempts[0].approved_events, 0);
        db.execute(
            "UPDATE events SET payload_json = '{\"task_id\":\"task\",\"check_gate\":\"skipped\",\"review_cycles\":0}' WHERE type = 'submission_committed'",
            [],
        )?;
        let report = build(Manifest {
            version: 1,
            attempts: vec![accepted_attempt.clone()],
        })?;
        assert!(!report.attempts[0].accepted);
        db.execute(
            "UPDATE events SET payload_json = '{\"task_id\":\"task\",\"check_gate\":\"passed\",\"review_cycles\":0}' WHERE type = 'submission_committed'",
            [],
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
            INSERT INTO events(type, run_id, payload_json) VALUES ('submission_committed', 'later', '{"task_id":"task","check_gate":"passed","review_cycles":1}');
            INSERT INTO events(type, run_id, payload_json) VALUES ('approved', 'later', '{}');"#,
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
