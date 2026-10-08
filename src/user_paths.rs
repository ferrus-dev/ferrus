//! Machine-local Ferrus home, without project registration or database initialization.

pub(crate) fn ferrus_home() -> anyhow::Result<std::path::PathBuf> {
    use anyhow::Context;
    Ok(dirs::home_dir()
        .context("Cannot determine home directory")?
        .join(".ferrus"))
}
