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
