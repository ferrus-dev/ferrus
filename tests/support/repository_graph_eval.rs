use std::{
    collections::{BTreeSet, HashSet},
    num::{NonZeroU32, NonZeroU64},
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result, bail};
use ferrus::repository_graph::{
    config::RepositoryGraphConfig,
    domain::{
        BuildId, NodeId, PublishedViewName, QueryBudget, RepoPath, RepositoryId,
        RepositoryNamespace, RepositoryRef, SemanticKey,
    },
    index::{IndexCoordinator, IndexRequest, active_extractor_identities},
    ports::GraphQuery,
    query::{
        ContextPolicy, ContextRequest, ContextSeed, EdgeDirection, PageRequest, QueryScope,
        SearchRequest, SnapshotSelector, StatusRequest, TruncationReason,
    },
    query_sqlite::{FreshnessComparison, SqliteGraphQuery, default_budget},
    source::{FilesystemRepositorySource, SourceDiscoveryContext},
    sqlite::{
        OpenQuerySidecarResult, OpenSidecarResult, Sidecar, open_for_build_at, open_for_query_at,
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;

const CORPUS_JSON: &str = include_str!("../fixtures/repository_graph_eval/cases.json");
const MIN_CASES: usize = 20;
const DISCOVERY_RECALL_THRESHOLD: f64 = 0.90;
const NAVIGATION_REDUCTION_THRESHOLD: f64 = 0.20;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvalCorpus {
    schema_version: u32,
    corpus_version: String,
    cases: Vec<EvalCase>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvalCase {
    id: String,
    labels: Vec<String>,
    operation: EvalOperation,
    query: Option<String>,
    seed: Option<SeedSpec>,
    baseline_hint: String,
    expected_paths: Vec<String>,
    supported: bool,
    designated_navigation: bool,
    #[serde(default)]
    expected_truncation: Option<TruncationReason>,
    #[serde(default)]
    require_repeat_determinism: bool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EvalOperation {
    Search,
    Context,
    MissingIndex,
    StaleStatus,
    SearchTruncation,
    ContextTruncation,
}

#[derive(Debug, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum SeedSpec {
    Node(String),
    Symbol(String),
    Path(String),
}

#[derive(Debug, Serialize)]
pub struct EvalReport {
    pub schema_version: u32,
    pub corpus_version: String,
    pub fixture_digest: String,
    pub case_count: usize,
    pub cases: Vec<CaseReport>,
    pub distributions: EvaluationDistributions,
    pub gates: QualityGates,
    pub automation_recommendation: AutomationRecommendation,
}

#[derive(Debug, Serialize)]
pub struct CaseReport {
    pub id: String,
    pub labels: Vec<String>,
    pub supported: bool,
    pub designated_navigation: bool,
    pub expected_paths: Vec<String>,
    pub returned_paths: Vec<String>,
    pub truncation: Option<TruncationReason>,
    pub repeat_required: bool,
    pub repeat_semantically_identical: bool,
    pub graph_cold: NavigationMetrics,
    pub graph_warm: NavigationMetrics,
    pub baseline: NavigationMetrics,
}

#[derive(Debug, Clone, Serialize)]
pub struct NavigationMetrics {
    pub success: bool,
    pub time_to_first_relevant_us: u64,
    pub tool_calls: u64,
    pub files_read: u64,
    pub context_bytes: u64,
    pub graph_query_bytes: u64,
    pub total_duration_us: u64,
}

#[derive(Debug, Serialize)]
pub struct EvaluationDistributions {
    pub graph_cold_latency_us: Vec<u64>,
    pub graph_warm_latency_us: Vec<u64>,
    pub graph_response_bytes: Vec<u64>,
}

#[derive(Debug, Serialize)]
pub struct QualityGates {
    pub exact_path_recall_at_1: GateResult,
    pub exact_unique_symbol_recall_at_1: GateResult,
    pub supported_discovery_recall_at_10: GateResult,
    pub repeated_query_determinism: GateResult,
    pub no_correctness_regression: GateResult,
    pub navigation_context_reduction: ReductionGateResult,
    pub all_passed: bool,
}

#[derive(Debug, Serialize)]
pub struct GateResult {
    pub passed: bool,
    pub observed: f64,
    pub threshold: f64,
    pub evaluated_cases: usize,
}

#[derive(Debug, Serialize)]
pub struct ReductionGateResult {
    pub passed: bool,
    pub median_files_read_reduction: f64,
    pub median_context_bytes_reduction: f64,
    pub threshold: f64,
    pub evaluated_cases: usize,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationRecommendation {
    EligibleForStrongerGuidance,
    KeepStrongerGuidanceDisabled,
}

struct EvalFixture {
    source_dir: TempDir,
    _sidecar_dir: TempDir,
    sidecar: Sidecar,
    config: RepositoryGraphConfig,
    repository: RepositoryRef,
    view: PublishedViewName,
    files: Vec<RepoPath>,
    freshness: FreshnessComparison,
    missing_sidecar: PathBuf,
    fixture_digest: String,
}

#[derive(Clone)]
struct GraphObservation {
    response: serde_json::Value,
    paths: Vec<String>,
    truncation: Option<TruncationReason>,
    metrics: NavigationMetrics,
}

pub fn run_evaluation() -> Result<EvalReport> {
    let corpus: EvalCorpus = serde_json::from_str(CORPUS_JSON)
        .context("repository graph evaluation corpus is invalid")?;
    validate_corpus(&corpus)?;
    let fixture = EvalFixture::new()?;
    let mut reports = Vec::with_capacity(corpus.cases.len());

    for case in &corpus.cases {
        let cold = fixture.run_graph(case)?;
        let warm = fixture.run_graph(case)?;
        let repeat_semantically_identical = cold.response == warm.response;
        let baseline = fixture.run_baseline(case)?;
        reports.push(CaseReport {
            id: case.id.clone(),
            labels: case.labels.clone(),
            supported: case.supported,
            designated_navigation: case.designated_navigation,
            expected_paths: case.expected_paths.clone(),
            returned_paths: cold.paths.clone(),
            truncation: cold.truncation,
            repeat_required: case.require_repeat_determinism,
            repeat_semantically_identical,
            graph_cold: cold.metrics,
            graph_warm: warm.metrics,
            baseline,
        });
    }

    let gates = quality_gates(&reports);
    let automation_recommendation = if gates.all_passed {
        AutomationRecommendation::EligibleForStrongerGuidance
    } else {
        AutomationRecommendation::KeepStrongerGuidanceDisabled
    };
    let mut cold_latency = reports
        .iter()
        .map(|case| case.graph_cold.total_duration_us)
        .collect::<Vec<_>>();
    let mut warm_latency = reports
        .iter()
        .map(|case| case.graph_warm.total_duration_us)
        .collect::<Vec<_>>();
    let mut response_bytes = reports
        .iter()
        .map(|case| case.graph_warm.graph_query_bytes)
        .collect::<Vec<_>>();
    cold_latency.sort_unstable();
    warm_latency.sort_unstable();
    response_bytes.sort_unstable();

    Ok(EvalReport {
        schema_version: corpus.schema_version,
        corpus_version: corpus.corpus_version,
        fixture_digest: fixture.fixture_digest,
        case_count: reports.len(),
        cases: reports,
        distributions: EvaluationDistributions {
            graph_cold_latency_us: cold_latency,
            graph_warm_latency_us: warm_latency,
            graph_response_bytes: response_bytes,
        },
        gates,
        automation_recommendation,
    })
}

fn validate_corpus(corpus: &EvalCorpus) -> Result<()> {
    if corpus.schema_version != 1 {
        bail!("unsupported repository graph evaluation corpus version");
    }
    if corpus.cases.len() < MIN_CASES {
        bail!("repository graph evaluation corpus requires at least {MIN_CASES} cases");
    }
    let mut ids = HashSet::new();
    for case in &corpus.cases {
        if case.id.trim().is_empty() || !ids.insert(case.id.as_str()) {
            bail!("repository graph evaluation case ids must be non-empty and unique");
        }
        if case.labels.is_empty() {
            bail!("repository graph evaluation cases require labels");
        }
        match case.operation {
            EvalOperation::Search | EvalOperation::SearchTruncation
                if case.query.as_deref().is_none_or(str::is_empty) =>
            {
                bail!("search evaluation cases require a query")
            }
            EvalOperation::Context | EvalOperation::ContextTruncation if case.seed.is_none() => {
                bail!("context evaluation cases require a seed")
            }
            _ => {}
        }
    }
    for required in [
        "exact_path",
        "exact_unique_symbol",
        "ambiguous_symbol",
        "supported_discovery",
        "dependency",
        "documentation",
        "configuration",
        "malformed_source",
        "unsupported_capability",
        "missing_index",
        "stale_index",
        "truncation",
        "determinism",
    ] {
        if !corpus
            .cases
            .iter()
            .any(|case| case.labels.iter().any(|label| label == required))
        {
            bail!("repository graph evaluation corpus is missing the {required} label");
        }
    }
    Ok(())
}

impl EvalFixture {
    fn new() -> Result<Self> {
        let source_dir = tempfile::tempdir()?;
        write_fixture(source_dir.path())?;
        let repository = RepositoryRef {
            namespace: RepositoryNamespace::new("local:evaluation")?,
            repository_id: RepositoryId::new("rg2-navigation")?,
        };
        let config = RepositoryGraphConfig::default();
        let source = discover(source_dir.path(), &repository, &config)?;
        let files = source
            .manifest()
            .files
            .iter()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>();
        let fixture_digest = fixture_digest(source_dir.path(), &files)?;
        let freshness = FreshnessComparison::from_manifest(source.manifest());
        let sidecar_dir = tempfile::tempdir()?;
        let sidecar_path = sidecar_dir.path().join("repo-graph.db");
        let mut sidecar = match open_for_build_at(&sidecar_path)? {
            OpenSidecarResult::Ready(sidecar) => sidecar,
            OpenSidecarResult::RequiresRebuild(_) => bail!("new evaluation sidecar needs rebuild"),
        };
        let view = PublishedViewName::new("canonical")?;
        IndexCoordinator::new(&mut sidecar).index(
            &source,
            &config,
            IndexRequest {
                build_id: BuildId::new("eval-build")?,
                view_name: view.clone(),
                force_full: true,
            },
        )?;
        let missing_sidecar = sidecar_dir.path().join("missing-repo-graph.db");
        Ok(Self {
            source_dir,
            _sidecar_dir: sidecar_dir,
            sidecar,
            config,
            repository,
            view,
            files,
            freshness,
            missing_sidecar,
            fixture_digest,
        })
    }

    fn run_graph(&self, case: &EvalCase) -> Result<GraphObservation> {
        let started = Instant::now();
        let (response, paths, truncation) = match case.operation {
            EvalOperation::Search | EvalOperation::SearchTruncation => {
                let budget =
                    self.budget(matches!(case.operation, EvalOperation::SearchTruncation))?;
                let request = SearchRequest {
                    scope: self.scope(budget),
                    text: case.query.clone().expect("validated search query"),
                    node_kinds: vec![],
                    paths: vec![],
                    page: PageRequest { cursor: None },
                };
                let query = self.query(self.freshness.clone());
                let response = query.search(&request)?;
                let paths = response
                    .data
                    .hits
                    .iter()
                    .filter_map(|hit| hit.path.as_ref().map(ToString::to_string))
                    .collect::<Vec<_>>();
                let truncation = response.page.truncation.as_ref().map(|value| value.reason);
                (
                    serde_json::to_value(response)?,
                    stable_unique(paths),
                    truncation,
                )
            }
            EvalOperation::Context | EvalOperation::ContextTruncation => {
                let budget =
                    self.budget(matches!(case.operation, EvalOperation::ContextTruncation))?;
                let request = ContextRequest {
                    scope: self.scope(budget),
                    seeds: vec![context_seed(
                        case.seed.as_ref().expect("validated context seed"),
                    )?],
                    policy: ContextPolicy {
                        direction: EdgeDirection::Both,
                        edge_kinds: vec![],
                        include_unresolved: false,
                        include_external: false,
                    },
                    page: PageRequest { cursor: None },
                };
                let query = self.query(self.freshness.clone());
                let response = query.context(&request)?;
                let paths = response
                    .data
                    .items
                    .iter()
                    .map(|item| item.path.to_string())
                    .collect::<Vec<_>>();
                let truncation = response.page.truncation.as_ref().map(|value| value.reason);
                (
                    serde_json::to_value(response)?,
                    stable_unique(paths),
                    truncation,
                )
            }
            EvalOperation::MissingIndex => {
                let state = match open_for_query_at(&self.missing_sidecar)? {
                    OpenQuerySidecarResult::Absent => "not_built",
                    _ => "unexpected",
                };
                (
                    serde_json::json!({
                        "availability": state,
                        "recommended_action": "index"
                    }),
                    vec![],
                    None,
                )
            }
            EvalOperation::StaleStatus => {
                let changed = self.stale_comparison()?;
                let query = self.query(changed);
                let response = query.status(&StatusRequest {
                    scope: self.scope(default_budget(&self.config.query_limits)?),
                })?;
                (serde_json::to_value(response)?, vec![], None)
            }
        };
        let response_bytes = serde_json::to_vec(&response)?.len() as u64;
        let success = graph_success(case, &response, &paths, truncation);
        let elapsed = elapsed_us(started);
        Ok(GraphObservation {
            response,
            paths,
            truncation,
            metrics: NavigationMetrics {
                success,
                time_to_first_relevant_us: elapsed,
                tool_calls: 1,
                files_read: 0,
                context_bytes: response_bytes,
                graph_query_bytes: response_bytes,
                total_duration_us: elapsed,
            },
        })
    }

    fn run_baseline(&self, case: &EvalCase) -> Result<NavigationMetrics> {
        let started = Instant::now();
        if case.labels.iter().any(|label| label == "exact_path") {
            let success = self
                .files
                .iter()
                .any(|path| path.as_str() == case.baseline_hint);
            let elapsed = elapsed_us(started);
            return Ok(NavigationMetrics {
                success,
                time_to_first_relevant_us: elapsed,
                tool_calls: 1,
                files_read: 0,
                context_bytes: 0,
                graph_query_bytes: 0,
                total_duration_us: elapsed,
            });
        }
        let mut files_read = 0_u64;
        let mut context_bytes = 0_u64;
        let mut matches = BTreeSet::new();
        for path in &self.files {
            let bytes = std::fs::read(self.source_dir.path().join(path.as_str()))?;
            files_read += 1;
            context_bytes += bytes.len() as u64;
            let is_match = String::from_utf8_lossy(&bytes).contains(&case.baseline_hint)
                || path.as_str().contains(&case.baseline_hint);
            if is_match {
                matches.insert(path.as_str().to_string());
            }
            if baseline_complete(case, &matches) {
                break;
            }
        }
        let success = if case.expected_paths.is_empty() {
            true
        } else {
            case.expected_paths
                .iter()
                .all(|path| matches.contains(path))
        };
        let elapsed = elapsed_us(started);
        Ok(NavigationMetrics {
            success,
            time_to_first_relevant_us: elapsed,
            tool_calls: files_read,
            files_read,
            context_bytes,
            graph_query_bytes: 0,
            total_duration_us: elapsed,
        })
    }

    fn query(&self, freshness: FreshnessComparison) -> SqliteGraphQuery<'_> {
        SqliteGraphQuery::new(
            &self.sidecar,
            self.config.query_limits.clone(),
            Some(freshness),
        )
    }

    fn scope(&self, budget: QueryBudget) -> QueryScope {
        QueryScope::current(
            self.repository.clone(),
            SnapshotSelector::Published(self.view.clone()),
            budget,
        )
    }

    fn budget(&self, truncated: bool) -> Result<QueryBudget> {
        let defaults = default_budget(&self.config.query_limits)?;
        if !truncated {
            return Ok(defaults);
        }
        Ok(QueryBudget::new(
            NonZeroU32::new(1).expect("one is non-zero"),
            NonZeroU64::new(self.config.query_limits.max_bytes)
                .context("evaluation query byte limit is zero")?,
            NonZeroU32::new(self.config.query_limits.max_depth)
                .context("evaluation query depth is zero")?,
            NonZeroU64::new(self.config.query_limits.max_duration_ms)
                .context("evaluation query duration is zero")?,
            NonZeroU32::new(self.config.query_limits.max_diagnostics)
                .context("evaluation query diagnostic limit is zero")?,
        ))
    }

    fn stale_comparison(&self) -> Result<FreshnessComparison> {
        let path = self.source_dir.path().join("zz-stale-marker.rs");
        std::fs::write(&path, "pub struct StaleMarker;\n")?;
        let changed = discover(self.source_dir.path(), &self.repository, &self.config)?;
        let comparison = FreshnessComparison::from_manifest(changed.manifest());
        std::fs::remove_file(path)?;
        Ok(comparison)
    }
}

fn graph_success(
    case: &EvalCase,
    response: &serde_json::Value,
    paths: &[String],
    truncation: Option<TruncationReason>,
) -> bool {
    match case.operation {
        EvalOperation::MissingIndex => response["availability"] == "not_built",
        EvalOperation::StaleStatus => response["freshness"]["freshness"] == "stale",
        EvalOperation::SearchTruncation | EvalOperation::ContextTruncation => {
            truncation == case.expected_truncation
        }
        _ if !case.supported => paths.is_empty(),
        _ => case
            .expected_paths
            .iter()
            .all(|expected| paths.iter().any(|path| path == expected)),
    }
}

fn baseline_complete(case: &EvalCase, matches: &BTreeSet<String>) -> bool {
    !case.expected_paths.is_empty()
        && case
            .expected_paths
            .iter()
            .all(|path| matches.contains(path))
}

fn quality_gates(cases: &[CaseReport]) -> QualityGates {
    let exact_path = recall_gate(cases, "exact_path", 1, 1.0);
    let exact_symbol = recall_gate(cases, "exact_unique_symbol", 1, 1.0);
    let discovery = recall_gate(cases, "supported_discovery", 10, DISCOVERY_RECALL_THRESHOLD);
    let deterministic_cases = cases
        .iter()
        .filter(|case| case.repeat_required)
        .collect::<Vec<_>>();
    let deterministic_passes = deterministic_cases
        .iter()
        .filter(|case| case.repeat_semantically_identical)
        .count();
    let deterministic = ratio_gate(deterministic_passes, deterministic_cases.len(), 1.0);
    let regression_cases = cases
        .iter()
        .filter(|case| {
            case.supported
                && case.baseline.success
                && !case.expected_paths.is_empty()
                && !case.labels.iter().any(|label| label == "truncation")
        })
        .collect::<Vec<_>>();
    let regression_passes = regression_cases
        .iter()
        .filter(|case| case.graph_cold.success)
        .count();
    let no_regression = ratio_gate(regression_passes, regression_cases.len(), 1.0);
    let reduction = reduction_gate(cases);
    let all_passed = exact_path.passed
        && exact_symbol.passed
        && discovery.passed
        && deterministic.passed
        && no_regression.passed
        && reduction.passed;
    QualityGates {
        exact_path_recall_at_1: exact_path,
        exact_unique_symbol_recall_at_1: exact_symbol,
        supported_discovery_recall_at_10: discovery,
        repeated_query_determinism: deterministic,
        no_correctness_regression: no_regression,
        navigation_context_reduction: reduction,
        all_passed,
    }
}

fn recall_gate(cases: &[CaseReport], label: &str, limit: usize, threshold: f64) -> GateResult {
    let selected = cases
        .iter()
        .filter(|case| case.labels.iter().any(|candidate| candidate == label))
        .collect::<Vec<_>>();
    let passes = selected
        .iter()
        .filter(|case| {
            let returned = case
                .returned_paths
                .iter()
                .take(limit)
                .collect::<BTreeSet<_>>();
            !case.expected_paths.is_empty()
                && case
                    .expected_paths
                    .iter()
                    .all(|expected| returned.iter().any(|path| path.as_str() == expected))
        })
        .count();
    ratio_gate(passes, selected.len(), threshold)
}

fn ratio_gate(passes: usize, total: usize, threshold: f64) -> GateResult {
    let observed = if total == 0 {
        0.0
    } else {
        passes as f64 / total as f64
    };
    GateResult {
        passed: total > 0 && observed >= threshold,
        observed,
        threshold,
        evaluated_cases: total,
    }
}

fn reduction_gate(cases: &[CaseReport]) -> ReductionGateResult {
    let selected = cases
        .iter()
        .filter(|case| case.designated_navigation && case.baseline.success)
        .collect::<Vec<_>>();
    let mut file_reductions = selected
        .iter()
        .filter(|case| case.baseline.files_read > 0)
        .map(|case| 1.0 - case.graph_warm.files_read as f64 / case.baseline.files_read as f64)
        .collect::<Vec<_>>();
    let mut byte_reductions = selected
        .iter()
        .filter(|case| case.baseline.context_bytes > 0)
        .map(|case| 1.0 - case.graph_warm.context_bytes as f64 / case.baseline.context_bytes as f64)
        .collect::<Vec<_>>();
    let median_files = median(&mut file_reductions);
    let median_bytes = median(&mut byte_reductions);
    ReductionGateResult {
        passed: !selected.is_empty()
            && (median_files >= NAVIGATION_REDUCTION_THRESHOLD
                || median_bytes >= NAVIGATION_REDUCTION_THRESHOLD),
        median_files_read_reduction: median_files,
        median_context_bytes_reduction: median_bytes,
        threshold: NAVIGATION_REDUCTION_THRESHOLD,
        evaluated_cases: selected.len(),
    }
}

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    }
}

fn context_seed(seed: &SeedSpec) -> Result<ContextSeed> {
    match seed {
        SeedSpec::Node(value) => Ok(ContextSeed::Node(NodeId::new(value)?)),
        SeedSpec::Symbol(value) => Ok(ContextSeed::Symbol(SemanticKey::new(value)?)),
        SeedSpec::Path(value) => Ok(ContextSeed::Path(RepoPath::new(value)?)),
    }
}

fn discover(
    root: &Path,
    repository: &RepositoryRef,
    config: &RepositoryGraphConfig,
) -> Result<FilesystemRepositorySource> {
    let identities = active_extractor_identities(config)?;
    let context = SourceDiscoveryContext::from_config(repository.clone(), config, &identities)?;
    Ok(FilesystemRepositorySource::discover(root, context)?)
}

fn stable_unique(paths: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

fn elapsed_us(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn fixture_digest(root: &Path, files: &[RepoPath]) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(CORPUS_JSON.as_bytes());
    for path in files {
        hash.update(path.as_str().as_bytes());
        hash.update([0]);
        hash.update(std::fs::read(root.join(path.as_str()))?);
        hash.update([0]);
    }
    Ok(format!("sha256:{}", hex_lower(&hash.finalize())))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn write_fixture(root: &Path) -> Result<()> {
    for directory in ["config", "docs", "scripts", "src"] {
        std::fs::create_dir_all(root.join(directory))?;
    }
    std::fs::write(
        root.join("README.md"),
        "# Repository Graph Evaluation\nDeterministic navigation fixture.\n",
    )?;
    std::fs::write(
        root.join("docs/architecture.md"),
        "# Architecture Overview\nThe service depends on the API boundary.\n",
    )?;
    std::fs::write(
        root.join("docs/guide.md"),
        "# Navigation Guide\nUse the worker through the service layer.\n",
    )?;
    std::fs::write(
        root.join("config/app.toml"),
        "service = \"repository-graph\"\ntimeout_ms = 250\n",
    )?;
    std::fs::write(
        root.join("scripts/entrypoint.sh"),
        "#!/bin/sh\nexec ferrus \"$@\"\n",
    )?;
    std::fs::write(
        root.join("src/lib.rs"),
        "pub mod api;\npub mod duplicate_a;\npub mod duplicate_b;\npub mod malformed;\npub mod service;\npub mod worker;\npub mod zeta_navigation;\n",
    )?;
    std::fs::write(
        root.join("src/api.rs"),
        "pub struct ApiClient;\nimpl ApiClient { pub fn request(&self) -> bool { true } }\n",
    )?;
    std::fs::write(
        root.join("src/service.rs"),
        "use crate::api::ApiClient;\npub struct RuntimeTaskContext { pub api: ApiClient }\npub fn build_context(api: ApiClient) -> RuntimeTaskContext { RuntimeTaskContext { api } }\n",
    )?;
    std::fs::write(
        root.join("src/worker.rs"),
        "use crate::service::{build_context, RuntimeTaskContext};\npub struct BackgroundWorker;\npub fn run_worker() -> fn(crate::api::ApiClient) -> RuntimeTaskContext { build_context }\n",
    )?;
    std::fs::write(root.join("src/duplicate_a.rs"), "pub struct Shared;\n")?;
    std::fs::write(root.join("src/duplicate_b.rs"), "pub struct Shared;\n")?;
    std::fs::write(
        root.join("src/malformed.rs"),
        "// deliberately malformed\npub fn broken( {\n",
    )?;
    std::fs::write(
        root.join("src/macro_only.rs"),
        "// derive_generated_route exists only in unindexed comment text\npub fn macro_host() {}\n",
    )?;
    std::fs::write(
        root.join("src/zeta_navigation.rs"),
        "pub fn navigation_target() -> &'static str { \"target\" }\n",
    )?;
    for index in 0..24 {
        std::fs::write(
            root.join(format!("src/filler_{index:02}.rs")),
            format!("pub struct Filler{index:02};\n"),
        )?;
    }
    Ok(())
}
