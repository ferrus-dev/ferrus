use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "ferrus-hq",
    no_binary_name = true,
    disable_help_subcommand = true
)]
struct HqCli {
    #[command(subcommand)]
    command: ShellCommand,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ModelTarget {
    Supervisor,
    Executor,
}

#[derive(Debug, Subcommand)]
pub enum ShellCommand {
    /// Show task state and agent list.
    Status,
    /// List task runtime rows from ferrus.db.
    Tasks,
    /// Plan a batch run from ready milestones in the selected spec.
    Run {
        /// Maximum number of ready milestones to include.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// List run attempts from ferrus.db.
    Runs {
        /// Maximum number of run rows to show.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// List runtime events from ferrus.db.
    Events {
        /// Maximum number of event rows to show.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Only show events for one run id.
        #[arg(long = "run-id", alias = "run", value_name = "RUN_ID")]
        run_id: Option<String>,
    },
    /// Run the Ferrus /check gate deterministically from HQ.
    Check {
        /// Run checks directly from HQ without requiring Executing or Addressing state.
        #[arg(long)]
        force: bool,
    },
    /// Reset all task files and set state to Idle (prompts for confirmation if state is Executing or Reviewing).
    Reset,
    /// Stop all running executor and supervisor/reviewer sessions (prompts for confirmation).
    Stop,
    /// Exit HQ.
    Quit,
    /// Free-form planning session with the supervisor (no task created, no state requirement).
    Plan,
    /// Queue one task with the supervisor, then run the SQLite scheduler.
    Task {
        /// Ignore the selected spec and define a free-form task.
        #[arg(long)]
        manual: bool,
    },
    /// Select the current spec without creating a task.
    Milestones,
    /// Clear the selected spec.
    ResetSpec,
    /// Draft and approve a feature specification with the supervisor.
    Spec,
    /// Open an interactive supervisor session (no initial prompt, no state requirement).
    Supervisor,
    /// Open an interactive executor session (no initial prompt, no state requirement).
    Executor,
    /// Resume the executor headlessly for the current task (escape hatch; also recovers Consultation).
    Resume,
    /// Show the log path for a running background session.
    Attach { name: String },
    /// Manually spawn supervisor in review mode (for the current Reviewing submission).
    Review,
    /// Initialize ferrus in the current directory.
    Init {
        #[arg(long, default_value = ".agents")]
        agents_path: String,
    },
    /// Register agents (same as `ferrus register`).
    Register {
        #[arg(long, value_name = "AGENT")]
        supervisor: Option<String>,
        #[arg(long, value_name = "MODEL")]
        supervisor_model: Option<String>,
        #[arg(long, value_name = "AGENT")]
        executor: Option<String>,
        #[arg(long, value_name = "MODEL")]
        executor_model: Option<String>,
    },
    /// Update the configured supervisor or executor model override.
    Model {
        #[arg(value_enum)]
        target: ModelTarget,
        #[arg(value_name = "MODEL", conflicts_with = "clear")]
        model: Option<String>,
        #[arg(long, conflicts_with = "model")]
        clear: bool,
    },
    /// Show all available HQ commands.
    Help,
}

/// Parse `/command [args…]` into a `ShellCommand`.
pub fn parse_command(input: &str) -> Result<ShellCommand> {
    let input = input.trim();
    if !input.starts_with('/') {
        bail!("Commands must start with '/' — try /status, /task, /quit");
    }
    let tokens = shlex::split(&input[1..])
        .ok_or_else(|| anyhow::anyhow!("Failed to tokenize command (unterminated quote?)"))?;
    let cli = HqCli::try_parse_from(tokens).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(cli.command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_quit() {
        assert!(matches!(
            parse_command("/quit").unwrap(),
            ShellCommand::Quit
        ));
    }
    #[test]
    fn parse_status() {
        assert!(matches!(
            parse_command("/status").unwrap(),
            ShellCommand::Status
        ));
    }
    #[test]
    fn parse_tasks() {
        assert!(matches!(
            parse_command("/tasks").unwrap(),
            ShellCommand::Tasks
        ));
    }
    #[test]
    fn parse_run() {
        assert!(matches!(
            parse_command("/run").unwrap(),
            ShellCommand::Run { limit: None }
        ));
    }
    #[test]
    fn parse_run_limit() {
        assert!(matches!(
            parse_command("/run --limit 3").unwrap(),
            ShellCommand::Run { limit: Some(3) }
        ));
    }
    #[test]
    fn parse_runs() {
        assert!(matches!(
            parse_command("/runs --limit 5").unwrap(),
            ShellCommand::Runs { limit: 5 }
        ));
    }
    #[test]
    fn parse_events() {
        match parse_command("/events --limit 7 --run r-123").unwrap() {
            ShellCommand::Events { limit, run_id } => {
                assert_eq!(limit, 7);
                assert_eq!(run_id.as_deref(), Some("r-123"));
            }
            _ => panic!("expected Events"),
        }
    }
    #[test]
    fn parse_check() {
        assert!(matches!(
            parse_command("/check").unwrap(),
            ShellCommand::Check { force: false }
        ));
    }
    #[test]
    fn parse_check_force() {
        assert!(matches!(
            parse_command("/check --force").unwrap(),
            ShellCommand::Check { force: true }
        ));
    }
    #[test]
    fn parse_reset() {
        assert!(matches!(
            parse_command("/reset").unwrap(),
            ShellCommand::Reset
        ));
    }
    #[test]
    fn parse_stop() {
        assert!(matches!(
            parse_command("/stop").unwrap(),
            ShellCommand::Stop
        ));
    }
    #[test]
    fn parse_plan() {
        assert!(matches!(
            parse_command("/plan").unwrap(),
            ShellCommand::Plan
        ));
    }
    #[test]
    fn parse_review() {
        assert!(matches!(
            parse_command("/review").unwrap(),
            ShellCommand::Review
        ));
    }
    #[test]
    fn parse_task() {
        assert!(matches!(
            parse_command("/task").unwrap(),
            ShellCommand::Task { manual: false }
        ));
    }
    #[test]
    fn parse_task_manual() {
        assert!(matches!(
            parse_command("/task --manual").unwrap(),
            ShellCommand::Task { manual: true }
        ));
    }
    #[test]
    fn parse_milestones() {
        assert!(matches!(
            parse_command("/milestones").unwrap(),
            ShellCommand::Milestones
        ));
    }
    #[test]
    fn parse_reset_spec() {
        assert!(matches!(
            parse_command("/reset-spec").unwrap(),
            ShellCommand::ResetSpec
        ));
    }
    #[test]
    fn parse_spec() {
        assert!(matches!(
            parse_command("/spec").unwrap(),
            ShellCommand::Spec
        ));
    }
    #[test]
    fn parse_supervisor_cmd() {
        assert!(matches!(
            parse_command("/supervisor").unwrap(),
            ShellCommand::Supervisor
        ));
    }
    #[test]
    fn parse_executor_cmd() {
        assert!(matches!(
            parse_command("/executor").unwrap(),
            ShellCommand::Executor
        ));
    }
    #[test]
    fn parse_resume() {
        assert!(matches!(
            parse_command("/resume").unwrap(),
            ShellCommand::Resume
        ));
    }
    #[test]
    fn parse_model() {
        match parse_command("/model supervisor claude-opus-4.6").unwrap() {
            ShellCommand::Model {
                target,
                model,
                clear,
            } => {
                assert_eq!(target, ModelTarget::Supervisor);
                assert_eq!(model.as_deref(), Some("claude-opus-4.6"));
                assert!(!clear);
            }
            _ => panic!("expected Model"),
        }
    }
    #[test]
    fn parse_model_clear() {
        match parse_command("/model executor --clear").unwrap() {
            ShellCommand::Model {
                target,
                model,
                clear,
            } => {
                assert_eq!(target, ModelTarget::Executor);
                assert_eq!(model, None);
                assert!(clear);
            }
            _ => panic!("expected Model"),
        }
    }
    #[test]
    fn execute_command_removed() {
        assert!(parse_command("/execute").is_err());
    }
    #[test]
    fn parse_attach_with_name() {
        match parse_command("/attach executor").unwrap() {
            ShellCommand::Attach { name } => assert_eq!(name, "executor"),
            _ => panic!("expected Attach"),
        }
    }
    #[test]
    fn attach_parses_structured_name() {
        match parse_command("/attach executor:codex:1").unwrap() {
            ShellCommand::Attach { name } => assert_eq!(name, "executor:codex:1"),
            _ => panic!("expected Attach"),
        }
    }
    #[test]
    fn unknown_command_errors() {
        assert!(parse_command("/foobar").is_err());
    }
    #[test]
    fn non_slash_errors() {
        assert!(parse_command("hello").is_err());
    }
}
