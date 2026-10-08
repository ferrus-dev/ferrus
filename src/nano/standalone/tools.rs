use super::{
    super::{
        coding::CodingTools,
        commands::{self, Commands, TrustedLocal},
        context_request::{self, Request},
        descriptors::descriptor,
        instructions::{self, Instructions},
        tools::*,
        workspace::Workspace,
    },
    Scope,
    context::LocalContext,
};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

pub(super) struct StandaloneTools {
    coding: CodingTools<TrustedLocal>,
    pub instructions: Instructions<Scope>,
    context: LocalContext,
    working_set_enabled: bool,
    #[cfg(feature = "nano-mcp")]
    pub mcp: Option<super::super::mcp::McpTools>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    skills: Vec<String>,
}

impl StandaloneTools {
    pub fn new(
        root: &Path,
        directory: &Path,
        session: &str,
        context: LocalContext,
        working_set_enabled: bool,
    ) -> Result<Self> {
        Ok(Self {
            coding: CodingTools {
                workspace: Workspace::new(root, Default::default())?,
                commands: Commands::trusted_local(
                    root,
                    session,
                    directory,
                    commands::Limits::default(),
                )?,
            },
            instructions: Instructions::new(Scope(root.into()), instructions::Limits::default())?,
            context,
            working_set_enabled,
            #[cfg(feature = "nano-mcp")]
            mcp: None,
        })
    }
}

impl Tools for StandaloneTools {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        let mut tools = self.coding.descriptors();
        tools.push(descriptor("load_instructions"));
        tools.push(descriptor("repository_fallback"));
        tools.extend(
            context_request::NAMES
                .iter()
                .filter(|name| {
                    (name.starts_with("repository_")
                        && self.context.graph_enabled
                        && **name != "repository_fallback")
                        || (name.starts_with("project_") && self.context.has_memory())
                })
                .map(|name| descriptor(name)),
        );
        #[cfg(feature = "nano-mcp")]
        if let Some(mcp) = &self.mcp {
            tools.extend(mcp.descriptors());
        }
        tools
    }
    fn validate(&self, name: &str, arguments: &Value) -> Result<(), ToolError> {
        #[cfg(feature = "nano-mcp")]
        if let Some(mcp) = &self.mcp
            && mcp.contains(name)
        {
            return mcp.validate(name, arguments);
        }
        if name == "load_instructions" {
            serde_json::from_value::<Selection>(arguments.clone())
                .map(|_| ())
                .map_err(|_| ToolError::InvalidArguments)
        } else if name == "repository_fallback" {
            ensure_fallback(arguments)
        } else if context_request::NAMES.contains(&name)
            && self.descriptors().iter().any(|tool| tool.name == name)
        {
            Request::parse(name, arguments.clone())
                .map(|_| ())
                .map_err(|_| ToolError::InvalidArguments)
        } else {
            self.coding.validate(name, arguments)
        }
    }
    fn effect_plan(&self, call: &ValidatedCall) -> Option<EffectPlan> {
        self.coding.effect_plan(call)
    }
    async fn prepare_context(
        &mut self,
        messages: &[super::super::provider::Message],
        _: &Cancellation,
    ) -> Result<Option<super::super::working_set::Preparation>, ToolError> {
        if !self.working_set_enabled {
            return Ok(None);
        }
        // No publication is claimed fresh without a current manifest comparison.
        // Reuse only source-verified workspace evidence between explicit queries.
        super::super::working_set::prepare(
            messages,
            &json!({"workspace":"standalone"}),
            &json!({"snapshot_id":null,"memory_revision_id":null,"task_view":null}),
            &self.coding.workspace,
            self.coding.commands.potentially_active_writers() > 0,
        )
        .map(Some)
        .map_err(|_| ToolError::Failed)
    }
    async fn execute(&mut self, call: &ValidatedCall, cancellation: &Cancellation) -> ToolOutcome {
        if matches!(call.name.as_str(), "exec" | "apply_patch") || call.name.starts_with("mcp_") {
            self.context.invalidate();
        }
        if context_request::NAMES.contains(&call.name.as_str())
            && call.name != "repository_fallback"
            && self.coding.commands.potentially_active_writers() > 0
        {
            return ToolOutcome::Failed(ToolError::Context(
                json!({"code":"workspace_busy","message":"Wait for command completion before querying graph or memory evidence"}),
            ));
        }

        #[cfg(feature = "nano-mcp")]
        if let Some(mcp) = &mut self.mcp
            && mcp.contains(&call.name)
        {
            return mcp.execute(call, cancellation).await;
        }
        let result: Result<Value> = if call.name == "load_instructions" {
            async {
                let selection: Selection = serde_json::from_value(call.arguments.clone())?;
                Ok(serde_json::to_value(
                    self.instructions
                        .load(&selection.paths, &selection.skills)
                        .await?,
                )?)
            }
            .await
        } else if call.name == "repository_fallback" {
            let name = if call.arguments["operation"] == "read" {
                "read_file"
            } else {
                "search_text"
            };
            let forwarded = ValidatedCall {
                name: name.into(),
                arguments: call.arguments["input"].clone(),
                call_id: call.call_id.clone(),
                provider_call_id: call.provider_call_id.clone(),
            };
            return match self.coding.execute(&forwarded, cancellation).await {
                ToolOutcome::Success(evidence) => ToolOutcome::Success(
                    json!({"kind":"workspace_fallback","requested_reason":call.arguments["reason"],"evidence":evidence}),
                ),
                outcome => outcome,
            };
        } else if context_request::NAMES.contains(&call.name.as_str()) {
            match Request::parse(&call.name, call.arguments.clone()) {
                Ok(request) => self.context.retrieve(&call.name, request).await,
                Err(error) => Err(error),
            }
        } else {
            return self.coding.execute(call, cancellation).await;
        };
        match result {
            Ok(value) if super::super::journal::encode(&value, 32 * 1024).is_ok() => {
                ToolOutcome::Success(value)
            }
            Ok(_) => ToolOutcome::Failed(ToolError::OutputLimit),
            Err(_) => ToolOutcome::Failed(ToolError::Context(
                json!({"code":"context_unavailable","message":"Use current workspace read/search fallback or explicitly refresh the standalone index","canonical_fallback":false}),
            )),
        }
    }
    async fn interrupted(&mut self) -> Option<ToolOutcome> {
        #[cfg(feature = "nano-mcp")]
        if let Some(mcp) = &mut self.mcp {
            return mcp.interrupted().await;
        }
        None
    }
    async fn shutdown(&mut self) -> bool {
        let clean = self.coding.shutdown().await;
        #[cfg(feature = "nano-mcp")]
        if let Some(mcp) = &mut self.mcp {
            return mcp.shutdown().await && clean;
        }
        clean
    }
}

fn ensure_fallback(value: &Value) -> Result<(), ToolError> {
    let object = value.as_object().ok_or(ToolError::InvalidArguments)?;
    if object.len() != 3
        || !["missing", "disabled", "stale", "ambiguous", "unsupported"]
            .contains(&value["reason"].as_str().unwrap_or_default())
    {
        return Err(ToolError::InvalidArguments);
    }
    match value["operation"].as_str() {
        Some("read") => {
            serde_json::from_value::<super::super::workspace::ReadRequest>(value["input"].clone())
                .map(|_| ())
                .map_err(|_| ToolError::InvalidArguments)
        }
        Some("search") => {
            serde_json::from_value::<super::super::workspace::SearchRequest>(value["input"].clone())
                .map(|_| ())
                .map_err(|_| ToolError::InvalidArguments)
        }
        _ => Err(ToolError::InvalidArguments),
    }
}
