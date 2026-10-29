//! Taskless Supervisor profiles. Authoring reuses Ferrus operations after a host-only
//! confirmation of immutable arguments; no task lease or review authority is created.

use super::{
    binding::Binding,
    commands,
    engine::Engine,
    frontend::{self, Approval},
    instructions,
    journal::{FileJournal, Quotas},
    native::NativeTools,
    providers::openai::OpenAi,
    session::*,
    tools::*,
};
use crate::{
    agents::DefinitionMode,
    project,
    server::tools::{archive_spec, create_spec, enqueue_task},
};
use anyhow::{Context as _, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::sync::{mpsc, oneshot};

mod artifacts;

const AUTHORING_BYTES: usize = 24 * 1024;

#[derive(Clone)]
struct Scope {
    binding: Binding,
    mode: DefinitionMode,
    archive_path: Option<String>,
}

impl Scope {
    async fn check(&self) -> Result<()> {
        self.binding.status().await?;
        ensure!(
            matches!(&self.binding, Binding::Interactive { definition: Some(mode), .. } if *mode == self.mode),
            "Supervisor profile requires a matching taskless Supervisor binding"
        );
        if self.mode == DefinitionMode::Archive {
            ensure!(
                project::read_project_selection().await?.selected_spec == self.archive_path,
                "Selected archive specification changed; start a new archive session"
            );
        }

        Ok(())
    }

    fn operation(&self, call: &ValidatedCall) -> Result<Operation> {
        let operation = Operation::parse(&call.name, &call.arguments)?;
        ensure!(
            matches!(
                (&operation, self.mode),
                (Operation::Enqueue(_), DefinitionMode::Task)
                    | (Operation::Spec(_), DefinitionMode::Spec)
                    | (Operation::Archive(_), DefinitionMode::Archive)
            ),
            "Tool is unavailable in this Supervisor profile"
        );
        if let Operation::Archive(input) = &operation {
            ensure!(
                self.archive_path.as_deref() == Some(input.spec_path.as_str()),
                "Archive request must target the specification pinned at launch"
            );
        }

        Ok(operation)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskInput {
    description: String,
    spec_path: Option<String>,
    milestone_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveInput {
    spec_path: String,
    outcome: String,
}

#[derive(Debug, Clone)]
enum Operation {
    Enqueue(TaskInput),
    Spec(String),
    Archive(ArchiveInput),
}

impl Operation {
    fn preview(&self) -> String {
        match self {
            Self::Enqueue(input) => {
                let mut text = input.description.clone();
                if let (Some(path), Some(milestone)) = (&input.spec_path, &input.milestone_id) {
                    text = format!("Specification: {path}\nMilestone: {milestone}\n\n{text}");
                }

                text
            }
            Self::Spec(markdown) => markdown.clone(),
            Self::Archive(input) => {
                format!("Specification: {}\n\n{}", input.spec_path, input.outcome)
            }
        }
    }

    fn parse(name: &str, arguments: &Value) -> Result<Self> {
        ensure!(
            serde_json::to_vec(arguments)?.len() <= AUTHORING_BYTES,
            "Authoring request exceeds byte limit"
        );

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wrapped<T> {
            input: T,
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Markdown {
            markdown: String,
        }
        Ok(match name {
            "enqueue_task" => {
                let input: TaskInput =
                    serde_json::from_value::<Wrapped<TaskInput>>(arguments.clone())?.input;
                ensure!(
                    !input.description.trim().is_empty(),
                    "Task description is required"
                );
                ensure!(
                    input.spec_path.is_some() == input.milestone_id.is_some(),
                    "Task origin requires both spec_path and milestone_id"
                );
                ensure!(
                    input
                        .spec_path
                        .as_ref()
                        .is_none_or(|v| !v.trim().is_empty())
                        && input
                            .milestone_id
                            .as_ref()
                            .is_none_or(|v| !v.trim().is_empty()),
                    "Empty task origin"
                );
                Self::Enqueue(input)
            }
            "create_spec" => {
                let markdown = serde_json::from_value::<Markdown>(arguments.clone())?.markdown;
                ensure!(
                    markdown.lines().any(|line| line
                        .trim()
                        .strip_prefix("# ")
                        .is_some_and(|title| !title.trim().is_empty())),
                    "Specification requires a title"
                );
                ensure!(
                    !markdown
                        .lines()
                        .any(|line| line.trim_start().starts_with('#')
                            && line
                                .trim()
                                .trim_start_matches('#')
                                .trim()
                                .trim_end_matches('#')
                                .trim()
                                .eq_ignore_ascii_case("Outcome")),
                    "Outcome authoring belongs to the archive profile"
                );
                let spec = crate::specs::parse_spec("", &markdown);
                let mut ids = std::collections::HashSet::new();
                ensure!(
                    !spec.milestones.is_empty()
                        && markdown
                            .lines()
                            .filter(|line| crate::specs::parse_milestone_header(line).is_some())
                            .count()
                            == spec.milestones.len()
                        && spec
                            .milestones
                            .iter()
                            .all(|m| !m.title.trim().is_empty() && ids.insert(&m.id)),
                    "Specification requires milestones with unique stable IDs"
                );
                Self::Spec(markdown)
            }
            "archive_spec" => {
                let input =
                    serde_json::from_value::<Wrapped<ArchiveInput>>(arguments.clone())?.input;
                ensure!(
                    !input.spec_path.trim().is_empty() && !input.outcome.trim().is_empty(),
                    "Archive path and Outcome are required"
                );
                Self::Archive(input)
            }
            _ => anyhow::bail!("Not an authoring operation"),
        })
    }

    async fn preflight(&self, root: &std::path::Path) -> Result<()> {
        if let Self::Enqueue(input) = self
            && let (Some(path), Some(id)) = (&input.spec_path, &input.milestone_id)
        {
            super::workspace::instruction_target(path)?;
            let (markdown, _) = super::workspace::instruction_file(root, path, 96 * 1024)?
                .context("Task origin specification is missing")?;
            let spec = crate::specs::parse_spec(path, &markdown);
            ensure!(
                spec.milestones.iter().any(|m| m.id == *id && !m.completed),
                "Task origin must identify an incomplete specification milestone"
            );
            ensure!(
                project::find_non_terminal_task_by_origin(path, id)
                    .await?
                    .is_none(),
                "Milestone already has a non-terminal task"
            );
        }

        Ok(())
    }

    async fn run(self) -> Result<String> {
        match self {
            Self::Enqueue(input) => {
                enqueue_task::run(input.description, input.spec_path, input.milestone_id).await
            }
            Self::Spec(markdown) => create_spec::run(markdown).await,
            Self::Archive(input) => archive_spec::run(input.spec_path, input.outcome).await,
        }
    }
}

fn authoring_name(name: &str) -> bool {
    matches!(name, "enqueue_task" | "create_spec" | "archive_spec")
}

fn readable(name: &str) -> bool {
    matches!(
        name,
        "read_file" | "search_text" | "load_instructions" | "repository_fallback"
    ) || super::context_request::NAMES.contains(&name)
}

struct SupervisorTools {
    operation_schema: Value,
    native: NativeTools<commands::TrustedLocal>,
    scope: Scope,
    goal: usize,
    saved: usize,
    pending: Option<tokio::task::JoinHandle<Result<String>>>,
}

impl SupervisorTools {
    async fn finish_operation(&mut self) -> ToolOutcome {
        let Some(pending) = self.pending.as_mut() else {
            return ToolOutcome::Unknown(ToolError::Interrupted);
        };
        let result = pending.await;
        self.pending = None;
        match result {
            Ok(Ok(message)) => {
                self.saved += 1;
                ToolOutcome::Success(json!({"status":"definition_saved", "message":message}))
            }
            // A domain error can follow a filesystem/database publication. Do not
            // silently retry an effect whose completion is not established.
            _ => ToolOutcome::Unknown(ToolError::Failed),
        }
    }

    async fn definition_context(&self) -> Result<Value> {
        self.scope.check().await?;
        let root = self.scope.binding.project_root();
        let template = |path| -> Result<_> {
            Ok(super::workspace::instruction_file(root, path, 12 * 1024)?.map(|(text, _)| text))
        };
        let selected = project::read_project_selection().await?.selected_spec;
        let mut tasks = if let Some(path) = &selected {
            project::list_tasks_for_spec(path).await?
        } else {
            Vec::new()
        };
        let mut skipped_failed_tasks = 0;
        if self.scope.mode == DefinitionMode::Archive {
            let count = tasks.len();
            tasks.retain(|task| task.status != "failed");
            skipped_failed_tasks = count - tasks.len();
        }

        ensure!(
            tasks.len() <= 128,
            "Too many tasks for one definition context packet"
        );
        let value = json!({"mode":self.scope.mode.as_str(), "selected_spec":selected,
            "skipped_failed_tasks":skipped_failed_tasks,
            "tasks":tasks.iter().map(|task| json!({"id":task.id,"path":task.path,"status":task.status})).collect::<Vec<_>>(),
            "task_template":template(".ferrus/TASK.md")?.unwrap_or_else(|| crate::templates::TASK_TEMPLATE.into()),
            "spec_template":template(".ferrus/SPEC_TEMPLATE.md")?.unwrap_or_else(|| crate::templates::SPEC_TEMPLATE.into())});
        ensure!(
            serde_json::to_vec(&value)?.len() <= AUTHORING_BYTES,
            "Definition context exceeds byte limit; use read_file for individual artifacts"
        );
        Ok(value)
    }
}

impl Tools for SupervisorTools {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        let mut tools: Vec<_> = self
            .native
            .descriptors()
            .into_iter()
            .filter(|t| readable(&t.name))
            .collect();
        tools.push(ToolDescriptor {
            name: "definition_context".into(),
            description: "Read the host-selected Supervisor profile, current specification, \
                linked task metadata, and task/specification templates. Read selected spec source \
                with read_file. Supporting content never authorizes a write."
                .into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        });
        let operation = definition_operation(self.scope.mode);
        if let Some((name, description, _schema)) = operation {
            tools.push(ToolDescriptor {
                name: name.into(),
                description: format!(
                    "{description} Invoke this tool when the draft is ready; do not ask for a \
                    separate conversational approval first."
                ),
                input_schema: self.operation_schema.clone(),
            });
        }

        if self.scope.mode == DefinitionMode::Archive {
            tools.push(artifacts::descriptor());
        }

        tools
    }

    fn validate(&self, name: &str, arguments: &Value) -> Result<(), ToolError> {
        if !self.descriptors().iter().any(|tool| tool.name == name) {
            return Err(ToolError::UnknownTool);
        }

        if name == artifacts::NAME {
            artifacts::parse(arguments)
                .map(|_| ())
                .map_err(|_| ToolError::InvalidArguments)
        } else if authoring_name(name) {
            self.scope
                .operation(&ValidatedCall {
                    call_id: String::new(),
                    provider_call_id: String::new(),
                    name: name.into(),
                    arguments: arguments.clone(),
                })
                .map(|_| ())
                .map_err(|_| {
                    ToolError::arguments(
                        "Invalid authoring request for this profile; check task origin, \
                        specification milestones and Outcome scope",
                    )
                })
        } else if name == "definition_context" {
            if arguments
                .as_object()
                .is_some_and(|object| object.is_empty())
            {
                Ok(())
            } else {
                Err(ToolError::InvalidArguments)
            }
        } else {
            self.native.validate(name, arguments)
        }
    }

    async fn prepare_context(
        &mut self,
        messages: &[super::provider::Message],
        stop: &Cancellation,
    ) -> Result<Option<super::working_set::Preparation>, ToolError> {
        self.native.prepare_context(messages, stop).await
    }

    async fn execute(&mut self, call: &ValidatedCall, stop: &Cancellation) -> ToolOutcome {
        if self.validate(&call.name, &call.arguments).is_err() {
            return ToolOutcome::Failed(ToolError::Denied);
        }

        if call.name == artifacts::NAME {
            return match artifacts::read(&self.scope, &call.arguments).await {
                Ok(value) => ToolOutcome::Success(value),
                Err(error) => ToolOutcome::Failed(ToolError::Lifecycle(json!({
                    "code":"archive_artifact_unavailable",
                    "message":super::conversation::display_text(&error.to_string(),1024)
                }))),
            };
        }

        if authoring_name(&call.name) {
            let operation = match self.scope.operation(call) {
                Ok(operation) => operation,
                Err(_) => return ToolOutcome::Failed(ToolError::InvalidArguments),
            };
            if let Err(error) = operation.preflight(self.scope.binding.project_root()).await {
                return ToolOutcome::Failed(ToolError::Lifecycle(
                    json!({"code":"invalid_definition", "message":error.to_string()}),
                ));
            }

            let scope = self.scope.clone();
            self.pending = Some(tokio::spawn(async move {
                scope.check().await?;
                let path = scope.binding.data_dir().join("canonical-approval.lock");
                let _lock = crate::server::tools::approve::acquire_canonical_approval_lock_at(
                    &path,
                    scope.binding.run_id(),
                )
                .await?;
                scope.check().await?;
                operation.run().await
            }));
            self.finish_operation().await
        } else if call.name == "definition_context" {
            match self.definition_context().await {
                Ok(value) => ToolOutcome::Success(value),
                Err(error) => ToolOutcome::Failed(ToolError::Lifecycle(
                    json!({"code":"definition_context_unavailable", "message":error.to_string()}),
                )),
            }
        } else {
            self.native.execute(call, stop).await
        }
    }

    async fn interrupted(&mut self) -> Option<ToolOutcome> {
        if self.pending.is_some() {
            Some(self.finish_operation().await)
        } else {
            self.native.interrupted().await
        }
    }

    async fn shutdown(&mut self) -> bool {
        let clean = if self.pending.is_some() {
            matches!(self.finish_operation().await, ToolOutcome::Success(_))
        } else {
            true
        };
        self.native.shutdown().await && clean
    }

    fn end_reason(&self) -> Option<EndReason> {
        (self.goal > 0 && self.saved >= self.goal).then_some(EndReason::DefinitionCompleted)
    }
}

struct SupervisorHost {
    scope: Scope,
    approvals: mpsc::Sender<Approval>,
}

impl Host for SupervisorHost {
    async fn authorize(&mut self, call: &ValidatedCall) -> Result<(), ToolError> {
        self.scope.check().await.map_err(|_| ToolError::Denied)?;
        if authoring_name(&call.name) {
            let operation = self.scope.operation(call).map_err(|_| ToolError::Denied)?;
            operation
                .preflight(self.scope.binding.project_root())
                .await
                .map_err(|error| {
                    ToolError::Lifecycle(
                        json!({"code":"invalid_definition", "message":error.to_string()}),
                    )
                })?;
            let (reply, response) = oneshot::channel();
            self.approvals
                .send(Approval {
                    tool: call.name.clone(),
                    content: operation.preview(),
                    reply,
                })
                .await
                .map_err(|_| ToolError::Denied)?;
            if !response.await.unwrap_or(false) {
                return Err(ToolError::Denied);
            }

            self.scope.check().await.map_err(|_| ToolError::Denied)?;
        }

        Ok(())
    }

    fn committed(&mut self, _: &Record) {}
}

pub(crate) async fn launch(
    mode: DefinitionMode,
    prompt: Option<String>,
    task_count: usize,
    config: PathBuf,
    model: Option<String>,
) -> Result<()> {
    frontend::require_terminal()?;
    ensure!(
        task_count > 0 && task_count <= 128,
        "Task count must be in 1..=128"
    );
    use crate::agent_id::*;
    ensure!(
        [ENV_TASK_ID, ENV_BASELINE_TREE, ENV_RUN_ID]
            .iter()
            .all(|key| std::env::var_os(key).is_none()),
        "Taskless Supervisor cannot inherit a task/run/baseline binding"
    );
    let agent =
        std::env::var(ENV_AGENT_ID).context("Supervisor launch requires a host agent identity")?;
    ensure!(
        agent.starts_with("supervisor:nano:"),
        "Expected a Nano Supervisor identity"
    );
    let root = project::canonical_project_root().await?;
    ensure!(
        std::env::current_dir()?.canonicalize()? == root,
        "Supervisor definitions must use the canonical project workspace"
    );
    let registration = project::read_project_registration_at(&root).await?;
    ensure!(
        std::fs::canonicalize(&registration.metadata.workspace_dir)? == root,
        "Supervisor project binding mismatch"
    );
    let run = project::allocate_run_id("supervisor", "nano");
    project::record_run_started_for_task_with_baseline(
        &run,
        "supervisor",
        &agent,
        std::process::id(),
        Some("current"),
        root.to_string_lossy().into_owned(),
        project::RunStartEvidence {
            baseline_tree: None,
            evaluation: None,
        },
    )
    .await?;
    let binding = Binding::Interactive {
        project_root: root,
        project_id: registration.metadata.id,
        data_dir: registration.data_dir,
        agent_id: agent,
        run_id: run.clone(),
        definition: Some(mode),
    };
    let result = Box::pin(run_bound(
        binding.clone(),
        mode,
        prompt,
        task_count,
        config,
        model,
    ))
    .await;
    let result = match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let path = failure_diagnostic(&binding, &error);
            project::record_runtime_event_best_effort(
                Some(run.clone()),
                "nano_supervisor_failed",
                json!({"reason":super::conversation::display_text(&error.to_string(),4096),
                    "diagnostic_path":path}),
            )
            .await;
            Err(error.context(format!(
                "Nano Supervisor {run} failed; diagnostics: {}",
                path.display()
            )))
        }
    };
    project::record_run_finished(&run, if result.is_ok() { 0 } else { 1 }).await?;
    result
}

fn failure_diagnostic(binding: &Binding, error: &anyhow::Error) -> PathBuf {
    use std::io::Write;
    let directory = binding
        .data_dir()
        .join("nano/sessions")
        .join(binding.run_id());
    let path = directory.join("diagnostics.log");
    let save = || -> Result<()> {
        for parent in [
            binding.data_dir().join("nano"),
            binding.data_dir().join("nano/sessions"),
            directory.clone(),
        ] {
            super::private::directory(&parent, false)?;
        }

        let mut file = super::private::file(&path, true)?;
        file.write_all(
            super::conversation::display_text(&format!("{error:#}\n"), 8192).as_bytes(),
        )?;
        file.sync_all()?;
        Ok(())
    };
    if let Err(error) = save() {
        tracing::warn!(error = ?error, "Could not save Nano Supervisor diagnostics");
    }

    path
}

async fn run_bound(
    binding: Binding,
    mode: DefinitionMode,
    prompt: Option<String>,
    task_count: usize,
    config: PathBuf,
    model: Option<String>,
) -> Result<()> {
    let settings = super::agent::load_config(&config, model.as_deref())?;
    let limits = Limits {
        tokens: settings.session_tokens,
        ..Default::default()
    };
    let journal = FileJournal::create(binding.data_dir(), binding.run_id(), Quotas::default())?;
    let stop = Cancellation::default();
    let signal = stop.clone();
    let listener = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        signal.cancel();
    });

    struct Signal(tokio::task::JoinHandle<()>);

    impl Drop for Signal {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _listener = Signal(listener);
    let coding = super::coding::CodingTools {
        workspace: super::workspace::Workspace::new(
            binding.workspace(),
            super::workspace::Limits::default(),
        )?,
        commands: commands::Commands::trusted_local(
            binding.workspace(),
            binding.run_id(),
            journal.directory(),
            commands::Limits::default(),
        )?,
    };
    let mut native = NativeTools::new(binding.clone(), coding, instructions::Limits::default())?;
    native.working_set_enabled = settings.working_set_enabled;
    native.native_context_enabled = settings.native_context_enabled;
    let scope = Scope {
        binding: binding.clone(),
        mode,
        archive_path: project::read_project_selection().await?.selected_spec,
    };
    ensure!(
        mode != DefinitionMode::Archive || scope.archive_path.is_some(),
        "Archive mode requires a selected specification"
    );
    scope.check().await?;
    let operation_schema = definition_schema(mode)?;
    let tools = SupervisorTools {
        operation_schema,
        native,
        scope: scope.clone(),
        goal: match mode {
            DefinitionMode::Plan => 0,
            DefinitionMode::Task => task_count,
            _ => 1,
        },
        saved: 0,
        pending: None,
    };
    let mut guidance = tools.native.instructions.load(&[], &[]).await?;
    guidance
        .documents
        .retain(|document| !matches!(document.kind, instructions::Kind::RuntimePolicy));
    let input = format!(
        "Supervisor profile: {}.\nLaunch request:\n{}\nScoped supporting instructions:\n{}",
        mode.as_str(),
        prompt.as_deref().unwrap_or("Wait for the user's request."),
        guidance.constraint_text(96 * 1024)?
    );
    let (journal, frontend) = frontend::session(
        journal,
        binding.run_id(),
        true,
        prompt
            .as_ref()
            .map(|_| "Begin the requested Supervisor session.".into()),
        None,
    );
    let (approvals, requests) = mpsc::channel(1);
    let identity = SessionIdentity {
        session_id: binding.run_id().into(),
        project_id: binding.project_id().into(),
        task_id: None,
        run_id: Some(binding.run_id().into()),
    };
    let mut engine = Engine::new(
        identity,
        limits,
        OpenAi::new(settings)?,
        tools,
        SupervisorHost { scope, approvals },
        journal,
    )?;
    engine.set_system_prompt(super::agent::SUPERVISOR_POLICY)?;
    let (done, finished) = tokio::sync::watch::channel(false);
    let session = async {
        let result = Box::pin(engine.run(SessionCommand::Start { input }, &stop)).await;
        done.send_replace(true);
        result
    };
    let (result, ui) = tokio::join!(session, frontend.authoring(requests).run(&stop, finished));
    ui?;
    let end = result?;
    ensure!(
        end.durable
            && matches!(
                end.reason,
                EndReason::Cancelled | EndReason::DefinitionCompleted
            ),
        "Nano Supervisor ended: {:?}, durable={}",
        end.reason,
        end.durable
    );
    Ok(())
}

fn definition_schema(mode: DefinitionMode) -> Result<Value> {
    Ok(definition_operation(mode)
        .map(|(_, _, schema)| serde_json::from_str(schema))
        .transpose()?
        .unwrap_or(Value::Null))
}

fn definition_operation(
    mode: DefinitionMode,
) -> Option<(&'static str, &'static str, &'static str)> {
    match mode {
        DefinitionMode::Plan => None,
        DefinitionMode::Task => Some((
            "enqueue_task",
            "Propose a complete task for user review, then enqueue it if the user confirms in the host UI.",
            enqueue_task::INPUT_SCHEMA,
        )),
        DefinitionMode::Spec => Some((
            "create_spec",
            "Propose a complete feature specification for user review, then save it if the user confirms in the host UI.",
            create_spec::INPUT_SCHEMA,
        )),
        DefinitionMode::Archive => Some((
            "archive_spec",
            "Propose an Outcome for the selected specification. The host displays it for user review and archives only after confirmation.",
            archive_spec::INPUT_SCHEMA,
        )),
    }
}

#[cfg(test)]
mod tests;
