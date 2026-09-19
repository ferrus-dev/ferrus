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
    },
}

pub(crate) async fn run(command: Command) -> Result<()> {
    let Command::Run { config, model } = command;
    super::agent::validate_config(config.as_deref(), model.as_deref())?;
    #[cfg(feature = "nano-openai")]
    return launch(super::agent::config_path(config.as_deref())?, model).await;
    #[cfg(not(feature = "nano-openai"))]
    unreachable!("feature validated above");
}

#[cfg(feature = "nano-openai")]
async fn launch(config: PathBuf, model: Option<String>) -> Result<()> {
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
        let native = NativeTools::new(session.clone(), coding, instructions::Limits::default())?;
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
