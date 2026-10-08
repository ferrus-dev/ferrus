//! Native tool composition. Only the bounded coding/context surface is advertised.

use super::{
    binding::Binding,
    coding::CodingTools,
    commands::ExecutionBackend,
    context::{self, Context, Fallback, Request},
    ferrus::FerrusSession,
    instructions::{Instructions, Limits},
    tools::*,
};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) struct NativeTools<B: ExecutionBackend> {
    pub coding: CodingTools<B>,
    pub context: Context,
    pub instructions: Instructions<Binding>,
    session: Binding,
    pub(super) working_set_enabled: bool,
    pub(super) native_context_enabled: bool,
    pub(super) prefetch: Vec<Value>,
    prefetch_invalidated: bool,
    refresh: super::refresh::Refresh,
    observations: Vec<Value>,
    #[cfg(feature = "nano-mcp")]
    pub(crate) mcp: Option<super::mcp::McpTools>,
    #[cfg(feature = "nano-mcp")]
    pub(crate) mcp_config: Option<std::path::PathBuf>,
}

impl<B: ExecutionBackend> NativeTools<B> {
    pub(super) fn belongs_to(&self, session: &FerrusSession) -> bool {
        self.session.managed().is_some_and(|bound| {
            bound.scope.database_path == session.scope.database_path
                && bound.scope.agent_id == session.scope.agent_id
                && bound.scope.task_id == session.scope.task_id
                && bound.scope.run_id == session.scope.run_id
                && bound.workspace() == session.workspace()
        })
    }
    pub(crate) fn new(
        session: impl Into<Binding>,
        coding: CodingTools<B>,
        limits: Limits,
    ) -> Result<Self> {
        let session = session.into();
        Ok(Self {
            coding,
            context: Context::new(session.clone()),
            instructions: Instructions::new(session.clone(), limits)?,
            session,
            working_set_enabled: true,
            native_context_enabled: true,
            prefetch: Vec::new(),
            prefetch_invalidated: false,
            refresh: Default::default(),
            observations: Vec::new(),
            #[cfg(feature = "nano-mcp")]
            mcp: None,
            #[cfg(feature = "nano-mcp")]
            mcp_config: None,
        })
    }

    pub(super) async fn invalidate_unknown(&mut self) {
        self.before_mutation().await;
        if self.session.managed().is_none()
            && let Err(error) = crate::project::record_canonical_graph_invalidation_at(
                &self.session.data_dir().join("ferrus.db"),
                "current",
                Some(self.session.run_id()),
                None,
                crate::project::CanonicalInvalidationReason::SourceComparisonUnavailable,
            )
            .await
        {
            tracing::warn!(error = ?error, "failed to invalidate the canonical graph before a Nano mutation");
        }
        if !self.working_set_enabled {
            self.prefetch_invalidated = true;
            return;
        }
        self.context.invalidate();
        self.refresh.invalidate();
        self.prefetch_invalidated = true;
    }

    async fn before_mutation(&mut self) {
        if let Some(observation) = self.refresh.settle().await {
            self.record_refresh(observation);
        }
    }

    fn record_refresh(&mut self, observation: Value) {
        if observation["status"] == "published" {
            self.prefetch_invalidated = false;
        }
        self.observations.push(observation);
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    skills: Vec<String>,
}

pub(super) use super::descriptors::descriptor;

impl<B: ExecutionBackend> Tools for NativeTools<B> {
    fn effect_plan(&self, call: &ValidatedCall) -> Option<EffectPlan> {
        self.coding.effect_plan(call)
    }

