//! Configured command checks used by HQ and the Executor review gate.

pub mod runner;

pub(crate) fn commands_sha256(commands: &[String]) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};

    let bytes = serde_json::to_vec(&(1_u8, commands))?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
