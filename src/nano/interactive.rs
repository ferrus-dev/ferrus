//! Taskless HQ conversation using the native engine, with no task lifecycle authority.

use super::{
    binding::Binding,
    commands::ExecutionBackend,
    engine::Engine,
    journal::Journal,
    native::NativeTools,
    provider::{Message, Provider},
    session::{Limits, Record, SessionCommand, SessionEnd, SessionIdentity},
    tools::*,
};
use crate::server::tools::approve::{CanonicalApprovalLock, acquire_canonical_approval_lock_at};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::sync::{Arc, Weak};
use tokio::task::JoinHandle;

pub(super) struct InteractiveTools<B: ExecutionBackend> {
    native: NativeTools<B>,
    binding: Binding,
    stop: Cancellation,
    pending: Option<JoinHandle<Result<Value>>>,
    workspace_lock: Option<Arc<CanonicalApprovalLock>>,
    workspace_owner: Weak<CanonicalApprovalLock>,
}

impl<B: ExecutionBackend> InteractiveTools<B> {
    pub(super) fn new(native: NativeTools<B>, binding: Binding, stop: Cancellation) -> Self {
        Self {
            native,
            binding,
            stop,
            pending: None,
            workspace_lock: None,
            workspace_owner: Weak::new(),
        }
    }

    fn release_workspace_if_quiet(&mut self) {
        if self.pending.is_none() {
            self.native.coding.commands.set_execution_guard(None);
            self.workspace_lock = None;
        }
    }

    async fn check(&mut self, call: &ValidatedCall) -> ToolOutcome {
        if self.validate(&call.name, &call.arguments).is_err() {
            return ToolOutcome::Failed(ToolError::InvalidArguments);
        }
        self.native.invalidate_unknown().await;
        if !self.native.coding.commands.quiesce().await {
            return ToolOutcome::Unknown(ToolError::Failed);
        }
        self.native.coding.workspace.invalidate_for_command();
        let (binding, stop) = (self.binding.clone(), self.stop.clone());
        self.pending = Some(tokio::spawn(async move {
            binding.status().await?;
            let config = crate::config::Config::load_from(binding.project_root()).await?;
            let log = binding.project_root().join(".ferrus/logs").join(format!(
                "check_nano_{}_{}.txt",
                binding.run_id(),
                crate::project::allocate_run_id("check", "nano"),
            ));
            tokio::fs::create_dir_all(log.parent().unwrap()).await?;
            let (passed, report, _) =
                super::checks::run(&config, binding.workspace(), &log, &stop).await?;
            Ok(json!({"passed":passed,"report":report,"log":log,"managed":false}))
        }));
        let result = self.pending.as_mut().unwrap().await;
        self.pending = None;
        check_outcome(result)
    }
}

