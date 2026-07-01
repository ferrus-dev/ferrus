use anyhow::Result;
use clap::Subcommand;

use crate::{project, runtime_table};

#[derive(Debug, Subcommand)]
pub enum RunsCommand {
    /// List run attempt rows from ferrus.db.
    List {
        /// Maximum number of run rows to show.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

pub async fn run(command: RunsCommand) -> Result<()> {
    match command {
        RunsCommand::List { limit } => list(limit).await,
    }
}

async fn list(limit: usize) -> Result<()> {
    let runs = project::list_runs(limit).await?;
    for line in runtime_table::run_lines(&runs) {
        println!("{line}");
    }
    Ok(())
}
