//! Native headless Executor adapter. Provider settings remain host-local.

use crate::agents::{
    AgentDisplayConfig, AgentRunMode, ExecutorAgent, ExecutorCapabilities, HeadlessPromptTransport,
    McpConfigEntry, normalized_model,
};
#[cfg(feature = "nano-openai")]
use anyhow::Context;
use anyhow::{Result, bail, ensure};
#[cfg(feature = "nano-openai")]
use std::{
    fs,
    io::{self, Write},
};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

pub(crate) const NAME: &str = "nano";
pub(crate) const CONFIG_ENV: &str = "FERRUS_NANO_CONFIG";
#[cfg(feature = "nano-openai")]
const DEFAULT_BASE_URL: &str = "http://127.0.0.1:1234/v1";

fn default_config_path() -> Result<PathBuf> {
    Ok(crate::project::global_dir()?.join("nano.toml"))
}

pub(crate) fn config_path(path: Option<&Path>) -> Result<PathBuf> {
    let path = path
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os(CONFIG_ENV).map(PathBuf::from))
        .map(Ok)
        .unwrap_or_else(default_config_path)?;
    ensure!(path.is_absolute(), "Nano config path must be absolute");
    Ok(path)
}

/// Provision the LM Studio default only during explicit registration.
pub(crate) fn prepare_registration_config(model: Option<&str>) -> Result<()> {
    #[cfg(feature = "nano-openai")]
    {
        let path = config_path(None)?;
        if std::env::var_os(CONFIG_ENV).is_none()
            && matches!(fs::symlink_metadata(&path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        {
            create_default_config(&path, model)?;
            eprintln!(
                "Created private Nano provider settings at {}",
                path.display()
            );
        }
        validate_config(Some(&path), model)
    }
    #[cfg(not(feature = "nano-openai"))]
    {
        let _ = model;
        bail!("Nano requires Ferrus built with --features nano-openai")
    }
}

#[cfg(feature = "nano-openai")]
fn create_default_config(path: &Path, model: Option<&str>) -> Result<()> {
    #[derive(serde::Serialize)]
    struct InitialConfig<'a> {
        base_url: &'static str,
        model: &'a str,
    }

    let model = normalized_model(model).context(
        "Nano needs --executor-model <loaded-model-id> to create default provider settings",
    )?;
    let contents = toml::to_string(&InitialConfig {
        base_url: DEFAULT_BASE_URL,
        model: &model,
    })?;
    let config: super::config::Config = toml::from_str(&contents)?;
    config.validate()?;

    let directory = path
        .parent()
        .context("Nano settings path has no parent directory")?;
    // The shared Ferrus root may predate Nano; the provider file has its own owner-only ACL.
    match fs::symlink_metadata(directory) {
        Ok(metadata) => ensure!(
            metadata.is_dir(),
            "Nano config directory is not a directory"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            super::private::directory(directory, true)?;
        }
        Err(error) => return Err(error.into()),
    }
    write_new_config(path, contents.as_bytes(), |file, bytes| {
        file.write_all(bytes)
    })
}

#[cfg(feature = "nano-openai")]
fn write_new_config(
    path: &Path,
    contents: &[u8],
    write: impl FnOnce(&mut fs::File, &[u8]) -> io::Result<()>,
) -> Result<()> {
    let directory = path
        .parent()
        .context("Nano settings path has no parent directory")?;
    let mut file = super::private::file(path, true)
        .with_context(|| format!("Cannot create Nano settings at {}", path.display()))?;
    let result = write(&mut file, contents).and_then(|()| file.sync_all());
    drop(file);
    let result = result.and_then(|()| super::private::sync_directory(directory));
    if let Err(error) = result {
        fs::remove_file(path).with_context(|| {
            format!(
                "Cannot remove incomplete Nano settings at {} after {error}",
                path.display()
            )
        })?;
        return Err(error).context("Cannot write Nano settings");
    }
    Ok(())
}

#[cfg(feature = "nano-openai")]
pub(crate) fn load_config(path: &Path, model: Option<&str>) -> Result<super::config::Config> {
    let mut config = super::config::Config::load(path)?;
    if let Some(model) = normalized_model(model) {
        config.model = model;
    } else {
        config.model = config.model.trim().to_owned();
    }
    config.validate()?;
    config.authorization()?;
    #[cfg(feature = "nano-mcp")]
    if let Some(path) = &config.mcp_config_file {
        super::mcp::validate_config(path)?;
    }
    Ok(config)
}

pub(crate) fn validate_config(path: Option<&Path>, model: Option<&str>) -> Result<()> {
    #[cfg(feature = "nano-openai")]
    {
        load_config(&config_path(path)?, model)?;
        Ok(())
    }
    #[cfg(not(feature = "nano-openai"))]
    {
        let _ = (path, model);
        bail!("Nano requires Ferrus built with --features nano-openai")
    }
}

pub(crate) struct Executor {
    model: Option<String>,
}
impl Executor {
    pub(crate) fn new(model: Option<&str>) -> Self {
        Self {
            model: normalized_model(model),
        }
    }
}
impl ExecutorAgent for Executor {
    fn name(&self) -> &'static str {
        NAME
    }
    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities {
            interactive: false,
            headless: true,
            native: true,
            event_output: true,
        }
    }
    fn spawn_with_index(&self, mode: AgentRunMode<'_>, _: u32) -> Result<Command> {
        ensure!(
            matches!(mode, AgentRunMode::Headless { .. }),
            "Nano supports headless Executor sessions only"
        );
        // The native host selects work or stored-response delivery from the bound
        // SQLite task; external-agent relaunch prompts are not model input here.
        validate_config(None, self.model())?;
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(["nano", "run", "--config"])
            .arg(config_path(None)?);
        if let Some(model) = self.model() {
            command.arg("--model").arg(model);
        }
        Ok(command)
    }
    fn version_command(&self) -> Result<Command> {
        let mut command = Command::new(std::env::current_exe()?);
        command.args(["nano", "--version"]);
        Ok(command)
    }
    fn mcp_config_entry(&self, _: &str, _: u32) -> Result<McpConfigEntry> {
        bail!("Nano uses native Ferrus operations and has no loopback MCP entry")
    }
    fn validate_interactive_launch(&self, _: &str, _: u32) -> Result<()> {
        bail!("Nano supports headless Executor sessions only")
    }
    fn validate_headless_launch(&self, role: &str, _: u32) -> Result<()> {
        ensure!(role == "executor", "Nano supports the Executor role only");
        validate_config(None, self.model())
    }
    fn headless_prompt_transport(&self) -> HeadlessPromptTransport {
        HeadlessPromptTransport::Jsonl
    }
    fn display_config(&self) -> AgentDisplayConfig {
        let display = AgentDisplayConfig::from_model(self.model());
        #[cfg(feature = "nano-openai")]
        if display.model.is_none()
            && let Ok(path) = config_path(None)
            && let Ok(config) = super::config::Config::load(&path)
        {
            return AgentDisplayConfig::from_model(Some(&config.model));
        }
        display
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "nano-openai")]
    #[test]
    fn default_config_is_private_valid_and_requires_a_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".ferrus").join("nano.toml");
        assert!(
            create_default_config(&path, None)
                .unwrap_err()
                .to_string()
                .contains("--executor-model")
        );
        assert!(!path.exists());

        crate::nano::private::directory(path.parent().unwrap(), true).unwrap();
        let failure = write_new_config(&path, b"complete settings", |file, _| {
            file.write_all(b"partial")?;
            Err(io::Error::other("injected write failure"))
        })
        .unwrap_err();
        assert!(failure.to_string().contains("Cannot write Nano settings"));
        assert!(!path.exists());

        create_default_config(&path, Some(" local/model ")).unwrap();
        let config = load_config(&path, None).unwrap();
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.model, "local/model");
        assert!(crate::nano::private::read_only_file(&path).is_ok());
        assert!(create_default_config(&path, Some("other-model")).is_err());
        assert_eq!(load_config(&path, None).unwrap().model, "local/model");
    }
    #[cfg(feature = "nano-openai")]
    #[test]
    fn provider_config_and_overrides_normalize_the_selected_model() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("provider.toml");
        let mut file = crate::nano::private::file(&path, true).unwrap();
        file.write_all(b"base_url = 'http://127.0.0.1:1234/v1'\nmodel = ' local/model '\n")
            .unwrap();
        drop(file);
        assert_eq!(load_config(&path, None).unwrap().model, "local/model");
        assert_eq!(load_config(&path, Some("  ")).unwrap().model, "local/model");
        assert_eq!(
            load_config(&path, Some(" override ")).unwrap().model,
            "override"
        );
    }
    #[test]
    fn native_capabilities_version_and_model_do_not_require_provider_setup() {
        let agent = Executor::new(Some("  local/model  "));
        assert_eq!(agent.model(), Some("local/model"));
        assert_eq!(agent.display_config().model.as_deref(), Some("local/model"));
        assert_eq!(Executor::new(Some("  ")).model(), None);
        assert_eq!(
            agent.capabilities(),
            ExecutorCapabilities {
                interactive: false,
                headless: true,
                native: true,
                event_output: true
            }
        );
        assert_eq!(
            agent.headless_prompt_transport(),
            HeadlessPromptTransport::Jsonl
        );
        assert!(
            agent
                .spawn(AgentRunMode::Interactive { prompt: None })
                .is_err()
        );
        assert!(crate::agents::parse_supervisor_agent("nano", None).is_err());
        assert!(agent.mcp_config_entry("executor", 1).is_err());
        let version = agent.version_command().unwrap();
        assert_eq!(
            version.get_args().collect::<Vec<_>>(),
            ["nano", "--version"]
        );
        assert!(config_path(Some(Path::new("relative.toml"))).is_err());
    }
    #[cfg(not(feature = "nano-openai"))]
    #[test]
    fn missing_provider_feature_fails_before_setup() {
        assert!(
            validate_config(None, None)
                .unwrap_err()
                .to_string()
                .contains("nano-openai")
        );
    }
}
