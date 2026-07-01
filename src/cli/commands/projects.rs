use anyhow::Result;
use clap::Subcommand;

use crate::{project, runtime_table};

#[derive(Debug, Subcommand)]
pub enum ProjectsCommand {
    /// List projects registered under ~/.ferrus/projects.
    List,
}

pub async fn run(command: ProjectsCommand) -> Result<()> {
    match command {
        ProjectsCommand::List => list().await,
    }
}

async fn list() -> Result<()> {
    let projects = project::list_registered_projects().await?;
    for line in runtime_table::project_lines(&projects) {
        println!("{line}");
    }
    Ok(())
}
