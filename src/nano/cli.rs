//! Managed process entry point. Stdout is reserved for the versioned event stream.

use anyhow::Result;
use std::path::PathBuf;

#[derive(clap::Subcommand)]
pub(crate) enum Command {
    /// Run a managed Executor; HQ sends start/cancel commands over open stdin
    Run {
        /// Absolute owner-only provider settings file (or FERRUS_NANO_CONFIG)
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override the configured provider model
        #[arg(long)]
        model: Option<String>,
        /// Disable working-set selection, query reuse, and scheduled overlay refresh
        #[arg(long)]
        no_working_set: bool,
        /// Prefetch this explicit task path (repeatable; at most eight seeds total)
        #[arg(long)]
        prefetch_path: Vec<String>,
        /// Prefetch this exact graph symbol key (repeatable; opt-in)
        #[arg(long)]
        prefetch_symbol: Vec<String>,
    },
}

pub(crate) async fn run(command: Command) -> Result<()> {
    let Command::Run {
        config,
        model,
        no_working_set,
        prefetch_path,
        prefetch_symbol,
    } = command;
    let seeds = prefetch_seeds(prefetch_path, prefetch_symbol)?;
    super::agent::validate_config(config.as_deref(), model.as_deref())?;
    #[cfg(feature = "nano-openai")]
    return launch(
        super::agent::config_path(config.as_deref())?,
        model,
        !no_working_set,
        seeds,
    )
    .await;
    #[cfg(not(feature = "nano-openai"))]
    {
        let _ = (no_working_set, seeds);
        unreachable!("feature validated above");
    }
}

#[cfg(feature = "nano-openai")]
async fn launch(
    config: PathBuf,
    model: Option<String>,
    working_set: bool,
    seeds: Vec<serde_json::Value>,
) -> Result<()> {
    use super::{
        coding::CodingTools,
        commands,
        ferrus::{FerrusSession, LaunchContext},
        instructions,
        journal::{FileJournal, Quotas},
        native::NativeTools,
        providers::openai::OpenAi,
        session::{Limits, SessionIdentity},
        tools::Cancellation,
        wire::{self, CommandKind, Event, ObservedJournal, Output},
        workspace,
    };
    use std::{
        io::BufReader,
        sync::{Arc, Mutex},
        time::Duration,
    };
    let provider = OpenAi::new(super::agent::load_config(&config, model.as_deref())?)?;
    let launch = LaunchContext::from_env()?;
    let stop = Cancellation::default();
    let error = Arc::new(Mutex::new(None::<String>));
    let (start_tx, start_rx) = tokio::sync::oneshot::channel();
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
        if !matches!(
            wire::read_command(&mut input),
            Ok(Some(CommandKind::Cancel)) | Ok(None)
        ) {
            *input_error.lock().unwrap() = Some("invalid_command".into());
        }
        input_stop.cancel();
    });
    let (output, drained) = Output::spawn(wire::stdout_file()?);
    output.publish(Event::Ready);
    let result = async {
        tokio::time::timeout(Duration::from_secs(30), start_rx).await???;
        let session_id = launch.run_id.clone();
        let session = FerrusSession::bind(launch).await?;
        let identity = SessionIdentity {
            session_id: session_id.clone(),
            project_id: session.project_id().into(),
            task_id: Some(session.scope.task_id.clone()),
            run_id: Some(session.scope.run_id.clone()),
        };
        let journal = FileJournal::create(session.data_dir(), &session_id, Quotas::default())?;
        let coding = CodingTools {
            workspace: workspace::Workspace::new(
                session.workspace(),
                workspace::Limits::default(),
            )?,
            commands: commands::Commands::trusted_local(
                session.workspace(),
                &session_id,
                journal.directory(),
                commands::Limits::default(),
            )?,
        };
        let mut native =
            NativeTools::new(session.clone(), coding, instructions::Limits::default())?;
        native.working_set_enabled = working_set;
        native.context.cache_enabled = working_set;
        native.prefetch = seeds;
        let journal = ObservedJournal {
            journal,
            output: output.clone(),
        };
        let end = super::managed::run(
            session,
            identity,
            Limits::default(),
            provider,
            native,
            journal,
            &stop,
        )
        .await?;
        output.publish(Event::Ended {
            reason: end.reason,
            durable: end.durable,
        });
        anyhow::ensure!(
            end.durable,
            "Nano session could not durably record its outcome"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if result.is_err() {
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
