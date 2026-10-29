//! Memory and federated graph CLI operations.

use super::*;

pub(super) async fn memory(command: MemoryCommand) -> Result<()> {
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

pub(super) async fn federated_search(
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

pub(super) fn federated_kind_filters(
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

pub(super) fn parse_memory_kind(value: &str) -> Result<MemoryEntityKind> {
    serde_json::from_value(serde_json::Value::String(value.to_string()))
        .with_context(|| format!("unknown project-memory kind {value:?}"))
}

pub(super) async fn federated_context(args: ContextArgs) -> Result<()> {
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
            args.run.context("A seed selector is required")?,
        )?)
    };
    if matches!(domain, ContextDomain::Memory)
        && matches!(seed, FederatedContextSeed::Repository(ContextSeed::Node(_)))
    {
        anyhow::bail!("repository node selectors require --domain repository or --domain all");
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

pub(super) fn print_federated_header(
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
