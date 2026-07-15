use std::{
    num::{NonZeroU32, NonZeroU64},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::{
    project,
    repository_graph::{
        QUERY_WIRE_VERSION,
        config::RepositoryGraphConfig,
        domain::{
            Availability, BuildId, EdgeTarget, Freshness, NodeId, PublishedViewName, QueryBudget,
            RepoPath, RepositoryId, RepositoryNamespace, RepositoryRef, SemanticKey,
        },
        health::{SidecarHealth, inspect_health_at},
        index::{IndexCoordinator, IndexOutcome, IndexRequest, active_extractor_identities},
        ports::GraphQuery,
        query::{
            DiagnosticSummary, EdgeDirection, FreshnessEnvelope, NeighborhoodRequest, PageInfo,
            PageRequest, QueryScope, SearchRequest, ShowLookup, ShowRequest, SnapshotSelector,
            StatusData, StatusRequest, StatusResponse,
        },
        query_sqlite::{FreshnessComparison, SqliteGraphQuery, default_budget},
        source::{LocalRepositorySource, SourceDiscoveryContext},
        sqlite::{
            OpenQuerySidecarResult, OpenSidecarResult, SIDECAR_FILE_NAME, Sidecar,
            open_for_build_at, open_for_query_at,
        },
    },
};

const CANONICAL_VIEW: &str = "canonical";
static BUILD_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Subcommand)]
pub enum GraphCommand {
    /// Build or incrementally update the canonical repository graph.
    Index {
        /// Ignore cached fragments and run every applicable extractor.
        #[arg(long)]
        full: bool,
        /// Emit one machine-readable JSON document.
        #[arg(long)]
        json: bool,
    },
    /// Inspect graph availability, freshness, diagnostics, and fact counts.
    Status {
        /// Emit one machine-readable JSON document.
        #[arg(long)]
        json: bool,
    },
    /// Search indexed node names, semantic keys, and evidence paths.
    Search {
        query: String,
        /// Restrict results to a node kind; may be repeated.
        #[arg(long = "kind")]
        kinds: Vec<String>,
        /// Restrict results to this repository-relative path prefix.
        #[arg(long)]
        path: Option<String>,
        /// Requested result limit; the configured service cap still applies.
        #[arg(long)]
        limit: Option<u32>,
        /// Emit one machine-readable JSON document.
        #[arg(long)]
        json: bool,
    },
    /// Inspect nodes selected by opaque id, semantic key, or evidence path.
    Show(ShowArgs),
    /// Traverse a bounded incoming/outgoing graph neighborhood.
    Neighbors {
        node_id: String,
        #[arg(long, value_enum, default_value_t = Direction::Both)]
        direction: Direction,
        /// Restrict traversal to an edge kind; may be repeated.
        #[arg(long = "kind")]
        kinds: Vec<String>,
        /// Requested traversal depth; the configured service cap still applies.
        #[arg(long)]
        depth: Option<u32>,
        /// Requested combined node/edge limit; the configured service cap still applies.
        #[arg(long)]
        limit: Option<u32>,
        /// Emit one machine-readable JSON document.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Args)]
#[group(skip)]
pub struct ShowArgs {
    /// Look up one opaque node id.
    #[arg(long)]
    node: Option<String>,
    /// Look up one exact semantic key.
    #[arg(long)]
    symbol: Option<String>,
    /// Look up all nodes evidenced by one repository-relative path.
    #[arg(long)]
    path: Option<String>,
    /// Emit one machine-readable JSON document.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
pub enum Direction {
    Outgoing,
    Incoming,
    #[default]
    Both,
}

impl From<Direction> for EdgeDirection {
    fn from(value: Direction) -> Self {
        match value {
            Direction::Outgoing => Self::Outgoing,
            Direction::Incoming => Self::Incoming,
            Direction::Both => Self::Both,
        }
    }
}

pub async fn run(command: GraphCommand) -> Result<()> {
    match command {
        GraphCommand::Index { full, json } => index(full, json).await,
        GraphCommand::Status { json } => status(json).await,
        GraphCommand::Search {
            query,
            kinds,
            path,
            limit,
            json,
        } => search(query, kinds, path, limit, json).await,
        GraphCommand::Show(args) => show(args).await,
        GraphCommand::Neighbors {
            node_id,
            direction,
            kinds,
            depth,
            limit,
            json,
        } => neighbors(node_id, direction, kinds, depth, limit, json).await,
    }
}

struct LocalGraphContext {
    root: std::path::PathBuf,
    repository: RepositoryRef,
    config: RepositoryGraphConfig,
}

impl LocalGraphContext {
    async fn load(require_enabled: bool) -> Result<Self> {
        let root = project::canonical_project_root().await?;
        let contents = tokio::fs::read_to_string(root.join("ferrus.toml"))
            .await
            .context("ferrus.toml not found — run ferrus init first")?;
        let config = RepositoryGraphConfig::from_ferrus_toml(&contents)
            .context("Invalid [repository_graph] configuration")?;
        if require_enabled && !config.enabled {
            anyhow::bail!(
                "repository graph is disabled; set repository_graph.enabled = true in ferrus.toml"
            );
        }
        let project_id = project::current_project_id().await?;
        Ok(Self {
            root,
            repository: RepositoryRef {
                namespace: RepositoryNamespace::new(format!("local:{project_id}"))?,
                repository_id: RepositoryId::new("root")?,
            },
            config,
        })
    }

    fn discover(&self) -> Result<LocalRepositorySource> {
        let identities = active_extractor_identities(&self.config)?;
        let context = SourceDiscoveryContext::from_config(
            self.repository.clone(),
            &self.config,
            &identities,
        )?;
        LocalRepositorySource::discover(&self.root, context)
            .context("Failed to discover the canonical repository source")
    }

    fn freshness_comparison(&self) -> Result<Option<FreshnessComparison>> {
        if !self.config.enabled {
            return Ok(None);
        }
        let source = self.discover()?;
        Ok(Some(FreshnessComparison::from_manifest(source.manifest())))
    }

    fn scope(&self, budget: QueryBudget) -> Result<QueryScope> {
        Ok(QueryScope::v1(
            self.repository.clone(),
            SnapshotSelector::Published(PublishedViewName::new(CANONICAL_VIEW)?),
            budget,
        ))
    }
}

#[derive(Serialize)]
struct IndexOutput {
    status: &'static str,
    freshness: Freshness,
    outcome: IndexOutcome,
}

async fn index(full: bool, json: bool) -> Result<()> {
    let context = LocalGraphContext::load(true).await?;
    let source = context.discover()?;
    let mut sidecar = match open_for_build_at(&sidecar_path().await?)? {
        OpenSidecarResult::Ready(sidecar) => sidecar,
        OpenSidecarResult::RequiresRebuild(reason) => anyhow::bail!(
            "repository graph storage is incompatible (schema {} vs {}): {}; remove the derived sidecar and retry",
            reason.found_schema_version,
            reason.supported_schema_version,
            reason.reason
        ),
    };
    let outcome = IndexCoordinator::new(&mut sidecar).index(
        &source,
        &context.config,
        IndexRequest {
            build_id: next_build_id(),
            view_name: PublishedViewName::new(CANONICAL_VIEW)?,
            force_full: full,
        },
    )?;
    if json {
        print_json(&IndexOutput {
            status: "indexed",
            freshness: Freshness::Fresh,
            outcome,
        })?;
    } else {
        println!("Indexed repository graph");
        println!("Snapshot: {}", outcome.snapshot.id);
        println!("Freshness: fresh");
        println!(
            "Files: {} discovered, {} reused, {} parsed, {} skipped, {} failed",
            outcome.metrics.discovered_files,
            outcome.metrics.reused_files,
            outcome.metrics.parsed_files,
            outcome.metrics.skipped_files,
            outcome.metrics.failed_files
        );
        println!(
            "Facts: {} nodes, {} edges, {} diagnostics",
            outcome.metrics.nodes, outcome.metrics.edges, outcome.metrics.diagnostics
        );
        println!(
            "Work: {} bytes in {} ms",
            outcome.metrics.processed_bytes, outcome.metrics.duration_ms
        );
    }
    Ok(())
}

#[derive(Serialize)]
struct StatusOutput {
    health: SidecarHealth,
    graph: StatusResponse,
}

async fn status(json: bool) -> Result<()> {
    let context = LocalGraphContext::load(false).await?;
    let sidecar_path = sidecar_path().await?;
    let health = inspect_health_at(&sidecar_path)?;
    let freshness_comparison = context.freshness_comparison().ok().flatten();
    let graph = status_response_at(&context, &sidecar_path, freshness_comparison)?;
    let output = StatusOutput { health, graph };
    if json {
        print_json(&output)?;
    } else {
        println!("Repository graph: {:?}", output.graph.data.availability);
        println!(
            "Snapshot: {}",
            output
                .graph
                .snapshot_id
                .as_ref()
                .map_or("none", |id| id.as_str())
        );
        println!("Freshness: {:?}", output.graph.freshness.freshness);
        if let Some(statistics) = &output.graph.data.statistics {
            println!(
                "Facts: {} files, {} nodes, {} edges",
                statistics.files, statistics.nodes, statistics.edges
            );
        }
        print_diagnostics(&output.graph.diagnostics);
        match output.graph.data.availability {
            Availability::NotBuilt => println!("Next: ferrus graph index"),
            Availability::Incompatible => {
                println!("Next: ferrus graph index or rebuild the derived sidecar")
            }
            Availability::Available => {}
        }
    }
    Ok(())
}

fn status_response_at(
    context: &LocalGraphContext,
    sidecar_path: &Path,
    freshness_comparison: Option<FreshnessComparison>,
) -> Result<StatusResponse> {
    match open_for_query_at(sidecar_path) {
        Ok(OpenQuerySidecarResult::Ready(sidecar)) => {
            let query = SqliteGraphQuery::new(
                &sidecar,
                context.config.query_limits.clone(),
                freshness_comparison,
            );
            Ok(query.status(&StatusRequest {
                scope: context.scope(default_budget(&context.config.query_limits)?)?,
            })?)
        }
        Ok(OpenQuerySidecarResult::Absent) => unavailable_status(
            context.repository.clone(),
            Availability::NotBuilt,
            "not_built",
        ),
        Ok(OpenQuerySidecarResult::NeedsMigration {
            found_schema_version,
        }) => unavailable_status(
            context.repository.clone(),
            Availability::Incompatible,
            &format!("schema_{found_schema_version}_needs_migration"),
        ),
        Ok(OpenQuerySidecarResult::RequiresRebuild(_)) => unavailable_status(
            context.repository.clone(),
            Availability::Incompatible,
            "incompatible_schema",
        ),
        Err(_) => unavailable_status(
            context.repository.clone(),
            Availability::Incompatible,
            "sidecar_unreadable",
        ),
    }
}

async fn search(
    text: String,
    kinds: Vec<String>,
    path: Option<String>,
    limit: Option<u32>,
    json: bool,
) -> Result<()> {
    let context = LocalGraphContext::load(true).await?;
    let sidecar = ready_query_sidecar().await?;
    let query = SqliteGraphQuery::new(
        &sidecar,
        context.config.query_limits.clone(),
        context.freshness_comparison()?,
    );
    let response = query.search(&SearchRequest {
        scope: context.scope(requested_budget(&context.config, limit, None)?)?,
        text,
        node_kinds: kinds,
        paths: path
            .map(RepoPath::new)
            .transpose()
            .context("--path must be repository-relative")?
            .into_iter()
            .collect(),
        page: PageRequest { cursor: None },
    })?;
    if json {
        print_json(&response)?;
    } else {
        print_query_header(
            response.snapshot_id.as_str(),
            &response.freshness,
            &response.diagnostics,
            &response.page,
        );
        for hit in &response.data.hits {
            println!(
                "{:.2} {} {}",
                hit.score,
                hit.kind,
                hit.semantic_key
                    .as_ref()
                    .map_or(hit.node_id.as_str(), |key| key.as_str())
            );
            println!(
                "  id={} evidence={} resolution={:?} confidence={:?}",
                hit.node_id,
                evidence_location(hit.path.as_ref(), hit.span.as_ref()),
                hit.provenance.resolution,
                hit.provenance.confidence
            );
        }
    }
    Ok(())
}

async fn show(args: ShowArgs) -> Result<()> {
    if usize::from(args.node.is_some())
        + usize::from(args.symbol.is_some())
        + usize::from(args.path.is_some())
        != 1
    {
        anyhow::bail!("graph show requires exactly one of --node, --symbol, or --path");
    }
    let context = LocalGraphContext::load(true).await?;
    let sidecar = ready_query_sidecar().await?;
    let query = SqliteGraphQuery::new(
        &sidecar,
        context.config.query_limits.clone(),
        context.freshness_comparison()?,
    );
    let lookup = if let Some(node) = args.node {
        ShowLookup::Node(NodeId::new(node)?)
    } else if let Some(symbol) = args.symbol {
        ShowLookup::Symbol(SemanticKey::new(symbol)?)
    } else {
        ShowLookup::Path(
            RepoPath::new(args.path.expect("clap requires exactly one lookup"))
                .context("--path must be repository-relative")?,
        )
    };
    let response = query.show(&ShowRequest {
        scope: context.scope(default_budget(&context.config.query_limits)?)?,
        lookup,
        page: PageRequest { cursor: None },
    })?;
    if args.json {
        print_json(&response)?;
    } else {
        print_query_header(
            response.snapshot_id.as_str(),
            &response.freshness,
            &response.diagnostics,
            &response.page,
        );
        for node in &response.data.nodes {
            let evidence = node.provenance.evidence.as_ref();
            println!(
                "{} {} {}",
                node.kind,
                node.semantic_key.as_ref().map_or("-", |key| key.as_str()),
                node.id
            );
            println!(
                "  evidence={} extractor={}@{} resolution={:?} confidence={:?}",
                evidence_location(
                    evidence.map(|evidence| &evidence.path),
                    evidence.and_then(|evidence| evidence.span.as_ref())
                ),
                node.provenance.extractor.id,
                node.provenance.extractor.version,
                node.provenance.resolution,
                node.provenance.confidence
            );
        }
    }
    Ok(())
}

async fn neighbors(
    node_id: String,
    direction: Direction,
    kinds: Vec<String>,
    depth: Option<u32>,
    limit: Option<u32>,
    json: bool,
) -> Result<()> {
    let context = LocalGraphContext::load(true).await?;
    let sidecar = ready_query_sidecar().await?;
    let query = SqliteGraphQuery::new(
        &sidecar,
        context.config.query_limits.clone(),
        context.freshness_comparison()?,
    );
    let response = query.neighborhood(&NeighborhoodRequest {
        scope: context.scope(requested_budget(&context.config, limit, depth)?)?,
        roots: vec![NodeId::new(node_id)?],
        direction: direction.into(),
        edge_kinds: kinds,
        page: PageRequest { cursor: None },
    })?;
    if json {
        print_json(&response)?;
    } else {
        print_query_header(
            response.snapshot_id.as_str(),
            &response.freshness,
            &response.diagnostics,
            &response.page,
        );
        println!("Nodes:");
        for node in &response.data.nodes {
            println!(
                "  {} {} evidence={} resolution={:?} confidence={:?}",
                node.kind,
                node.id,
                evidence_location(node.path.as_ref(), node.span.as_ref()),
                node.provenance.resolution,
                node.provenance.confidence
            );
        }
        println!("Edges:");
        for edge in &response.data.edges {
            println!(
                "  {} {} -> {} resolution={:?} confidence={:?}",
                edge.kind,
                edge.source,
                edge_target(&edge.target),
                edge.provenance.resolution,
                edge.provenance.confidence
            );
        }
    }
    Ok(())
}

async fn ready_query_sidecar() -> Result<Sidecar> {
    match open_for_query_at(&sidecar_path().await?)? {
        OpenQuerySidecarResult::Ready(sidecar) => Ok(sidecar),
        OpenQuerySidecarResult::Absent => {
            anyhow::bail!("repository graph is not built; run ferrus graph index")
        }
        OpenQuerySidecarResult::NeedsMigration { .. } => {
            anyhow::bail!("repository graph storage needs migration; run ferrus graph index")
        }
        OpenQuerySidecarResult::RequiresRebuild(reason) => anyhow::bail!(
            "repository graph storage is incompatible (schema {} vs {}); rebuild the derived index",
            reason.found_schema_version,
            reason.supported_schema_version
        ),
    }
}

async fn sidecar_path() -> Result<PathBuf> {
    Ok(project::current_project_data_dir()
        .await?
        .join(SIDECAR_FILE_NAME))
}

fn requested_budget(
    config: &RepositoryGraphConfig,
    results: Option<u32>,
    depth: Option<u32>,
) -> Result<QueryBudget> {
    let defaults = &config.query_limits;
    Ok(QueryBudget::new(
        NonZeroU32::new(results.unwrap_or(defaults.max_results))
            .context("--limit must be greater than zero")?,
        NonZeroU64::new(defaults.max_bytes)
            .context("repository_graph.query_limits.max_bytes must be greater than zero")?,
        NonZeroU32::new(depth.unwrap_or(defaults.max_depth))
            .context("--depth must be greater than zero")?,
        NonZeroU64::new(defaults.max_duration_ms)
            .context("repository_graph.query_limits.max_duration_ms must be greater than zero")?,
    ))
}

fn unavailable_status(
    repository: RepositoryRef,
    availability: Availability,
    reason: &str,
) -> Result<StatusResponse> {
    Ok(StatusResponse {
        wire_version: QUERY_WIRE_VERSION,
        repository,
        snapshot_id: None,
        freshness: FreshnessEnvelope {
            freshness: Freshness::NotApplicable,
            compared_manifest: None,
            reason_codes: vec![reason.to_string()],
        },
        diagnostics: DiagnosticSummary::default(),
        data: StatusData {
            availability,
            build_state: None,
            published_view: Some(PublishedViewName::new(CANONICAL_VIEW)?),
            graph_model_version: None,
            statistics: None,
        },
    })
}

fn print_query_header(
    snapshot: &str,
    freshness: &FreshnessEnvelope,
    diagnostics: &DiagnosticSummary,
    page: &PageInfo,
) {
    println!("Snapshot: {snapshot}");
    println!("Freshness: {:?}", freshness.freshness);
    print_diagnostics(diagnostics);
    if let Some(truncation) = &page.truncation {
        println!(
            "Truncated: {:?} ({} results, {} bytes, depth {})",
            truncation.reason,
            truncation.returned_results,
            truncation.returned_bytes,
            truncation.explored_depth
        );
    } else {
        println!("Truncated: no");
    }
}

fn print_diagnostics(diagnostics: &DiagnosticSummary) {
    println!(
        "Diagnostics: {} info, {} warnings, {} errors",
        diagnostics.info, diagnostics.warning, diagnostics.error
    );
}

fn evidence_location(
    path: Option<&RepoPath>,
    span: Option<&crate::repository_graph::domain::SourceSpan>,
) -> String {
    let Some(path) = path else {
        return "none".to_string();
    };
    match span {
        Some(span) => format!(
            "{}:{}:{}",
            path,
            span.start.line.unwrap_or(0),
            span.start.column.unwrap_or(0)
        ),
        None => path.as_str().to_string(),
    }
}

fn edge_target(target: &EdgeTarget) -> &str {
    match target {
        EdgeTarget::Node(node) => node.as_str(),
        EdgeTarget::External(target) | EdgeTarget::Unresolved(target) => target,
    }
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn next_build_id() -> BuildId {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let counter = BUILD_COUNTER.fetch_add(1, Ordering::Relaxed);
    let digest = Sha256::digest(format!(
        "{}:{}:{counter}",
        elapsed.as_secs(),
        elapsed.subsec_nanos()
    ));
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    BuildId::new(format!("build:{encoded}")).expect("a prefixed sha256 build id is never empty")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_ids_are_opaque_and_unique_per_process() {
        let first = next_build_id();
        let second = next_build_id();
        assert_ne!(first, second);
        assert!(first.as_str().starts_with("build:"));
    }

    #[test]
    fn requested_budgets_reject_zero_cli_values() {
        let config = RepositoryGraphConfig::default();
        assert!(requested_budget(&config, Some(0), None).is_err());
        assert!(requested_budget(&config, None, Some(0)).is_err());
    }

    #[test]
    fn evidence_locations_are_repository_relative() {
        let path = RepoPath::new("src/main.rs").unwrap();
        assert_eq!(evidence_location(Some(&path), None), "src/main.rs");
    }

    #[test]
    fn unreadable_sidecar_is_reported_as_incompatible_status() {
        let directory = tempfile::tempdir().unwrap();
        let sidecar_path = directory.path().join(SIDECAR_FILE_NAME);
        std::fs::write(&sidecar_path, b"not a sqlite database").unwrap();
        let context = LocalGraphContext {
            root: directory.path().to_path_buf(),
            repository: RepositoryRef {
                namespace: RepositoryNamespace::new("local:test").unwrap(),
                repository_id: RepositoryId::new("root").unwrap(),
            },
            config: RepositoryGraphConfig::default(),
        };

        let response = status_response_at(&context, &sidecar_path, None).unwrap();

        assert_eq!(response.data.availability, Availability::Incompatible);
        assert_eq!(response.freshness.reason_codes, ["sidecar_unreadable"]);
    }
}
