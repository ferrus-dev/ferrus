#[path = "../tests/support/repository_graph_eval.rs"]
mod repository_graph_eval;

use anyhow::{Context, Result, bail};
use ferrus::repository_graph::extractors::cargo::run_parser_worker_if_requested;

fn main() -> Result<()> {
    if run_parser_worker_if_requested().context("Cargo parser worker protocol failed")? {
        return Ok(());
    }

    let mut output = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--output" => {
                output = Some(
                    arguments
                        .next()
                        .context("--output requires a JSON report path")?,
                );
            }
            unknown => bail!("unknown repository graph evaluation argument: {unknown}"),
        }
    }

    let report = repository_graph_eval::run_evaluation()?;
    let passed = report.gates.all_passed;
    let json = serde_json::to_string_pretty(&report)?;
    if let Some(path) = output {
        std::fs::write(path, format!("{json}\n"))?;
    } else {
        println!("{json}");
    }
    if !passed {
        bail!("one or more repository graph usefulness gates failed");
    }
    Ok(())
}
