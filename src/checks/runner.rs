//! Run configured checks in order, spooling review output to disk.

use anyhow::{Context, Result};
use std::{path::Path, process::Stdio};

use crate::platform;

mod output;
pub(crate) use output::CapturedOutput;
use output::Spool;

pub struct CommandResult {
    pub command: String,
    pub passed: bool,
    pub output: CapturedOutput,
}

pub struct CheckResult {
    pub passed: bool,
    pub commands: Vec<CommandResult>,
}

/// HQ needs exit statuses only; discard output directly rather than buffering it.
pub async fn run_checks(commands: &[String]) -> Result<CheckResult> {
    run_checks_with_cwd(commands, None, None, 0).await
}

pub async fn run_checks_logged(
    commands: &[String],
    cwd: Option<&Path>,
    log_path: &Path,
    max_feedback_lines: usize,
) -> Result<CheckResult> {
    run_checks_with_cwd(commands, cwd, Some(log_path), max_feedback_lines).await
}

async fn run_checks_with_cwd(
    commands: &[String],
    cwd: Option<&Path>,
    log_path: Option<&Path>,
    max_feedback_lines: usize,
) -> Result<CheckResult> {
    let mut results = Vec::with_capacity(commands.len());
    let mut passed = true;
    for cmd in commands {
        let result = run_command(cmd, cwd, log_path, max_feedback_lines)
            .await
            .with_context(|| format!("Failed to run command: {cmd}"))?;
        passed &= result.passed;
        results.push(result);
    }
    Ok(CheckResult {
        passed,
        commands: results,
    })
}

async fn run_command(
    cmd: &str,
    cwd: Option<&Path>,
    log_path: Option<&Path>,
    max_feedback_lines: usize,
) -> Result<CommandResult> {
    if cmd.trim().is_empty() {
        return Ok(CommandResult {
            command: cmd.to_string(),
            passed: true,
            output: CapturedOutput::default(),
        });
    }
    let mut command = platform::shell_command(cmd);
    command.kill_on_drop(true);
    command.stdin(Stdio::null());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let Some(log_path) = log_path else {
        let status = command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await?;
        return Ok(CommandResult {
            command: cmd.to_string(),
            passed: status.success(),
            output: CapturedOutput::default(),
        });
    };
    let stdout_spool = Spool::new(log_path, "stdout")?;
    let stderr_spool = Spool::new(log_path, "stderr")?;
    let mut stdout_file = tokio::fs::File::from_std(stdout_spool.file().try_clone()?);
    let mut stderr_file = tokio::fs::File::from_std(stderr_spool.file().try_clone()?);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let (status, _, _) = tokio::try_join!(
        child.wait(),
        tokio::io::copy(&mut stdout, &mut stdout_file),
        tokio::io::copy(&mut stderr, &mut stderr_file),
    )?;
    // Tokio file writes may still be in flight after the last poll_write.
    use tokio::io::AsyncWriteExt;
    stdout_file.flush().await?;
    stderr_file.flush().await?;
    drop((stdout_file, stderr_file));
    let passed = status.success();
    let log_path = log_path.to_path_buf();
    let command = cmd.to_string();
    let output = tokio::task::spawn_blocking(move || {
        output::finish_log(
            &log_path,
            &command,
            passed,
            stdout_spool,
            stderr_spool,
            max_feedback_lines,
        )
    })
    .await??;
    Ok(CommandResult {
        command: cmd.to_string(),
        passed,
        output,
    })
}

#[cfg(test)]
mod tests {
    //! Check ordering, aggregate failure reporting, and empty-command behavior.

    use super::*;

    #[cfg(unix)]
    const STDIN_HELPER_ENV: &str = "FERRUS_CHECK_RUNNER_STDIN_HELPER";

