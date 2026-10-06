//! Managed process entry point. Stdout is reserved for the versioned event stream.

use anyhow::Result;
use std::path::PathBuf;

#[derive(clap::Subcommand)]
pub(crate) enum Command {
    /// Summarize a pinned, opt-in headless evaluation manifest without inference
    Eval {
        /// JSON manifest with attempts and authoritative Ferrus database paths
        #[arg(long)]
        manifest: PathBuf,
    },
    /// Run a native Executor; HQ sends versioned commands over open stdin
    Run {
        /// Keep the conversation open for queued user steering
        #[arg(long)]
        interactive: bool,
        /// Open a direct workspace conversation without claiming a managed task
        #[arg(long, requires = "interactive")]
        taskless: bool,
        /// Absolute owner-only provider settings file (or FERRUS_NANO_CONFIG)
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override the configured provider model
        #[arg(long)]
        model: Option<String>,
        /// Disable working-set selection, query reuse, and scheduled overlay refresh
        #[arg(long)]
        no_working_set: bool,
        /// Disable native graph and memory tools; external MCP tools remain available
        #[arg(long)]
        no_native_context: bool,
        /// Prefetch this explicit workspace path (repeatable; at most eight seeds total)
        #[arg(long)]
        prefetch_path: Vec<String>,
        /// Prefetch this exact graph symbol key (repeatable; opt-in)
        #[arg(long)]
        prefetch_symbol: Vec<String>,
    },
    #[cfg(feature = "nano-mcp")]
    #[command(hide = true)]
    McpPeer {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        server: String,
    },
}

pub(crate) async fn run(command: Command) -> Result<()> {
    let (
        config,
        model,
        no_working_set,
        no_native_context,
        prefetch_path,
        prefetch_symbol,
        interactive,
        taskless,
    ) = match command {
        Command::Eval { manifest } => return super::eval::report(&manifest),
        Command::Run {
            interactive,
            taskless,
            config,
            model,
            no_working_set,
            no_native_context,
            prefetch_path,
            prefetch_symbol,
        } => (
            config,
            model,
            no_working_set,
            no_native_context,
            prefetch_path,
            prefetch_symbol,
            interactive,
            taskless,
        ),
        #[cfg(feature = "nano-mcp")]
        Command::McpPeer { config, server } => return super::mcp::run_peer(&config, &server),
    };
    let seeds = prefetch_seeds(prefetch_path, prefetch_symbol)?;
    anyhow::ensure!(
        !no_native_context || seeds.is_empty(),
        "Native prefetch requires native context tools"
    );
    super::agent::validate_config(config.as_deref(), model.as_deref())?;
    #[cfg(feature = "nano-openai")]
    // Keep the session state off the CLI/main-thread stack, especially on Windows.
    return Box::pin(launch(
        super::agent::config_path(config.as_deref())?,
        model,
        !no_working_set,
        !no_native_context,
        seeds,
        interactive,
        taskless,
    ))
    .await;
    #[cfg(not(feature = "nano-openai"))]
    {
        let _ = (
            no_working_set,
            no_native_context,
            seeds,
            interactive,
            taskless,
        );
        unreachable!("feature validated above");
    }
}

