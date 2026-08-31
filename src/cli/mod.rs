use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::{agent_id::DEFAULT_AGENT_INDEX, server::Role};

pub mod commands;

#[derive(Parser)]
#[command(
    name = "ferrus",
    about = "AI orchestration MCP server -- coordinates Supervisor + Executor agents",
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
    /// Build and inspect the optional local repository graph
    Graph {
        #[command(subcommand)]
        command: commands::graph::GraphCommand,
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
            Some(Commands::Graph { command }) => commands::graph::run(command).await,
            None => crate::hq::run(debug).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::commands::graph::{Direction, GraphCommand, GraphDomain, MemoryCommand};

    #[test]
    fn graph_search_surface_parses_scriptable_filters() {
        let cli = Cli::try_parse_from([
            "ferrus",
            "graph",
            "search",
            "RuntimeTaskContext",
            "--kind",
            "struct",
            "--path",
            "src",
            "--limit",
            "20",
            "--json",
        ])
        .unwrap();
        let Some(Commands::Graph {
            command:
                GraphCommand::Search {
                    query,
                    domain,
                    kinds,
                    path,
                    limit,
                    json,
                },
        }) = cli.command
        else {
            panic!("graph search did not parse");
        };
        assert_eq!(query, "RuntimeTaskContext");
        assert!(matches!(domain, GraphDomain::Repository));
        assert_eq!(kinds, ["struct"]);
        assert_eq!(path.as_deref(), Some("src"));
        assert_eq!(limit, Some(20));
        assert!(json);
    }

    #[test]
    fn graph_memory_and_explicit_domain_surfaces_parse() {
        let memory = Cli::try_parse_from(["ferrus", "graph", "memory", "index", "--full"]).unwrap();
        assert!(matches!(
            memory.command,
            Some(Commands::Graph {
                command: GraphCommand::Memory {
                    command: MemoryCommand::Index {
                        full: true,
                        json: false
                    }
                }
            })
        ));

        let search =
            Cli::try_parse_from(["ferrus", "graph", "search", "decision", "--domain", "all"])
                .unwrap();
        assert!(matches!(
            search.command,
            Some(Commands::Graph {
                command: GraphCommand::Search {
                    domain: GraphDomain::All,
                    ..
                }
            })
        ));
    }

    #[test]
    fn graph_neighbors_surface_parses_direction_and_budgets() {
        let cli = Cli::try_parse_from([
            "ferrus",
            "graph",
            "neighbors",
            "node:1",
            "--direction",
            "incoming",
            "--depth",
            "2",
            "--limit",
            "9",
        ])
        .unwrap();
        let Some(Commands::Graph {
            command:
                GraphCommand::Neighbors {
                    direction,
                    depth,
                    limit,
                    ..
                },
        }) = cli.command
        else {
            panic!("graph neighbors did not parse");
        };
        assert!(matches!(direction, Direction::Incoming));
        assert_eq!(depth, Some(2));
        assert_eq!(limit, Some(9));
    }

    #[test]
    fn graph_show_json_is_not_part_of_the_lookup_exclusion_group() {
        assert!(
            Cli::try_parse_from([
                "ferrus",
                "graph",
                "show",
                "--symbol",
                "rust:struct:src/lib.rs:Thing",
                "--json",
            ])
            .is_ok()
        );
    }

    #[test]
    fn graph_context_surface_parses_seed_and_hard_budget_requests() {
        assert!(
            Cli::try_parse_from([
                "ferrus",
                "graph",
                "context",
                "--symbol",
                "rust:struct:src/lib.rs:Thing",
                "--depth",
                "2",
                "--max-results",
                "12",
                "--max-bytes",
                "4096",
                "--json",
            ])
            .is_ok()
        );
    }
}