    #[cfg(unix)]
    #[test]
    fn check_commands_cannot_consume_runner_stdin() {
        use std::io::Write as _;

        for mode in ["logged", "unlogged"] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "checks::runner::tests::stdin_is_closed_in_check_helper",
                    "--nocapture",
                ])
                .env(STDIN_HELPER_ENV, mode)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();

            let mut stdin = child.stdin.take().unwrap();
            stdin.write_all(b"{\"jsonrpc\":\"2.0\"}\n").unwrap();
            drop(stdin);

            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{mode} check inherited runner stdin:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdin_is_closed_in_check_helper() {
        let Ok(mode) = std::env::var(STDIN_HELPER_ENV) else {
            return;
        };
        let commands = vec!["if IFS= read -r line; then exit 42; fi".into()];
        let result = if mode == "logged" {
            let dir = tempfile::tempdir().unwrap();
            run_checks_logged(
                &commands,
                Some(dir.path()),
                &dir.path().join("checks.log"),
                30,
            )
            .await
            .unwrap()
        } else {
            run_checks(&commands).await.unwrap()
        };

        assert!(result.passed);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn logged_checks_drain_both_streams_and_keep_full_output_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("checks.log");
        let commands = vec![
            "printf 'first\\n'".into(),
            "i=0; while [ $i -lt 10000 ]; do printf 'stdout-%s\\n' \"$i\"; printf 'stderr-%s\\n' \"$i\" >&2; i=$((i + 1)); done; printf 'last failure\\n' >&2; exit 1".into(),
        ];
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            run_checks_logged(&commands, Some(dir.path()), &log_path, 3),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!result.passed);
        assert!(result.commands[0].passed);
        let output = &result.commands[1].output;
        assert_eq!(output.total_lines, 20_001);
        assert_eq!(output.tail, "stderr-9998\nstderr-9999\nlast failure");
        assert!(output.truncated);
        let log = std::fs::read_to_string(&log_path).unwrap();
        for expected in [
            "first\n",
            "stdout-0\n",
            "stdout-9999\n",
            "stderr-0\n",
            "stderr-9999\n",
            "last failure\n",
        ] {
            assert!(log.contains(expected), "missing {expected:?}");
        }
        assert!(log.contains("=== [PASS]"));
        assert!(log.contains("=== [FAIL]"));
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "spools must be removed"
        );
    }

    #[tokio::test]
    async fn logged_checks_clean_spools_after_spawn_failure() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_checks_logged(
            &["echo failure".into()],
            Some(&dir.path().join("missing-directory")),
            &dir.path().join("checks.log"),
            30,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    const PASSING_COMMAND: &str = "true";
    #[cfg(unix)]
    const FAILING_COMMAND: &str = "false";

    #[cfg(windows)]
    const PASSING_COMMAND: &str = "ver > nul";
    #[cfg(windows)]
    const FAILING_COMMAND: &str = "exit 1";

    #[tokio::test]
    async fn run_checks_with_single_passing_command() {
        let commands = vec![PASSING_COMMAND.to_string()];

        let result = run_checks(&commands).await.unwrap();

        assert!(result.passed);
        assert_eq!(result.commands.len(), 1);
        assert_eq!(result.commands[0].command, PASSING_COMMAND);
        assert!(result.commands[0].passed);
    }

    #[tokio::test]
    async fn run_checks_with_single_failing_command() {
        let commands = vec![FAILING_COMMAND.to_string()];

        let result = run_checks(&commands).await.unwrap();

        assert!(!result.passed);
        assert_eq!(result.commands.len(), 1);
        assert_eq!(result.commands[0].command, FAILING_COMMAND);
        assert!(!result.commands[0].passed);
    }

    #[tokio::test]
    async fn run_checks_with_mixed_commands_collects_all_results() {
        let commands = vec![PASSING_COMMAND.to_string(), FAILING_COMMAND.to_string()];

        let result = run_checks(&commands).await.unwrap();

        assert!(!result.passed);
        assert_eq!(result.commands.len(), 2);
        assert!(result.commands[0].passed);
        assert!(!result.commands[1].passed);
    }

    #[tokio::test]
    async fn run_checks_with_empty_command_is_a_no_op() {
        let commands = vec![String::new()];

        let result = run_checks(&commands).await.unwrap();

        assert!(result.passed);
        assert_eq!(result.commands.len(), 1);
        assert_eq!(result.commands[0].command, "");
        assert!(result.commands[0].passed);
        assert!(result.commands[0].output.tail.is_empty());
    }

    #[tokio::test]
    async fn run_checks_with_no_commands_passes() {
        let result = run_checks(&[]).await.unwrap();

        assert!(result.passed);
        assert!(result.commands.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_checks_uses_shell_parsing_for_quoted_arguments() {
        let commands = vec!["printf '%s' 'hello world'".to_string()];

        let dir = tempfile::tempdir().unwrap();
        let result = run_checks_logged(
            &commands,
            Some(dir.path()),
            &dir.path().join("checks.log"),
            30,
        )
        .await
        .unwrap();

        assert!(result.passed);
        assert_eq!(result.commands[0].output.tail, "hello world");
    }
}