    async fn prepare_context(
        &mut self,
        messages: &[super::provider::Message],
        cancellation: &Cancellation,
    ) -> std::result::Result<Option<super::working_set::Preparation>, ToolError> {
        if !self.working_set_enabled && self.prefetch.is_empty() {
            return Ok(None);
        }
        if cancellation.is_cancelled() {
            return Err(ToolError::Interrupted);
        }
        let writers = self.coding.commands.potentially_active_writers() > 0;
        if self.working_set_enabled {
            // Resolve the task view before selecting prior graph packets or
            // prefetching new ones for the next model request. A writer may
            // have deferred scheduling and exited since the last tool call.
            for observation in self.refresh.prepare(&self.session, writers).await {
                self.record_refresh(observation);
            }
            if let Some(observation) = self.refresh.settle().await {
                self.record_refresh(observation);
            }
        }
        let revisions = self
            .context
            .revisions()
            .await
            .map_err(|_| ToolError::Denied)?;
        let binding = json!({"project":self.session.project_id(), "task":self.session.task_id(),
            "run":self.session.run_id(), "workspace":self.session.workspace()});
        let mut prepared = if self.working_set_enabled {
            super::working_set::prepare(
                messages,
                &binding,
                &revisions,
                &self.coding.workspace,
                writers,
            )
            .map_err(|_| ToolError::OutputLimit)?
        } else {
            super::working_set::Preparation::default()
        };
        let prefetch_ready = !writers
            && !self.prefetch_invalidated
            && (!self.working_set_enabled || self.refresh.pending_reason().is_none());
        if self.native_context_enabled && !self.prefetch.is_empty() && prefetch_ready {
            // Explicit host seeds only; no task-text heuristic or hidden planner.
            let request = json!({"seeds":self.prefetch, "max_results":8, "max_bytes":8192,
                "max_depth":1, "max_duration_ms":250, "max_snippet_bytes":4096, "include_snippets":true});
            let result = async {
                anyhow::ensure!(self.prefetch.len() <= 8, "Too many prefetch seeds");
                let response = self
                    .context
                    .retrieve(
                        "repository_context",
                        Request::parse("repository_context", request.clone())?,
                    )
                    .await?;
                serde_json::to_value(response).map_err(Into::into)
            }
            .await;
            match result {
                Ok(value) => {
                    let evidence = super::working_set::evidence("repository_context", &value, &binding);
                    if evidence.as_ref().is_some_and(|handle| {
                        let returned = value.pointer("/result/Ok/data/items")
                            .and_then(Value::as_array).is_some_and(|items| !items.is_empty());
                        (!returned || !handle.sources.is_empty())
                            && handle.sources_match(&self.coding.workspace)
                    }) {
                        prepared.observations.push(json!({"kind":"prefetch", "request":request, "response":value, "evidence":evidence}));
                    } else {
                        prepared.observations.push(json!({"kind":"prefetch_unavailable", "reason":"source_changed_or_unverified"}));
                    }
                }
                Err(error) => prepared.observations.push(json!({"kind":"prefetch_unavailable", "message":error.to_string().chars().take(256).collect::<String>()})),
            }
        } else if !self.prefetch.is_empty() {
            prepared
                .observations
                .push(json!({"kind":"prefetch_unavailable", "reason":"overlay_not_current"}));
        }
        prepared.observations.append(&mut self.observations);
        if self.working_set_enabled {
            prepared
                .observations
                .extend(self.refresh.prepare(&self.session, writers).await);
        }
        Ok(Some(prepared))
    }
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        let mut tools = self.coding.descriptors();
        if self.native_context_enabled {
            tools.extend(context::NAMES.iter().map(|name| descriptor(name)));
        }
        tools.push(descriptor("load_instructions"));
        #[cfg(feature = "nano-mcp")]
        if let Some(mcp) = &self.mcp {
            tools.extend(mcp.descriptors());
        }
        tools
    }

    fn validate(&self, name: &str, arguments: &Value) -> std::result::Result<(), ToolError> {
        #[cfg(feature = "nano-mcp")]
        if let Some(mcp) = &self.mcp
            && mcp.contains(name)
        {
            mcp.validate(name, arguments)?;
            if let Some(remote_name) = mcp.graph_remote_name(name)
                && Request::parse(remote_name, arguments.clone()).is_err()
            {
                return Err(ToolError::InvalidArguments);
            }
            return Ok(());
        }
        let valid = if context::NAMES.contains(&name) && !self.native_context_enabled {
            return Err(ToolError::UnknownTool);
        } else if name == "load_instructions" {
            serde_json::from_value::<Selection>(arguments.clone())
                .is_ok_and(|s| s.paths.len() <= 16 && s.skills.len() <= 8)
        } else if name == "repository_fallback" {
            serde_json::from_value::<Fallback>(arguments.clone()).is_ok()
        } else if context::NAMES.contains(&name) {
            Request::parse(name, arguments.clone()).is_ok()
        } else {
            return self.coding.validate(name, arguments);
        };

        if valid {
            Ok(())
        } else {
            Err(ToolError::InvalidArguments)
        }
    }

