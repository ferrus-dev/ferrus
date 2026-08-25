use std::{
    num::{NonZeroU32, NonZeroU64},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::{
    project,
    project_memory::{
        domain::{MemoryEntityId, MemoryEntityKind, MemoryQueryText, MemoryRecordId},
        federation::{
            ContextDomain, FederatedContextItem, FederatedContextRequest, FederatedContextSeed,
            FederatedSearchRequest, FederatedSearchResult,
        },
        index::{MemoryIndexOptions, index_current_project},
        query::MemoryContextPolicy,
    },
    project_memory_runtime::LocalProjectContext,
    repository_graph::{
        config::RepositoryGraphConfig,
        domain::{
            Availability, BuildId, EdgeTarget, Freshness, NodeId, PublishedViewName, QueryBudget,
            RepoPath, SemanticKey,
        },
        health::{SidecarHealth, inspect_health_at},
        index::{IndexCoordinator, IndexOutcome, IndexRequest},
        maintenance::RefreshLeaseOutcome,
        ports::GraphQuery,
        query::{
            ContextPolicy, ContextRequest, ContextSeed, DiagnosticsEnvelope, EdgeDirection,
            FreshnessEnvelope, NeighborhoodRequest, PageInfo, PageRequest, SearchRequest,
            ShowLookup, ShowRequest, StatusResponse,
        },
        query_sqlite::{SqliteGraphQuery, default_budget},
        sqlite::{
            OpenQuerySidecarResult, OpenSidecarResult, Sidecar, open_for_build_at,
            open_for_query_at,
        },
        store::PublicationOutcome,
    },
    repository_graph_runtime::{
        CANONICAL_VIEW, LocalGraphContext, REFRESH_LEASE_TTL, sidecar_path, status_response_at,
    },
};

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
    /// Build or inspect the independent local project-memory index.
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
    /// Search repository structure, project memory, or both.
    Search {
        query: String,
        /// Select repository structure, project memory, or both domains.
        #[arg(long, value_enum, default_value_t = GraphDomain::Repository)]
        domain: GraphDomain,
        /// Restrict results to a repository node or memory entity kind; may be repeated.
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
    /// Assemble deterministic evidence-backed repository and/or memory context.
    Context(ContextArgs),
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

#[derive(Debug, Args)]
#[group(skip)]
pub struct ContextArgs {
    /// Select repository structure, project memory, or both domains.
    #[arg(long, value_enum, default_value_t = GraphDomain::Repository)]
    domain: GraphDomain,
    /// Seed context with one opaque node id.
    #[arg(long)]
    node: Option<String>,
    /// Seed context with one exact semantic key.
    #[arg(long)]
    symbol: Option<String>,
    /// Seed context with one exact repository-relative evidence path.
    #[arg(long)]
    path: Option<String>,
    /// Seed project memory with one exact memory entity id.
    #[arg(long = "memory-entity")]
    memory_entity: Option<String>,
    /// Seed project memory with one stable milestone id.
    #[arg(long)]
    milestone: Option<String>,
    /// Seed project memory with one task id.
    #[arg(long)]
    task: Option<String>,
    /// Seed project memory with one run id.
    #[arg(long)]
    run: Option<String>,
    /// Requested expansion depth; the configured service cap still applies.
    #[arg(long)]
    depth: Option<u32>,
    /// Requested result count; the configured service cap still applies.
    #[arg(long = "max-results")]
    max_results: Option<u32>,
    /// Requested response bytes; the configured service cap still applies.
    #[arg(long = "max-bytes")]
    max_bytes: Option<u64>,
    /// Emit one machine-readable JSON document.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Subcommand)]
pub enum MemoryCommand {
    /// Build or incrementally update the local project-memory index.
    Index {
        /// Ignore cached source fragments.
        #[arg(long)]
        full: bool,
        /// Emit one machine-readable JSON document.
        #[arg(long)]
        json: bool,
    },
    /// Inspect project-memory availability, freshness, policy, and counts.
    Status {
        /// Emit one machine-readable JSON document.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum GraphDomain {
    #[default]
    Repository,
    Memory,
    All,
}

impl From<GraphDomain> for ContextDomain {
    fn from(value: GraphDomain) -> Self {
        match value {
            GraphDomain::Repository => Self::Repository,
            GraphDomain::Memory => Self::Memory,
            GraphDomain::All => Self::All,
        }
    }
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
    project::prepare_runtime_database_for_read_only_operations().await?;
    match command {
        GraphCommand::Index { full, json } => index(full, json).await,
        GraphCommand::Status { json } => status(json).await,
        GraphCommand::Memory { command } => memory(command).await,
        GraphCommand::Search {
            query,
            domain,
            kinds,
            path,
            limit,
            json,
        } => search(query, domain, kinds, path, limit, json).await,
        GraphCommand::Show(args) => show(args).await,
        GraphCommand::Context(args) => context(args).await,
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

#[derive(Serialize)]
struct IndexOutput {
    status: &'static str,
    freshness: Freshness,
    outcome: IndexOutcome,
}

async fn index(full: bool, json: bool) -> Result<()> {
    let context = LocalGraphContext::load(true).await?;
    let refresh_guard = project::canonical_graph_refresh_guard().await?;
    // Indexing always publishes the canonical view. A managed Executor may invoke
    // the CLI from its task worktree, but unapproved task contents must remain in
    // that task's overlay publication until approval.
    let source = context.discover_canonical()?;
    let mut sidecar = match open_for_build_at(&sidecar_path().await?)? {
        OpenSidecarResult::Ready(sidecar) => sidecar,
        OpenSidecarResult::RequiresRebuild(reason) => anyhow::bail!(
            "repository graph storage is incompatible (schema {} vs {}): {}; remove the derived sidecar and retry",
            reason.found_schema_version,
            reason.supported_schema_version,
            reason.reason
        ),
    };
    let build_id = next_build_id();
    let view_name = PublishedViewName::new(CANONICAL_VIEW)?;
    if sidecar.acquire_refresh_lease(
        &context.repository,
        &view_name,
        build_id.as_str(),
        REFRESH_LEASE_TTL,
    )? == RefreshLeaseOutcome::Busy
    {
        anyhow::bail!("canonical repository graph refresh is already in progress");
    }
    let heartbeat = sidecar.start_refresh_lease_heartbeat(
        &context.repository,
        &view_name,
        build_id.as_str(),
        REFRESH_LEASE_TTL,
    )?;
    let indexed = IndexCoordinator::new(&mut sidecar).index(
        &source,
        &context.config,
        IndexRequest {
            build_id: build_id.clone(),
            view_name: view_name.clone(),
            force_full: full,
        },
    );
    let outcome = match indexed {
        Ok(outcome) => outcome,
        Err(error) => {
            let _ = heartbeat.finish();
            let _ =
                sidecar.release_refresh_lease(&context.repository, &view_name, build_id.as_str());
            return Err(error.into());
        }
    };
    let lease_healthy = heartbeat.finish();
    if !lease_healthy
        || !sidecar.release_refresh_lease(&context.repository, &view_name, build_id.as_str())?
    {
        anyhow::bail!("canonical repository graph refresh lease was lost");
    }
    let source_identity = project::CanonicalSourceIdentity {
        source_revision_id: source.manifest().revision.id.clone(),
        manifest_digest: source.manifest().revision.manifest_digest.clone(),
    };
    let publication_won = publication_matches_snapshot(&outcome.publication, &outcome.snapshot.id);
    let durable_refresh_recorded = if publication_won {
        match project::record_canonical_graph_refresh(
            None,
            None,
            refresh_guard,
            &source_identity,
            &outcome.snapshot.id,
            &outcome.build_id,
        )
        .await
        {
            Ok(project::CanonicalGraphRefreshOutcome::Recorded) => true,
            Ok(project::CanonicalGraphRefreshOutcome::Superseded) => {
                tracing::warn!(
                    "canonical graph was indexed but a newer source invalidation remains pending"
                );
                false
            }
            Err(error) => {
                tracing::warn!(
                    error = ?error,
                    "canonical graph indexed but durable freshness state was not updated"
                );
                false
            }
        }
    } else {
        tracing::warn!(
            "canonical graph index was superseded; durable freshness state was left unchanged"
        );
        false
    };
    let freshness = reported_index_freshness(publication_won, durable_refresh_recorded);
    crate::repository_graph_runtime::maintain_graph_best_effort().await;
    if json {
        print_json(&IndexOutput {
            status: "indexed",
            freshness,
            outcome,
        })?;
    } else {
        println!("Indexed repository graph");
        println!("Snapshot: {}", outcome.snapshot.id);
        println!(
            "Freshness: {}",
            if freshness == Freshness::Fresh {
                "fresh"
            } else {
                "unknown"
            }
        );
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

fn reported_index_freshness(publication_won: bool, durable_refresh_recorded: bool) -> Freshness {
    if publication_won && durable_refresh_recorded {
        Freshness::Fresh
    } else {
        Freshness::Unknown
    }
}

fn publication_matches_snapshot(
    publication: &PublicationOutcome,
    snapshot_id: &crate::repository_graph::domain::SnapshotId,
) -> bool {
    matches!(
        publication,
        PublicationOutcome::Published { view } if &view.snapshot_id == snapshot_id
    )
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

async fn search(
    text: String,
    domain: GraphDomain,
    kinds: Vec<String>,
    path: Option<String>,
    limit: Option<u32>,
    json: bool,
) -> Result<()> {
    if !matches!(domain, GraphDomain::Repository) {
        return federated_search(text, domain, kinds, path, limit, json).await;
    }
    let context = LocalGraphContext::load(true).await?;
    let sidecar = ready_query_sidecar().await?;
    let query = SqliteGraphQuery::new(
        &sidecar,
        context.config.query_limits.clone(),
        context.freshness_comparison()?,
    );
    let response = query.search(&SearchRequest {
        scope: context.scope(requested_budget(&context.config, limit, None, None)?)?,
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

async fn memory(command: MemoryCommand) -> Result<()> {
    match command {
        MemoryCommand::Index { full, json } => {
            let started = std::time::Instant::now();
            let outcome = index_current_project(MemoryIndexOptions { full }).await?;
            if json {
                print_json(&outcome)?;
            } else {
                println!("Indexed project memory");
                println!("Revision: {}", outcome.revision.id);
                println!(
                    "Sources: {} discovered, {} reused, {} extracted, {} skipped, {} failed",
                    outcome.metrics.discovered_sources,
                    outcome.metrics.reused_sources,
                    outcome.metrics.extracted_sources,
                    outcome.metrics.skipped_sources,
                    outcome.metrics.failed_sources
                );
                println!(
                    "Facts: {} entities, {} relationships, {} diagnostics",
                    outcome.metrics.entities,
                    outcome.metrics.relationships,
                    outcome.metrics.diagnostics
                );
            }
            tracing::info!(
                target: "ferrus::project_memory::index",
                revision_id = outcome.revision.id.as_str(),
                build_id = outcome.build_id.as_str(),
                duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                discovered_sources = outcome.metrics.discovered_sources,
                reused_sources = outcome.metrics.reused_sources,
                extracted_sources = outcome.metrics.extracted_sources,
                entities = outcome.metrics.entities,
                relationships = outcome.metrics.relationships,
                diagnostics = outcome.metrics.diagnostics,
                "project memory index"
            );
            Ok(())
        }
        MemoryCommand::Status { json } => {
            let context = LocalProjectContext::load_for_cli(false).await?;
            let response = context.memory_status(context.default_budget()?)?;
            if json {
                print_json(&response)?;
            } else {
                println!("Project memory: {:?}", response.data.availability);
                println!(
                    "Revision: {}",
                    response
                        .revision_id
                        .as_ref()
                        .map_or("none", |id| id.as_str())
                );
                println!("Freshness: {:?}", response.freshness.freshness);
                if let Some(statistics) = response.data.statistics {
                    println!(
                        "Facts: {} sources, {} entities, {} relationships, {} stale links",
                        statistics.sources,
                        statistics.entities,
                        statistics.relationships,
                        statistics.stale_links
                    );
                }
                if let Some(retention) = response.data.retention {
                    println!(
                        "Retention: {} revisions ({} historical), {} builds ({} terminal unpublished), {} repository link sets",
                        retention.revisions,
                        retention.historical_revisions,
                        retention.builds,
                        retention.terminal_unpublished_builds,
                        retention.repository_link_sets
                    );
                }
                if response.data.recommended_action.is_some() {
                    println!(
                        "Next: ferrus graph memory index{}",
                        if response.data.availability
                            == crate::project_memory::query::MemoryAvailability::Incompatible
                        {
                            " --full"
                        } else {
                            ""
                        }
                    );
                }
            }
            Ok(())
        }
    }
}

async fn federated_search(
    text: String,
    domain: GraphDomain,
    kinds: Vec<String>,
    path: Option<String>,
    limit: Option<u32>,
    json: bool,
) -> Result<()> {
    let (effective_domain, repository_kinds, memory_kinds) = federated_kind_filters(domain, kinds)?;
    let context = LocalProjectContext::load_for_cli(matches!(
        effective_domain,
        GraphDomain::Repository | GraphDomain::All
    ))
    .await?;
    let budget = context.requested_budget(limit, None, None, None, None, None)?;
    if matches!(effective_domain, GraphDomain::Memory) && path.is_some() {
        anyhow::bail!("--path scopes repository results and requires --domain repository or all");
    }
    let response = context.search(FederatedSearchRequest {
        scope: context.scope(effective_domain.into(), budget)?,
        text: MemoryQueryText::new(text)?,
        repository_kinds,
        repository_paths: path
            .map(RepoPath::new)
            .transpose()
            .context("--path must be repository-relative")?
            .into_iter()
            .collect(),
        memory_kinds,
        memory_sources: vec![],
        cursor: None,
    })?;
    if json {
        print_json(&response)?;
    } else {
        print_federated_header(&response.repository, &response.memory);
        for result in response.results {
            match result {
                FederatedSearchResult::Repository(hit) => println!(
                    "repository {:.2} {} {}",
                    hit.score,
                    hit.kind,
                    hit.semantic_key
                        .as_ref()
                        .map_or(hit.node_id.as_str(), |key| key.as_str())
                ),
                FederatedSearchResult::Memory(hit) => println!(
                    "memory {:.2} {} {}",
                    hit.score,
                    hit.entity.data.kind().as_str(),
                    hit.entity.id
                ),
            }
        }
    }
    Ok(())
}

fn federated_kind_filters(
    domain: GraphDomain,
    kinds: Vec<String>,
) -> Result<(
    GraphDomain,
    Vec<crate::project_memory::domain::MemoryStatusToken>,
    Vec<MemoryEntityKind>,
)> {
    let has_kind_filters = !kinds.is_empty();
    let mut memory_kinds = Vec::new();
    let mut repository_kinds = Vec::new();
    for kind in kinds {
        match domain {
            GraphDomain::Memory => memory_kinds.push(parse_memory_kind(&kind)?),
            GraphDomain::All => match parse_memory_kind(&kind) {
                Ok(kind) => memory_kinds.push(kind),
                Err(_) => repository_kinds
                    .push(crate::project_memory::domain::MemoryStatusToken::new(kind)?),
            },
            GraphDomain::Repository => unreachable!("repository search uses the legacy path"),
        }
    }
    let effective_domain = if matches!(domain, GraphDomain::All) && has_kind_filters {
        match (repository_kinds.is_empty(), memory_kinds.is_empty()) {
            (true, false) => GraphDomain::Memory,
            (false, true) => GraphDomain::Repository,
            _ => GraphDomain::All,
        }
    } else {
        domain
    };
    Ok((effective_domain, repository_kinds, memory_kinds))
}

fn parse_memory_kind(value: &str) -> Result<MemoryEntityKind> {
    serde_json::from_value(serde_json::Value::String(value.to_string()))
        .with_context(|| format!("unknown project-memory kind {value:?}"))
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

async fn context(args: ContextArgs) -> Result<()> {
    if usize::from(args.node.is_some())
        + usize::from(args.symbol.is_some())
        + usize::from(args.path.is_some())
        + usize::from(args.memory_entity.is_some())
        + usize::from(args.milestone.is_some())
        + usize::from(args.task.is_some())
        + usize::from(args.run.is_some())
        != 1
    {
        anyhow::bail!(
            "graph context requires exactly one seed selector: --node, --symbol, --path, --memory-entity, --milestone, --task, or --run"
        );
    }
    if !matches!(args.domain, GraphDomain::Repository) {
        return federated_context(args).await;
    }
    if args.memory_entity.is_some()
        || args.milestone.is_some()
        || args.task.is_some()
        || args.run.is_some()
    {
        anyhow::bail!("project-memory seed selectors require --domain memory or --domain all");
    }
    let context = LocalGraphContext::load(true).await?;
    let seed = if let Some(node) = args.node {
        ContextSeed::Node(NodeId::new(node)?)
    } else if let Some(symbol) = args.symbol {
        ContextSeed::Symbol(SemanticKey::new(symbol)?)
    } else {
        ContextSeed::Path(
            RepoPath::new(args.path.expect("one context seed is required"))
                .context("--path must be repository-relative")?,
        )
    };
    let sidecar = ready_query_sidecar().await?;
    let query = SqliteGraphQuery::new(
        &sidecar,
        context.config.query_limits.clone(),
        context.freshness_comparison()?,
    );
    let response = query.context(&ContextRequest {
        scope: context.scope(requested_budget(
            &context.config,
            args.max_results,
            args.max_bytes,
            args.depth,
        )?)?,
        seeds: vec![seed],
        policy: ContextPolicy {
            direction: EdgeDirection::Both,
            edge_kinds: vec![],
            include_unresolved: false,
            include_external: false,
        },
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
        for item in &response.data.items {
            let reasons = item
                .selection_reasons
                .iter()
                .map(|reason| format!("{:?}", reason.kind))
                .collect::<Vec<_>>()
                .join(",");
            println!(
                "{} {} {}",
                item.kind,
                item.semantic_key
                    .as_ref()
                    .map_or(item.node_id.as_str(), |key| key.as_str()),
                evidence_location(Some(&item.path), item.span.as_ref())
            );
            println!(
                "  selected={} resolution={:?} confidence={:?} extractor={}@{}",
                reasons,
                item.provenance.resolution,
                item.provenance.confidence,
                item.provenance.extractor.id,
                item.provenance.extractor.version
            );
        }
    }
    Ok(())
}

async fn federated_context(args: ContextArgs) -> Result<()> {
    let domain: ContextDomain = args.domain.into();
    let seed = if let Some(node) = args.node {
        FederatedContextSeed::Repository(ContextSeed::Node(NodeId::new(node)?))
    } else if let Some(symbol) = args.symbol {
        FederatedContextSeed::Repository(ContextSeed::Symbol(SemanticKey::new(symbol)?))
    } else if let Some(path) = args.path {
        FederatedContextSeed::Repository(ContextSeed::Path(
            RepoPath::new(path).context("--path must be repository-relative")?,
        ))
    } else if let Some(entity) = args.memory_entity {
        FederatedContextSeed::MemoryEntity(MemoryEntityId::new(entity)?)
    } else if let Some(milestone) = args.milestone {
        FederatedContextSeed::Milestone(MemoryRecordId::new(milestone)?)
    } else if let Some(task) = args.task {
        FederatedContextSeed::Task(MemoryRecordId::new(task)?)
    } else {
        FederatedContextSeed::Run(MemoryRecordId::new(
            args.run.expect("one seed selector is required"),
        )?)
    };
    if matches!(domain, ContextDomain::Memory)
        && matches!(seed, FederatedContextSeed::Repository(_))
    {
        anyhow::bail!("repository seed selectors require --domain repository or --domain all");
    }
    let context = LocalProjectContext::load_for_cli(matches!(domain, ContextDomain::All)).await?;
    let budget = context.requested_budget(
        args.max_results,
        args.max_bytes,
        None,
        args.depth,
        None,
        None,
    )?;
    let response = context.context(FederatedContextRequest {
        scope: context.scope(domain, budget)?,
        seeds: vec![seed],
        repository_policy: ContextPolicy {
            direction: EdgeDirection::Both,
            edge_kinds: vec![],
            include_unresolved: false,
            include_external: false,
        },
        memory_policy: MemoryContextPolicy {
            direction: EdgeDirection::Both,
            relationship_kinds: vec![],
            include_unresolved: false,
            include_stale: false,
            include_snippets: false,
        },
        cursor: None,
    })?;
    if args.json {
        print_json(&response)?;
    } else {
        print_federated_header(&response.repository, &response.memory);
        for item in response.items {
            match item {
                FederatedContextItem::Repository(item) => println!(
                    "repository {} {} {}",
                    item.kind,
                    item.semantic_key
                        .as_ref()
                        .map_or(item.node_id.as_str(), |key| key.as_str()),
                    item.path
                ),
                FederatedContextItem::Memory(item) => println!(
                    "memory {} {}",
                    item.entity.data.kind().as_str(),
                    item.entity.id
                ),
            }
        }
    }
    Ok(())
}

fn print_federated_header(
    repository: &Option<crate::project_memory::federation::RepositoryDomainState>,
    memory: &Option<crate::project_memory::federation::MemoryDomainState>,
) {
    if let Some(repository) = repository {
        println!(
            "Repository: snapshot={} freshness={:?}",
            repository
                .snapshot_id
                .as_ref()
                .map_or("none", |id| id.as_str()),
            repository.freshness.freshness
        );
    }
    if let Some(memory) = memory {
        println!(
            "Memory: revision={} freshness={:?}",
            memory.revision_id.as_ref().map_or("none", |id| id.as_str()),
            memory.freshness.freshness
        );
    }
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
        scope: context.scope(requested_budget(&context.config, limit, None, depth)?)?,
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

fn requested_budget(
    config: &RepositoryGraphConfig,
    results: Option<u32>,
    bytes: Option<u64>,
    depth: Option<u32>,
) -> Result<QueryBudget> {
    let defaults = &config.query_limits;
    Ok(QueryBudget::new(
        NonZeroU32::new(results.unwrap_or(defaults.max_results))
            .context("--limit must be greater than zero")?,
        NonZeroU64::new(bytes.unwrap_or(defaults.max_bytes))
            .context("--max-bytes must be greater than zero")?,
        NonZeroU32::new(depth.unwrap_or(defaults.max_depth))
            .context("--depth must be greater than zero")?,
        NonZeroU64::new(defaults.max_duration_ms)
            .context("repository_graph.query_limits.max_duration_ms must be greater than zero")?,
        NonZeroU32::new(defaults.max_diagnostics)
            .context("repository_graph.query_limits.max_diagnostics must be greater than zero")?,
    ))
}

fn print_query_header(
    snapshot: &str,
    freshness: &FreshnessEnvelope,
    diagnostics: &DiagnosticsEnvelope,
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

fn print_diagnostics(diagnostics: &DiagnosticsEnvelope) {
    let summary = &diagnostics.summary;
    println!(
        "Diagnostics: {} info, {} warnings, {} errors",
        summary.info, summary.warning, summary.error
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
#[path = "graph_tests.rs"]
mod tests;
