use anyhow::Result;
use clap::Subcommand;

use crate::{project, runtime_table};

#[derive(Debug, Subcommand)]
pub enum TasksCommand {
    /// List task rows from ferrus.db.
    List,
}

pub async fn run(command: TasksCommand) -> Result<()> {
    match command {
        TasksCommand::List => list().await,
    }
}

async fn list() -> Result<()> {
    let tasks = project::list_tasks().await?;
    for line in runtime_table::task_lines(&tasks) {
        println!("{line}");
    }
    Ok(())
}
