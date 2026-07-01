mod agent_id;
mod agents;
mod checks;
mod cli;
mod config;
mod hq;
mod legacy_state;
mod platform;
mod project;
mod runtime_status;
mod runtime_table;
mod server;
mod specs;
mod state;
mod templates;
mod update_check;

#[cfg(test)]
mod test_support {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    pub(crate) struct RecoveringMutex(Mutex<()>);

    impl RecoveringMutex {
        pub(crate) fn lock(&self) -> Result<MutexGuard<'_, ()>, std::convert::Infallible> {
            Ok(self
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()))
        }
    }

    pub(crate) fn cwd_lock() -> &'static RecoveringMutex {
        static LOCK: OnceLock<RecoveringMutex> = OnceLock::new();
        LOCK.get_or_init(|| RecoveringMutex(Mutex::new(())))
    }

    pub(crate) fn assert_no_state_json() {
        assert!(
            !std::path::Path::new(".ferrus/STATE.json").exists(),
            ".ferrus/STATE.json should not be created by SQLite runtime paths"
        );
    }
}

use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = cli::Cli::parse();
    let debug = args.debug_enabled();

    if args.is_hq_mode() {
        init_hq_logger(debug);
    } else {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("ferrus=info"));
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    }

    args.run(debug).await
}

fn init_hq_logger(debug: bool) {
    // Log to file in HQ mode so the terminal isn't cluttered.
    // Best-effort: if .ferrus/ doesn't exist yet, logging goes nowhere.
    let default_filter = if debug { "ferrus=debug" } else { "off" };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter));
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(".ferrus/hq.log")
    {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::sync::Mutex::new(file))
            .try_init();
    }
}