#[cfg(feature = "nano-openai")]
async fn launch(
    config: PathBuf,
    model: Option<String>,
    working_set: bool,
    native_context: bool,
    seeds: Vec<serde_json::Value>,
    interactive: bool,
    taskless: bool,
) -> Result<()> {
    use super::{
        coding::CodingTools,
        commands,
        ferrus::{FerrusSession, LaunchContext},
        instructions,
        journal::{FileJournal, Quotas},
        native::NativeTools,
        providers::openai::OpenAi,
        session::{EndReason, Limits, SessionIdentity},
        tools::Cancellation,
        wire::{self, CommandKind, Event, ObservedJournal, Output},
        workspace,
    };
    use std::{
        io::BufReader,
        sync::{Arc, Mutex},
        time::Duration,
    };
    let settings = super::agent::load_config(&config, model.as_deref())?;
    let limits = Limits {
        tokens: settings.session_tokens,
        ..Default::default()
    };
    let token_limit = limits.tokens;
    let working_set = working_set && settings.working_set_enabled;
    let native_context = native_context && settings.native_context_enabled;
    anyhow::ensure!(
        native_context || seeds.is_empty(),
        "Native prefetch requires native context tools"
    );
    #[cfg(feature = "nano-mcp")]
    let mcp_config = settings.mcp_config_file.clone();
    let provider = OpenAi::new(settings)?;
    let launch = if taskless {
        None
    } else {
        Some(LaunchContext::from_env()?)
    };
    let stop = Cancellation::default();
    let error = Arc::new(Mutex::new(None::<String>));
    let (start_tx, start_rx) = tokio::sync::oneshot::channel();
    let preview_enabled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(interactive));
    let input_preview = preview_enabled.clone();
    let (commands_tx, commands_rx) = tokio::sync::mpsc::channel(8);
    let (output, drained) = Output::spawn(wire::stdout_file()?);
    let input_output = output.clone();
    let input_stop = stop.clone();
    let input_error = error.clone();
    std::thread::spawn(move || {
        let mut input = BufReader::new(std::io::stdin());
        let first = wire::read_command(&mut input);
        match first {
            Ok(Some(CommandKind::Start)) => {}
            other => {
                let error = other
                    .err()
                    .unwrap_or_else(|| anyhow::anyhow!("Expected a versioned nano start command"));
                let _ = start_tx.send(Err(error));
                return;
            }
        }
        let _ = start_tx.send(Ok(()));
        loop {
            match wire::read_command(&mut input) {
                Ok(Some(CommandKind::Cancel)) | Ok(None) => break,
                Ok(Some(CommandKind::Interact)) => {
                    input_preview.store(true, std::sync::atomic::Ordering::Relaxed);
                    if commands_tx
                        .try_send(super::session::SessionCommand::Interact)
                        .is_err()
                    {
                        input_output.publish(Event::Error {
                            code: "input_queue_full".into(),
                        });
                    }
                }
                Ok(Some(CommandKind::Steer { text, input_id }))
                    if !text.trim().is_empty()
                        && input_id.as_deref().is_none_or(super::journal::valid_id) =>
                {
                    input_preview.store(true, std::sync::atomic::Ordering::Relaxed);
                    let rejection = input_id.as_ref().map_or_else(
                        || "input_queue_full".into(),
                        |id| format!("input_queue_full:{id}"),
                    );
                    if commands_tx
                        .try_send(super::session::SessionCommand::Steer { text, input_id })
                        .is_err()
                    {
                        input_output.publish(Event::Error { code: rejection });
                    }
                }
                _ => {
                    *input_error.lock().unwrap() = Some("invalid_command".into());
                    break;
                }
            }
        }
        input_stop.cancel();
    });
    output.publish(Event::Ready);
    let mut terminal_published = false;
    let result = async {
        tokio::time::timeout(Duration::from_secs(30), start_rx).await???;
        let binding = match launch {
            Some(launch) => super::binding::Binding::from(FerrusSession::bind(launch).await?),
            None => super::binding::Binding::interactive_from_env().await?,
        };
        let session_id = binding.run_id().to_owned();
        let journal = FileJournal::create(binding.data_dir(), &session_id, Quotas::default())?;
        let coding = CodingTools {
            workspace: workspace::Workspace::new(
                binding.workspace(),
                workspace::Limits::default(),
            )?,
            commands: commands::Commands::trusted_local(
                binding.workspace(),
                &session_id,
                journal.directory(),
                commands::Limits::default(),
            )?,
        };
        let mut native =
            NativeTools::new(binding.clone(), coding, instructions::Limits::default())?;
        #[cfg(feature = "nano-mcp")]
        {
            native.mcp_config = mcp_config;
        }
        native.working_set_enabled = working_set;
        native.native_context_enabled = native_context;
        native.context.cache_enabled = working_set;
        native.prefetch = seeds;
        let journal = ObservedJournal {
            journal,
            output: output.clone(),
            commands: Some(commands_rx),
            streaming: (0, String::new()),
            interactive,
            preview_enabled,
        };
        let end = if let Some(session) = binding.managed() {
            let identity = SessionIdentity {
                session_id,
                project_id: binding.project_id().into(),
                task_id: Some(session.scope.task_id.clone()),
                run_id: Some(session.scope.run_id.clone()),
            };
            Box::pin(super::managed::run(session.clone(), identity, limits, provider, native, journal, &stop)).await?
        } else {
            Box::pin(super::interactive::run(binding, limits, provider, native, journal, &stop)).await?
        };
        output.publish(Event::Ended {
            reason: end.reason.clone(),
            durable: end.durable,
        });
        terminal_published = true;
        anyhow::ensure!(
            end.durable,
            "Nano session could not durably record its outcome"
        );
        if end.reason == EndReason::Limit(super::session::LimitKind::Tokens) {
            anyhow::bail!(
                "Nano cannot reserve another request within the work-phase token budget ({} consumed, {token_limit} limit). Configure session_tokens in nano.toml for a new work phase; context_tokens is the per-request window",
                end.budget.tokens()
            );
        }
        anyhow::ensure!(
            !matches!(
                end.reason,
                EndReason::ProviderProtocol | EndReason::ProviderFailed
            ) && end.reason.managed_failure_code(end.retryable_provider_failure).is_none(),
            "Nano stopped after a provider failure or exhausted work-phase budget; inspect diagnostics"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if result.is_err() && !terminal_published {
        output.publish(Event::Error {
            code: "session_failed".into(),
        });
    }
    let protocol_error = error.lock().unwrap().take();
    if let Some(code) = &protocol_error {
        output.publish(Event::Error { code: code.clone() });
    }
    output.close();
    // Stdout is advisory: do not wait indefinitely for a stalled frontend after durable cleanup.
    let _ = tokio::time::timeout(Duration::from_secs(2), drained).await;
    result?;
    anyhow::ensure!(protocol_error.is_none(), "Invalid nano command stream");
    Ok(())
}

fn prefetch_seeds(paths: Vec<String>, symbols: Vec<String>) -> Result<Vec<serde_json::Value>> {
    use serde_json::json;
    anyhow::ensure!(
        paths.len() + symbols.len() <= 8,
        "At most eight prefetch seeds are allowed"
    );
    let seeds: std::collections::BTreeSet<_> = paths
        .into_iter()
        .map(|p| ("path", p))
        .chain(symbols.into_iter().map(|s| ("symbol", s)))
        .collect();
    let seeds: Vec<_> = seeds
        .into_iter()
        .map(|(kind, value)| json!({"type":kind, "value":value}))
        .collect();
    if !seeds.is_empty() {
        super::context::Request::parse("repository_context", json!({"seeds":seeds}))?;
    }
    Ok(seeds)
}

#[cfg(test)]
mod working_set_tests {
    use super::*;
    #[cfg(feature = "nano-openai")]
    #[test]
    fn launch_future_does_not_embed_the_session_engine() {
        // Windows processes have a small main-thread stack. Keep the public
        // launch state bounded so nested polling and moves leave room for tools.
        for taskless in [false, true] {
            let future = run(Command::Run {
                config: None,
                model: None,
                no_working_set: false,
                no_native_context: false,
                prefetch_path: Vec::new(),
                prefetch_symbol: Vec::new(),
                interactive: true,
                taskless,
            });
            let bytes = std::mem::size_of_val(&future);
            assert!(bytes <= 8 * 1024, "Nano launch future uses {bytes} bytes");
            let host = launch(PathBuf::new(), None, true, true, Vec::new(), true, taskless);
            let bytes = std::mem::size_of_val(&host);
            assert!(bytes <= 8 * 1024, "Nano host future uses {bytes} bytes");
        }
    }

    #[test]
    fn explicit_prefetch_is_opt_in_validated_bounded_and_sorted() {
        assert!(prefetch_seeds(vec![], vec![]).unwrap().is_empty());
        assert!(prefetch_seeds(vec!["../escape".into()], vec![]).is_err());
        assert!(prefetch_seeds(vec![], vec!["".into()]).is_err());
        assert!(prefetch_seeds(vec!["a.rs".into(); 9], vec![]).is_err());
        let seeds = prefetch_seeds(
            vec!["b.rs".into(), "a.rs".into(), "a.rs".into()],
            vec!["symbol:one".into()],
        )
        .unwrap();
        assert_eq!(seeds.len(), 3);
        assert_eq!(seeds[0]["value"], "a.rs");
        use clap::Parser;
        assert!(
            crate::cli::Cli::try_parse_from([
                "ferrus",
                "nano",
                "run",
                "--no-working-set",
                "--prefetch-path",
                "a.rs",
                "--prefetch-symbol",
                "symbol:one"
            ])
            .is_ok()
        );
    }
}
