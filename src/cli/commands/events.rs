use anyhow::Result;
use clap::Subcommand;

use crate::{project, runtime_table};

#[derive(Debug, Subcommand)]
pub enum EventsCommand {
    /// List runtime event rows from ferrus.db.
    List {
        /// Maximum number of event rows to show.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Only show events for one run id.
        #[arg(long = "run-id", alias = "run", value_name = "RUN_ID")]
        run_id: Option<String>,
    },
}

pub async fn run(command: EventsCommand) -> Result<()> {
    match command {
        EventsCommand::List { limit, run_id } => list(limit, run_id).await,
    }
}

async fn list(limit: usize, run_id: Option<String>) -> Result<()> {
    let events = project::list_events(limit, run_id.clone()).await?;
    for line in runtime_table::event_lines(&events, run_id.as_deref()) {
        println!("{line}");
    }
    Ok(())
}
