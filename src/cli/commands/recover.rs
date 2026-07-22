use anyhow::Result;

use crate::project;

pub async fn run(dry_run: bool, worktrees: bool) -> Result<()> {
    let recovery = if dry_run {
        project::preview_runtime_recovery().await?
    } else {
        project::recover_runtime_state().await?
    };
    println!(
        "Mode: {}",
        if dry_run {
            "dry-run (no changes)"
        } else {
            "apply"
        }
    );
    println!("Interrupted runs: {}", recovery.interrupted_runs);
    println!("Expired task leases: {}", recovery.expired_task_leases);
    let graph = if dry_run {
        crate::repository_graph_runtime::preview_graph_recovery().await
    } else {
        crate::repository_graph_runtime::recover_graph_state().await
    };
    match graph {
        Ok(graph) => {
            println!("Interrupted graph builds: {}", graph.interrupted_builds);
            println!(
                "Expired graph refresh leases: {}",
                graph.expired_refresh_leases
            );
            if !dry_run {
                println!("Graph views removed: {}", graph.removed_views);
                println!("Graph snapshots removed: {}", graph.removed_snapshots);
            }
        }
        Err(error) => {
            tracing::warn!(
                error = ?error,
                "repository graph recovery was unavailable; runtime recovery completed"
            );
            println!("Repository graph recovery: unavailable (runtime recovery completed)");
        }
    }
    if worktrees {
        if dry_run {
            let orphaned = project::preview_orphaned_worktrees().await?;
            println!("Orphaned worktrees: {orphaned}");
            println!("Worktrees removed: 0");
        } else {
            let removed = project::recover_orphaned_worktrees().await?;
            println!("Worktrees removed: {removed}");
        }
    }
    Ok(())
}
