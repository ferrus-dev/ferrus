use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::{agent_id::DEFAULT_AGENT_INDEX, server::Role};

pub mod commands;

#[derive(Parser)]
#[command(
    name = "ferrus",
    about = "AI orchestration MCP server — coordinates Supervisor + Executor agents",
    version = env!("CARGO_PKG_VERSION"),
)]
pub struct Cli {
    /// Enable debug mode regardless of build profile
    #[arg(long, global = true)]
    debug: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize ferrus in the current directory (creates ferrus.toml and .ferrus/)
    Init {
        /// Root directory for agent skill files (default: .agents)
        #[arg(long, default_value = ".agents")]
        agents_path: String,
    },
    /// Start the MCP server on stdio
    Serve {
        /// Filter the exposed tool set by role (omit to expose all tools)
        #[arg(long, value_enum)]
        role: Option<Role>,
        /// Human-readable agent name embedded in the claimed_by field (e.g. "codex", "claude-code")
        #[arg(long, default_value = "unknown")]
        agent_name: String,
        /// Index disambiguating multiple agents of the same role and name (e.g. 1, 2)
        #[arg(long, default_value_t = DEFAULT_AGENT_INDEX)]
        agent_index: u32,
    },
    /// Write MCP config files so agents can launch ferrus automatically
    Register {
        /// Agent to configure as Supervisor (optional if --executor is set)
        #[arg(long, value_enum, value_name = "AGENT")]
        supervisor: Option<commands::register::Agent>,
        /// Optional model override to store for the Supervisor
        #[arg(long, value_name = "MODEL")]
        supervisor_model: Option<String>,
        /// Agent to configure as Executor (optional if --supervisor is set)
        #[arg(long, value_enum, value_name = "AGENT")]
        executor: Option<commands::register::Agent>,
        /// Optional model override to store for the Executor
        #[arg(long, value_name = "MODEL")]
        executor_model: Option<String>,
    },
    /// Check that local and global ferrus project metadata are consistent
    Doctor,
    /// Migrate an existing ferrus project to the global project registry
    #[command(visible_alias = "upgrade")]
    Migrate,
    /// Recover ferrus.db runtime state after crashes or stale leases
    Recover {
        /// Show pending recovery work without mutating ferrus.db
        #[arg(long)]
        dry_run: bool,
        /// Also remove managed task worktrees that no active task or active run still owns
        #[arg(long)]
        worktrees: bool,
    },
    /// Inspect globally registered ferrus projects
    Projects {
        #[command(subcommand)]
        command: commands::projects::ProjectsCommand,
    },
    /// Inspect task runtime records from ferrus.db
    Tasks {
        #[command(subcommand)]
        command: commands::tasks::TasksCommand,
    },
    /// Inspect run attempt records from ferrus.db
    Runs {
        #[command(subcommand)]
        command: commands::runs::RunsCommand,
    },
    /// Inspect runtime event records from ferrus.db
    Events {
        #[command(subcommand)]
        command: commands::events::EventsCommand,
    },
}

impl Cli {
    pub fn debug_enabled(&self) -> bool {
        cfg!(debug_assertions) || self.debug
    }

    pub fn is_hq_mode(&self) -> bool {
        self.command.is_none()
    }

    pub async fn run(self, debug: bool) -> Result<()> {
        match self.command {
            Some(Commands::Init { agents_path }) => commands::init::run(agents_path).await,
            Some(Commands::Serve {
                role,
                agent_name,
                agent_index,
            }) => commands::serve::run(role, agent_name, agent_index, debug).await,
            Some(Commands::Register {
                supervisor,
                supervisor_model,
                executor,
                executor_model,
            }) => {
                if supervisor.is_none() && executor.is_none() {
                    anyhow::bail!("At least one of --supervisor or --executor must be specified");
                }
                commands::register::run(supervisor, supervisor_model, executor, executor_model)
                    .await
            }
            Some(Commands::Doctor) => commands::doctor::run().await,
            Some(Commands::Migrate) => commands::migrate::run().await,
            Some(Commands::Recover { dry_run, worktrees }) => {
                commands::recover::run(dry_run, worktrees).await
            }
            Some(Commands::Projects { command }) => commands::projects::run(command).await,
            Some(Commands::Tasks { command }) => commands::tasks::run(command).await,
            Some(Commands::Runs { command }) => commands::runs::run(command).await,
            Some(Commands::Events { command }) => commands::events::run(command).await,
            None => crate::hq::run(debug).await,
        }
    }
}