    async fn execute(&mut self, call: &ValidatedCall, cancellation: &Cancellation) -> ToolOutcome {
        if cancellation.is_cancelled() {
            return ToolOutcome::Failed(ToolError::Interrupted);
        }

        if self.session.status().await.is_err() {
            return ToolOutcome::Failed(ToolError::Denied);
        }
        if context::NAMES.contains(&call.name.as_str()) && !self.native_context_enabled {
            return ToolOutcome::Failed(ToolError::UnknownTool);
        }

        #[cfg(feature = "nano-mcp")]
        if self
            .mcp
            .as_ref()
            .is_some_and(|mcp| mcp.contains(&call.name))
        {
            let remote_name = self
                .mcp
                .as_ref()
                .and_then(|mcp| mcp.graph_remote_name(&call.name));
            let normalized = if let Some(name) = remote_name {
                match self
                    .context
                    .normalized_graph_arguments(name, call.arguments.clone())
                    .await
                {
                    Ok(arguments) => Some(ValidatedCall {
                        call_id: call.call_id.clone(),
                        provider_call_id: call.provider_call_id.clone(),
                        name: call.name.clone(),
                        arguments,
                    }),
                    Err(_) => {
                        return ToolOutcome::Failed(ToolError::Context(json!({
                            "code":"graph_budget_unavailable"
                        })));
                    }
                }
            } else {
                None
            };
            self.invalidate_unknown().await;
            let outcome = self
                .mcp
                .as_mut()
                .unwrap()
                .execute(normalized.as_ref().unwrap_or(call), cancellation)
                .await;
            if self.working_set_enabled {
                let writers = self.coding.commands.potentially_active_writers() > 0;
                self.observations
                    .extend(self.refresh.prepare(&self.session, writers).await);
            }
            return outcome;
        }

        if !context::NAMES.contains(&call.name.as_str()) && call.name != "load_instructions" {
            if matches!(call.name.as_str(), "exec" | "apply_patch") {
                self.invalidate_unknown().await;
                let outcome = self.coding.execute(call, cancellation).await;
                if self.working_set_enabled {
                    let writers = self.coding.commands.potentially_active_writers() > 0;
                    self.observations
                        .extend(self.refresh.prepare(&self.session, writers).await);
                }
                return outcome;
            }
            return self.coding.execute(call, cancellation).await;
        }

        let needs_graph = matches!(
            call.name.as_str(),
            "repository_graph_status" | "repository_search" | "repository_context"
        ) || matches!(
            call.name.as_str(),
            "project_context_search" | "project_context"
        ) && call.arguments["domain"] != "memory";
        if needs_graph && self.working_set_enabled {
            if let Some(observation) = self.refresh.settle().await {
                self.record_refresh(observation);
            }
            if let Some(reason) = self.refresh.pending_reason() {
                return ToolOutcome::Failed(ToolError::Context(json!({
                    "code":reason, "canonical_fallback":false,
                    "message":"The bound repository overlay needs a refresh; use repository_fallback for current workspace evidence"
                })));
            }
        }

        let result: Result<Value> = async {
            if call.name == "load_instructions" {
                let selected: Selection = serde_json::from_value(call.arguments.clone())?;
                Ok(serde_json::to_value(
                    self.instructions
                        .load(&selected.paths, &selected.skills)
                        .await?,
                )?)
            } else if call.name == "repository_fallback" {
                self.context
                    .fallback(
                        serde_json::from_value(call.arguments.clone())?,
                        cancellation,
                    )
                    .await
            } else {
                Ok(serde_json::to_value(
                    self.context
                        .retrieve(
                            &call.name,
                            Request::parse(&call.name, call.arguments.clone())?,
                        )
                        .await?,
                )?)
            }
        }
        .await;

        match result {
            Ok(value) if super::journal::encode(&value, 32 * 1024).is_ok() => {
                ToolOutcome::Success(value)
            }
            Ok(_) => ToolOutcome::Failed(ToolError::OutputLimit),
            Err(error) => ToolOutcome::Failed(ToolError::Context(
                json!({"code":"context_unavailable", "message":error.to_string().chars().take(256).collect::<String>(), "canonical_fallback":false}),
            )),
        }
    }

    async fn shutdown(&mut self) -> bool {
        // A running command can leave the overlay dirty without a scheduled
        // refresh. Stop and join owned writers before publishing their bytes.
        let coding_clean = self.coding.shutdown().await;
        #[cfg(feature = "nano-mcp")]
        if let Some(mcp) = &mut self.mcp
            && !mcp.shutdown().await
        {
            return false;
        }
        if !coding_clean {
            return false;
        }
        if self.working_set_enabled {
            self.before_mutation().await;
            for observation in self.refresh.prepare(&self.session, false).await {
                self.record_refresh(observation);
            }
            self.before_mutation().await;
        }
        true
    }

    async fn interrupted(&mut self) -> Option<ToolOutcome> {
        #[cfg(feature = "nano-mcp")]
        if let Some(mcp) = &mut self.mcp {
            return mcp.interrupted().await;
        }
        None
    }
}
