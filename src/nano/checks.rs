//! Managed checks reuse Ferrus log formatting and Nano's owned process backend.

use anyhow::{Result, ensure};
use std::{path::Path, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use super::{
    commands::{ChildEnvironment, ExecutionBackend, TrustedLocal, backend::exited},
    tools::Cancellation,
};
use crate::{
    checks::runner::{
        CommandResult,
        output::{Spool, finish_log},
    },
    config::Config,
};

const STREAM_BYTES: u64 = 8 * 1024 * 1024;

pub(super) async fn run(
    config: &Config,
    workspace: &Path,
    log: &Path,
    stop: &Cancellation,
) -> Result<(bool, String, String)> {
    let backend = TrustedLocal::new(workspace, ChildEnvironment::capture())?;
    let mut results = Vec::new();
    for command in &config.checks.commands {
        ensure!(!stop.is_cancelled(), "Checks interrupted");
        if command.trim().is_empty() {
            continue;
        }

        let stdout_spool = Spool::new(log, "stdout")?;
        let stderr_spool = Spool::new(log, "stderr")?;
        let mut child = backend.spawn(command, ".")?;

        let stdout = child.child.stdout.take().expect("piped stdout");
        let stderr = child.child.stderr.take().expect("piped stderr");
        let stdout_file = tokio::fs::File::from_std(stdout_spool.file().try_clone()?);
        let stderr_file = tokio::fs::File::from_std(stderr_spool.file().try_clone()?);

        let observed = async {
            while !exited(&mut child)? {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            child.tree.lock().unwrap().stop();
            Ok::<_, anyhow::Error>(child.child.wait().await?)
        };

        let result = tokio::select! {
            biased;
            _ = stop.cancelled() => Err(anyhow::anyhow!("Checks interrupted")),
            result = async { tokio::try_join!(observed, capture(stdout, stdout_file), capture(stderr, stderr_file)) } => result,
        };

        // Always stop descendants and reap the owned leader before returning.
        child.tree.lock().unwrap().stop();
        child.child.wait().await?;

        let (status, _, _) = result?;
        let log = log.to_path_buf();
        let text = command.clone();
        let max_lines = config.limits.max_feedback_lines;
        let passed = status.success();
        let output = tokio::task::spawn_blocking(move || {
            finish_log(&log, &text, passed, stdout_spool, stderr_spool, max_lines)
        })
        .await??;

        results.push(CommandResult {
            command: command.clone(),
            passed,
            output,
        });
    }

    let passed = results.iter().all(|result| result.passed);
    let report = if passed {
        "All checks passed.".into()
    } else {
        crate::server::tools::check_gate::build_report(
            &results,
            config.limits.max_feedback_lines,
            log,
        )
    };

    let failed: Vec<_> = results
        .iter()
        .filter(|result| !result.passed)
        .map(|result| result.command.as_str())
        .collect();

    Ok((
        passed,
        report.chars().take(4096).collect(),
        format!("Commands failed: {}", failed.join(", ")),
    ))
}

async fn capture(input: impl AsyncRead + Unpin, mut file: tokio::fs::File) -> Result<()> {
    let copied = tokio::io::copy(&mut input.take(STREAM_BYTES + 1), &mut file).await?;
    file.flush().await?;
    ensure!(
        copied <= STREAM_BYTES,
        "Check output exceeded the stream limit"
    );
    Ok(())
}
