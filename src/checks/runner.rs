//! Run configured checks in order and capture each command's exit status and output.

use anyhow::{Context, Result};
use std::path::Path;

use crate::platform;

pub struct CommandResult {
    pub command: String,
    pub passed: bool,
    pub stdout: String,
    pub stderr: String,
}

pub struct CheckResult {
    pub passed: bool,
    pub commands: Vec<CommandResult>,
}

/// Run every configured check command in order, collecting stdout/stderr for each.
pub async fn run_checks(commands: &[String]) -> Result<CheckResult> {
    run_checks_with_cwd(commands, None).await
}

pub async fn run_checks_in(commands: &[String], cwd: &Path) -> Result<CheckResult> {
    run_checks_with_cwd(commands, Some(cwd)).await
}

async fn run_checks_with_cwd(commands: &[String], cwd: Option<&Path>) -> Result<CheckResult> {
    let mut results = Vec::with_capacity(commands.len());
    let mut passed = true;

    for cmd in commands {
        let result = run_command(cmd, cwd)
            .await
            .with_context(|| format!("Failed to spawn command: {cmd}"))?;
        if !result.passed {
            passed = false;
        }
        results.push(result);
    }

    Ok(CheckResult {
        passed,
        commands: results,
    })
}

async fn run_command(cmd: &str, cwd: Option<&Path>) -> Result<CommandResult> {
    if cmd.trim().is_empty() {
        return Ok(CommandResult {
            command: cmd.to_string(),
            passed: true,
            stdout: String::new(),
            stderr: String::new(),
        });
    }

    let mut command = platform::shell_command(cmd);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command
        .output()
        .await
        .with_context(|| format!("Failed to run check command `{cmd}`"))?;

    Ok(CommandResult {
        command: cmd.to_string(),
        passed: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    //! Check ordering, aggregate failure reporting, and empty-command behavior.

    use super::*;

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
        assert!(result.commands[0].stdout.is_empty());
        assert!(result.commands[0].stderr.is_empty());
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

        let result = run_checks(&commands).await.unwrap();

        assert!(result.passed);
        assert_eq!(result.commands[0].stdout, "hello world");
    }
}
