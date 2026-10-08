//! Headless standalone frontend; no HQ or orchestration modules are linked here.

use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if ferrus::repository_graph::extractors::cargo::run_parser_worker_if_requested()? {
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    ferrus::nano::standalone::run(ferrus::nano::standalone::Cli::parse()).await
}
