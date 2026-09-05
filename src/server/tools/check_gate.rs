//! Run configured review checks and persist full logs with bounded feedback for the agent.

use anyhow::Result;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    checks::runner::{self, CommandResult},
    config::Config,
    state::store,
};

pub(super) enum CheckGateResult {
    Passed,
    Failed(CheckFailure),
}

pub(super) struct CheckFailure {
    pub failure_reason: String,
    pub report: String,
}

pub(super) async fn run(config: &Config, attempt: u32, log_scope: &str) -> Result<CheckGateResult> {
    run_with_cwd(config, attempt, log_scope, None).await
}

pub(super) async fn run_in(
    config: &Config,
    attempt: u32,
    log_scope: &str,
    cwd: &Path,
) -> Result<CheckGateResult> {
    run_with_cwd(config, attempt, log_scope, Some(cwd)).await
}

async fn run_with_cwd(
    config: &Config,
    attempt: u32,
    log_scope: &str,
    cwd: Option<&Path>,
) -> Result<CheckGateResult> {
    if config.checks.commands.is_empty() {
        return Ok(CheckGateResult::Passed);
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    static LOG_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = LOG_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let scope = format!("{log_scope}-{}-{sequence}", std::process::id());
    let log_path = store::write_check_log(attempt, ts, &scope, "").await?;
    let result = runner::run_checks_logged(
        &config.checks.commands,
        cwd,
        &log_path,
        config.limits.max_feedback_lines,
    )
    .await?;
    if result.passed {
        let _ = tokio::fs::remove_file(&log_path).await;
        return Ok(CheckGateResult::Passed);
    }

    let failed_commands: Vec<&str> = result
        .commands
        .iter()
        .filter(|c| !c.passed)
        .map(|c| c.command.as_str())
        .collect();
    let failure_reason = format!("Commands failed: {}", failed_commands.join(", "));
    let report = build_report(
        &result.commands,
        config.limits.max_feedback_lines,
        &log_path,
    );

    Ok(CheckGateResult::Failed(CheckFailure {
        failure_reason,
        report,
    }))
}

fn build_report(commands: &[CommandResult], max_lines: usize, log_path: &Path) -> String {
    let failed: Vec<&CommandResult> = commands.iter().filter(|c| !c.passed).collect();

    let mut out = String::from("Checks failed.\n\nFailed commands:\n");
    for cmd in &failed {
        out.push_str(&format!("- `{}`\n", cmd.command));
    }
    out.push('\n');

    for cmd in &failed {
        out.push_str(&format!("`{}`\n", cmd.command));
        let tail = &cmd.output.tail;
        if cmd.output.truncated {
            out.push_str(&format!("(bounded tail of {} lines; up to {max_lines} lines and 64 KiB of output retained)\n", cmd.output.total_lines));
        }
        out.push_str("```\n");
        out.push_str(tail);
        if !tail.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("```\n\n");
    }

    out.push_str(&format!("Full log: `{}`", log_path.display()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_report_uses_bounded_feedback_and_full_log_link() {
        let commands = vec![CommandResult {
            command: "check".into(),
            passed: false,
            output: crate::checks::runner::CapturedOutput {
                tail: "last failure".into(),
                total_lines: 100_000,
                truncated: true,
            },
        }];
        let report = build_report(&commands, 30, Path::new(".ferrus/logs/check.txt"));
        assert!(report.contains("last failure"));
        assert!(report.contains("100000 lines"));
        assert!(report.contains("Full log: `.ferrus/logs/check.txt`"));
        assert!(report.len() < 512);
    }
}
