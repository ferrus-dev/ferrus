use anyhow::Result;

use super::register;
use crate::project;

pub async fn run() -> Result<()> {
    let registration = project::migrate_current_project().await?;
    println!("Registered project {}", registration.local_ref.project_id);
    println!("Wrote .ferrus/project.toml");
    println!(
        "Wrote {}",
        registration.data_dir.join("project.toml").display()
    );
    println!("Initialized {}", registration.database_path.display());
    println!("Created .ferrus/tasks/ and .ferrus/runs/");
    for message in register::migrate_legacy_mcp_configs().await? {
        println!("{message}");
    }
    register::ensure_configured_hq_mcp_configs().await?;
    Ok(())
}
