use anyhow::Result;

use super::register;
use crate::project;

pub async fn run() -> Result<()> {
    let report = project::doctor_current_project().await?;
    println!("Project: {}", report.registration.metadata.name);
    println!("Project id: {}", report.registration.local_ref.project_id);
    println!("Data dir: {}", report.registration.data_dir.display());
    println!("Database: {}", report.registration.database_path.display());

    for check in &report.checks {
        let status = if check.ok { "ok" } else { "error" };
        println!("{status}: {}", check.message);
    }
    for observation in crate::repository_graph_runtime::graph_doctor_observations().await {
        let status = if observation.healthy { "ok" } else { "warning" };
        println!("{status}: {}", observation.message);
    }

    for warning in register::legacy_mcp_config_warnings().await? {
        println!("warning: {warning}");
    }
    let mcp_checks = register::configured_hq_mcp_checks().await?;
    for check in &mcp_checks {
        let status = if check.ok {
            "ok"
        } else if check.fatal {
            "error"
        } else {
            "warning"
        };
        println!("{status}: {}", check.message);
    }

    if report.has_errors() || mcp_checks.iter().any(|check| check.fatal) {
        anyhow::bail!("ferrus doctor found project registration issues");
    }
    Ok(())
}