impl<B: ExecutionBackend> Tools for InteractiveTools<B> {
    async fn prepare_context(
        &mut self,
        messages: &[Message],
        stop: &Cancellation,
    ) -> Result<Option<super::working_set::Preparation>, ToolError> {
        self.release_workspace_if_quiet();
        self.native.prepare_context(messages, stop).await
    }
    fn effect_plan(&self, call: &ValidatedCall) -> Option<EffectPlan> {
        self.native.effect_plan(call)
    }
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        let mut tools = self.native.descriptors();
        tools.push(ToolDescriptor {
            name: "check".into(),
            description: "Run configured workspace checks after stopping owned writers. Does not claim or change any Ferrus task.".into(),
            input_schema: json!({"type":"object", "properties":{}, "additionalProperties":false}),
        });
        tools
    }
    fn validate(&self, name: &str, args: &Value) -> Result<(), ToolError> {
        if name == "check" {
            return if args.as_object().is_some_and(|args| args.is_empty()) {
                Ok(())
            } else {
                Err(ToolError::InvalidArguments)
            };
        }
        self.native.validate(name, args)
    }
    async fn execute(&mut self, call: &ValidatedCall, cancellation: &Cancellation) -> ToolOutcome {
        let may_write = matches!(call.name.as_str(), "exec" | "apply_patch" | "check")
            || call.name.starts_with("mcp_");
        if may_write && self.workspace_lock.is_none() {
            if let Some(lock) = self.workspace_owner.upgrade() {
                self.workspace_lock = Some(lock);
            } else {
                let path = self.binding.data_dir().join("canonical-approval.lock");
                let result = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return ToolOutcome::Failed(ToolError::Interrupted),
                    lock = acquire_canonical_approval_lock_at(&path, self.binding.run_id()) => lock,
                };
                match result {
                    Ok(lock) => self.workspace_lock = Some(Arc::new(lock)),
                    Err(_) => return ToolOutcome::Failed(ToolError::Failed),
                }
            }
            self.workspace_owner = Arc::downgrade(self.workspace_lock.as_ref().unwrap());
        }
        if may_write {
            self.native.coding.commands.set_execution_guard(
                self.workspace_lock
                    .clone()
                    .map(|lock| lock as Arc<dyn Send + Sync>),
            );
        }
        let outcome = if call.name == "check" {
            self.check(call).await
        } else {
            self.native.execute(call, cancellation).await
        };
        // An exec reply can precede process completion. Keep the lock until
        // every owned writer is joined or has a definitive terminal state.
        self.release_workspace_if_quiet();
        outcome
    }
    async fn interrupted(&mut self) -> Option<ToolOutcome> {
        if let Some(task) = self.pending.take() {
            self.stop.cancel();
            return Some(check_outcome(task.await));
        }
        self.native.interrupted().await
    }
    async fn shutdown(&mut self) -> bool {
        self.stop.cancel();
        let joined = match self.pending.take() {
            Some(task) => task.await.is_ok(),
            None => true,
        };
        let stopped = self.native.shutdown().await && joined;
        if stopped {
            self.native.coding.commands.set_execution_guard(None);
            self.workspace_lock = None;
        }
        stopped
    }
}

impl<B: ExecutionBackend> Drop for InteractiveTools<B> {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

fn check_outcome(result: Result<Result<Value>, tokio::task::JoinError>) -> ToolOutcome {
    match result {
        Ok(Ok(value)) => ToolOutcome::Success(value),
        Ok(Err(_)) => ToolOutcome::Failed(ToolError::Failed),
        Err(_) => ToolOutcome::Unknown(ToolError::Failed),
    }
}

struct InteractiveHost(Binding);
impl Host for InteractiveHost {
    async fn authorize(&mut self, _: &ValidatedCall) -> Result<(), ToolError> {
        self.0
            .status()
            .await
            .map(|_| ())
            .map_err(|_| ToolError::Denied)
    }
    fn committed(&mut self, _: &Record) {}
}

pub(crate) async fn run<P: Provider, B: ExecutionBackend, J: Journal>(
    binding: Binding,
    limits: Limits,
    provider: P,
    native: NativeTools<B>,
    journal: J,
    cancellation: &Cancellation,
) -> Result<SessionEnd> {
    ensure!(
        binding.managed().is_none() && journal.interactive(),
        "Expected taskless interactive session"
    );
    binding.status().await?;
    #[cfg(feature = "nano-mcp")]
    let mut native = native;
    #[cfg(feature = "nano-mcp")]
    if let Some(path) = native.mcp_config.take() {
        native.mcp = Some(
            super::mcp::McpTools::connect_taskless(&path, &native.descriptors(), cancellation)
                .await?,
        );
    }
    let mut instructions = native.instructions.load(&[], &[]).await?;
    instructions
        .documents
        .retain(|document| !matches!(document.kind, super::instructions::Kind::RuntimePolicy));
    let input = instructions.constraint_text(
        limits
            .context_bytes
            .saturating_sub(super::instructions::INTERACTIVE_POLICY.len()),
    )?;
    let identity = SessionIdentity {
        session_id: binding.run_id().into(),
        project_id: binding.project_id().into(),
        task_id: None,
        run_id: Some(binding.run_id().into()),
    };
    let host = InteractiveHost(binding.clone());
    let tools = InteractiveTools::new(native, binding, cancellation.clone());
    let mut engine = Engine::new(identity, limits, provider, tools, host, journal)?;
    engine.set_system_prompt(super::instructions::INTERACTIVE_POLICY)?;
    // Do not embed the inference/tool future in the host's launch state.
    Box::pin(engine.run(SessionCommand::Start { input }, cancellation)).await
}
