//! Native headless Executor adapter. Provider settings remain host-local.

use crate::agents::{
    AgentDisplayConfig, AgentRunMode, ExecutorAgent, ExecutorCapabilities, HeadlessPromptTransport,
    McpConfigEntry, normalized_model,
};
use anyhow::{Result, bail, ensure};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

pub(crate) const NAME: &str = "nano";
pub(crate) const CONFIG_ENV: &str = "FERRUS_NANO_CONFIG";

pub(crate) fn config_path(path: Option<&Path>) -> Result<PathBuf> {
    let path = path
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os(CONFIG_ENV).map(PathBuf::from))
        .ok_or_else(|| {
            anyhow::anyhow!("Set FERRUS_NANO_CONFIG to an absolute private provider settings file")
        })?;
    ensure!(path.is_absolute(), "Nano config path must be absolute");
    Ok(path)
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
