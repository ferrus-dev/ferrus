pub mod agent_manager;
mod commands;
mod display;
mod state_watcher;
mod tui;

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tokio::sync::watch;

use crate::agent_id::{
    DEFAULT_AGENT_INDEX, ENV_AGENT_ID, ENV_BASELINE_TREE, ENV_SUPERVISOR_MODE, ENV_TASK_ID,
    ROLE_EXECUTOR, ROLE_SUPERVISOR, SUPERVISOR_MODE_ARCHIVE, agent_id,
};
use crate::agents::{AgentDisplayConfig, AgentRunMode, ExecutorAgent, SupervisorAgent};
use crate::checks::runner;
use crate::config::{Config, HqConfig, HqRole, update_hq_agent_config};
use crate::platform;
use crate::project::{ProjectSelection, TaskRecord};
use crate::specs::{self, MilestoneReadiness, SelectedMilestone};
use crate::state::{agents, store};
use crate::update_check;
use commands::{ModelTarget, ShellCommand, parse_command};
use display::Display;
use state_watcher::WatchedState;

pub async fn run(debug: bool) -> Result<()> {
    if let Err(err) = crate::project::touch_current_project().await {
        tracing::debug!(error = ?err, "skipped ferrus project touch");
    }
    if let Ok(recovery) = crate::project::recover_runtime_state().await
        && (recovery.interrupted_runs > 0 || recovery.expired_task_leases > 0)
    {
        tracing::info!(
            interrupted_runs = recovery.interrupted_runs,
            expired_task_leases = recovery.expired_task_leases,
            "recovered ferrus.db runtime state"
        );
    }
    reconcile_agent_pids().await;

    let (state_tx, state_rx) = watch::channel::<Option<WatchedState>>(None);
    tokio::spawn(state_watcher::watch(state_tx));

    let (msg_tx, msg_rx) = tokio::sync::mpsc::unbounded_channel::<tui::UiMessage>();
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<tui::HqInput>();

    let hq_config = load_hq_config_from_config().await;
    let supervisor_type = hq_config
        .as_ref()
        .map(|hq| hq.supervisor_name().to_string())
        .unwrap_or_default();
    let executor_type = hq_config
        .as_ref()
        .map(|hq| hq.executor_name().to_string())
        .unwrap_or_default();
    let (supervisor_version, executor_version) = load_agent_details(hq_config.as_ref()).await;

    let display = Display(msg_tx);
    let mut ctx = HqContext::new(state_rx.clone(), display.clone(), debug);
    if let Err(err) = ctx.seed_completed_task_announcements().await {
        tracing::debug!(error = ?err, "skipped completed task announcement seed");
    }
    if let Some(hq) = hq_config {
        ctx.set_hq_config(&hq);
    }

    let update_display = display.clone();
    tokio::spawn(async move {
        if let Some(message) = update_check::notification_message().await {
            update_display.tip(message);
        }
    });

    let mut tui_task = tokio::spawn(tui::run_tui(
        msg_rx,
        cmd_tx,
        state_rx.clone(),
        debug,
        supervisor_type,
        executor_type,
        supervisor_version,
        executor_version,
    ));

    let mut tui_finished = false;
    let mut scheduler_tick = tokio::time::interval(std::time::Duration::from_secs(2));
    scheduler_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let loop_result: Result<()> = loop {
        tokio::select! {
            _ = scheduler_tick.tick() => {
                if let Err(err) = ctx.reconcile_runtime_schedule().await {
                    tracing::debug!(error = ?err, "skipped runtime schedule reconciliation");
                }
            }
            changed = ctx.state_rx.changed() => {
                if changed.is_err() {
                    break Ok(());
                }
                let _ = ctx.state_rx.borrow_and_update();
            }
            maybe_cmd = cmd_rx.recv() => {
                match maybe_cmd {
                    Some(input) => {
                        let line = input.text.as_str();
                        if line.trim().is_empty() {
                            continue;
                        }
                        if line.trim() == "/quit" {
                            ctx.display.muted("Bye.");
                            break Ok(());
                        }
                        if let Err(err) = dispatch_with_human_question_target(
                            line,
                            input.human_question_task_id.as_deref(),
                            false,
                            &mut ctx,
                        )
                        .await
                        {
                            ctx.display.error(err.to_string());
                        }
                    }
                    None => break Ok(()),
                }
            }
            result = &mut tui_task => {
                tui_finished = true;
                break match result {
                    Ok(inner) => inner,
                    Err(err) => Err(err.into()),
                };
            }
        }
    };

    ctx.shutdown_all_headless().await;

    drop(ctx);
    if !tui_finished {
        match tui_task.await {
            Ok(result) => result?,
            Err(err) if err.is_cancelled() => {}
            Err(err) => return Err(err.into()),
        }
    }

    loop_result?;
    Ok(())
}

async fn load_hq_config_from_config() -> Option<HqConfig> {
    Config::load().await.ok().and_then(|cfg| cfg.hq)
}

async fn load_agent_details(hq: Option<&HqConfig>) -> (String, String) {
    let Some(hq) = hq else {
        return (String::new(), String::new());
    };
    let supervisor = match hq.supervisor_agent() {
        Ok(agent) => match agent.version_command() {
            Ok(command) => format_agent_details(
                agent.name(),
                &load_agent_version_from_version_command(command).await,
                agent.display_config(),
            ),
            Err(_) => format_agent_details(agent.name(), "", agent.display_config()),
        },
        Err(_) => String::new(),
    };
    let executor = match hq.executor_agent() {
        Ok(agent) => match agent.version_command() {
            Ok(command) => format_agent_details(
                agent.name(),
                &load_agent_version_from_version_command(command).await,
                agent.display_config(),
            ),
            Err(_) => format_agent_details(agent.name(), "", agent.display_config()),
        },
        Err(_) => String::new(),
    };
    (supervisor, executor)
}

async fn load_agent_version_from_version_command(command: std::process::Command) -> String {
    let Ok(output) = Command::from(command).output().await else {
        return String::new();
    };
    if !output.status.success() {
        return String::new();
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn format_agent_details(agent_name: &str, version: &str, config: AgentDisplayConfig) -> String {
    let version = normalize_agent_version(agent_name, version);
    let config = format_agent_display_config(config);
    match (version.as_deref(), config.as_deref()) {
        (Some(version), Some(config)) => format!("{version} ({config})"),
        (Some(version), None) => version.to_string(),
        (None, Some(config)) => format!("({config})"),
        (None, None) => String::new(),
    }
}

fn format_agent_display_config(config: AgentDisplayConfig) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(model) = config.model {
        parts.push(model);
    }
    if let Some(effort) = config.effort {
        parts.push(format!("effort: {effort}"));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn normalize_agent_version(agent_name: &str, version: &str) -> Option<String> {
    let mut version = version.trim();
    if version.is_empty() {
        return None;
    }

    if agent_name == "claude-code" {
        version = version
            .strip_suffix(" (Claude Code)")
            .unwrap_or(version)
            .trim();
    }

    let prefixes: &[&str] = match agent_name {
        "claude-code" => &["claude-code ", "claude "],
        "codex" => &["codex-cli ", "codex "],
        "goose" => &["goose "],
        "opencode" => &["opencode "],
        "qwen-code" => &["qwen-code ", "qwen "],
        _ => &[],
    };
    for prefix in prefixes {
        if let Some(stripped) = version.strip_prefix(prefix) {
            version = stripped.trim();
            break;
        }
    }

    if version.is_empty() {
        None
    } else {
        Some(version.to_string())
    }
}

#[cfg(test)]
async fn dispatch(line: &str, ctx: &mut HqContext) -> Result<()> {
    dispatch_with_human_question_target(line, None, true, ctx).await
}

async fn dispatch_with_human_question_target(
    line: &str,
    human_question_task_id: Option<&str>,
    allow_fifo_fallback: bool,
    ctx: &mut HqContext,
) -> Result<()> {
    // When state is AwaitingHuman, non-command input is treated as the human's answer.
    if !line.starts_with('/') {
        if human_question_task_id.is_some() {
            return ctx
                .answer_scoped_human_question_for_task(line.to_string(), human_question_task_id)
                .await;
        }
        if allow_fifo_fallback && ctx.has_pending_human_question().await? {
            return ctx.answer(line.to_string()).await;
        }
        anyhow::bail!("Commands must start with '/' -- try /status, /task, /quit");
    }

    match parse_command(line)? {
        ShellCommand::Quit => {
            ctx.display.muted("Bye.");
        }
        ShellCommand::Status => {
            let reg = agents::read_agents().await?;
            let watched = if let Some(watched) = ctx.state_rx.borrow().clone() {
                watched
            } else {
                WatchedState {
                    selected_spec_display: None,
                    selected_milestones: Vec::new(),
                }
            };
            ctx.display.status(&watched, &reg);
            if !ctx.headless.is_empty() {
                let mut lines = vec!["Headless agents:".to_string()];
                for (name, handle) in &ctx.headless {
                    let status = if handle.is_alive() {
                        "running"
                    } else {
                        "exited"
                    };
                    lines.push(format!(
                        "  {name} ({status}) -- tail logs: {}",
                        handle.log_path.display()
                    ));
                }
                ctx.display.info_block(lines);
            }
        }
        ShellCommand::Tasks => {
            let tasks = crate::project::list_tasks().await?;
            ctx.display.table(crate::runtime_table::task_lines(&tasks));
        }
        ShellCommand::Run { limit } => ctx.run_batch_plan(limit).await?,
        ShellCommand::Runs { limit } => {
            let runs = crate::project::list_runs(limit).await?;
            ctx.display.table(crate::runtime_table::run_lines(&runs));
        }
        ShellCommand::Events { limit, run_id } => {
            let events = crate::project::list_events(limit, run_id.clone()).await?;
            ctx.display.table(crate::runtime_table::event_lines(
                &events,
                run_id.as_deref(),
            ));
        }
        ShellCommand::Check { force } => ctx.check(force).await?,
        ShellCommand::Help => {
            ctx.display.info(concat!(
                "ferrus HQ commands:\n",
                "  /plan              Free-form planning session with the supervisor\n",
                "  /task              Queue one task from the next ready milestone, then run the scheduler\n",
                "  /task --manual     Queue one free-form task without spec context\n",
                "  /milestones        Select the current spec\n",
                "  /reset-spec        Clear the selected spec\n",
                "  /archive-spec      Summarize and archive completed selected spec artifacts\n",
                "  /spec              Draft, approve, and save a feature specification; offers archive first\n",
                "  /check             Run the Ferrus check gate deterministically from HQ\n",
                "  /check --force     Run configured checks from HQ without state requirements\n",
                "  /supervisor        Open an interactive supervisor session\n",
                "  /executor          Open an interactive executor session\n",
                "  /resume            Resume the executor headlessly; recovers Consultation too\n",
                "  /review            Manually spawn supervisor in review mode\n",
                "  /status            Show task state, agent list, and session log paths\n",
                "  /tasks             List SQLite task runtime rows\n",
                "  /run [--limit N]   Plan a batch run from ready milestones\n",
                "  /runs [--limit N]  List SQLite run attempts\n",
                "  /events [--limit N]\n",
                "                     List SQLite runtime events\n",
                "  /events --run <id> List SQLite runtime events for one run\n",
                "  /attach <name>     Show log path for a running headless agent\n",
                "  /stop              Stop all running agent sessions\n",
                "  /reset             Force-reset tasks and clear scoped artifacts\n",
                "  /init              Initialize ferrus in the current directory\n",
                "  /register          Register agent configs and permissions\n",
                "  /model <role> <model>\n",
                "                     Update the configured model override\n",
                "  /model <role> --clear\n",
                "                     Clear the configured model override\n",
                "  /quit              Exit HQ\n",
                "\n",
                "When an agent asks a question (state = AwaitingHuman):\n",
                "  Type your answer and press Enter; queued questions are shown one at a time.",
            ));
        }
        ShellCommand::Reset => ctx.reset().await?,
        ShellCommand::Stop => ctx.stop().await?,
        ShellCommand::Plan => ctx.plan().await?,
        ShellCommand::Task { manual } => ctx.task(manual, true).await?,
        ShellCommand::Milestones => ctx.milestones().await?,
        ShellCommand::ResetSpec => ctx.reset_spec_selection().await?,
        ShellCommand::ArchiveSpec => {
            let _ = ctx.archive_spec().await?;
        }
        ShellCommand::Spec => ctx.spec().await?,
        ShellCommand::Supervisor => ctx.supervisor_interactive().await?,
        ShellCommand::Executor => ctx.executor_interactive().await?,
        ShellCommand::Resume => ctx.resume().await?,
        ShellCommand::Review => ctx.review().await?,
        ShellCommand::Attach { name } => {
            if let Some(handle) = ctx.headless.get(&name) {
                let log = handle.log_path.display().to_string();
                ctx.display.info(format!(
                    "{name} runs headlessly -- no terminal to attach.\n\
                     Tail its log to observe: tail -f {log}"
                ));
            } else {
                ctx.display.error(format!(
                    "No agent named '{name}'. Run /status to see active agents."
                ));
            }
        }
        ShellCommand::Init { agents_path } => {
            crate::cli::commands::init::run(agents_path).await?;
        }
        ShellCommand::Register {
            supervisor,
            supervisor_model,
            executor,
            executor_model,
        } => {
            let sup = supervisor.as_deref().and_then(parse_agent_type);
            let exe = executor.as_deref().and_then(parse_agent_type);
            if sup.is_none() && exe.is_none() {
                ctx.display
                    .error("At least one of --supervisor or --executor required");
            } else {
                crate::cli::commands::register::run(sup, supervisor_model, exe, executor_model)
                    .await?;
                ctx.reload_hq_config().await?;
            }
        }
        ShellCommand::Model {
            target,
            model,
            clear,
        } => {
            let model = match (model, clear) {
                (Some(model), false) => Some(model),
                (None, true) => None,
                _ => anyhow::bail!(
                    "Usage: /model <supervisor|executor> <model> | /model <supervisor|executor> --clear"
                ),
            };
            ctx.update_model(target, model.as_deref()).await?;
        }
    }
    Ok(())
}

fn parse_agent_type(s: &str) -> Option<crate::cli::commands::register::Agent> {
    use crate::cli::commands::register::Agent;

    match s {
        "claude-code" => Some(Agent::ClaudeCode),
        "codex" => Some(Agent::Codex),
        "goose" => Some(Agent::Goose),
        "opencode" => Some(Agent::OpenCode),
        "qwen-code" => Some(Agent::QwenCode),
        _ => None,
    }
}

struct ResumeGuard {
    display: Display,
    active: bool,
}

impl ResumeGuard {
    fn new(display: Display) -> Self {
        Self {
            display,
            active: true,
        }
    }

    fn resume_now(&mut self) {
        if self.active {
            self.display.resume();
            self.active = false;
        }
    }
}

impl Drop for ResumeGuard {
    fn drop(&mut self) {
        self.resume_now();
    }
}

fn clear_primary_screen() {
    use std::io::Write as _;

    let mut stdout = std::io::stdout();
    let _ = crossterm::execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0)
    );
    let _ = stdout.flush();
}

fn capture_interactive_stderr(
    child: &mut tokio::process::Child,
) -> Option<tokio::task::JoinHandle<String>> {
    use tokio::io::AsyncReadExt as _;

    let mut stderr = child.stderr.take()?;
    Some(tokio::spawn(async move {
        let mut captured = Vec::new();
        let mut buf = [0; 8192];
        loop {
            let read = stderr.read(&mut buf).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            let chunk = &buf[..read];
            captured.extend_from_slice(chunk);
            if captured.len() > 8192 {
                let extra = captured.len() - 8192;
                captured.drain(0..extra);
            }
        }
        String::from_utf8_lossy(&captured).trim().to_string()
    }))
}

async fn finish_interactive_stderr(handle: Option<tokio::task::JoinHandle<String>>) -> String {
    match handle {
        Some(handle) => handle.await.unwrap_or_default(),
        None => String::new(),
    }
}

fn interactive_exit_error(
    role: &str,
    agent_type: &str,
    status: std::process::ExitStatus,
    stderr: &str,
) -> String {
    let mut message = format!("{role} agent ({agent_type}) exited with {status}");
    if !stderr.trim().is_empty() {
        message.push_str("\n\nstderr:\n");
        message.push_str(stderr.trim());
    }
    message
}

enum TaskMilestoneSelection {
    UseFallback,
    Use(SelectedMilestone),
    Stop,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RunPlanMilestone {
    id: String,
    marker: String,
    title: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SkippedRunMilestone {
    id: String,
    marker: String,
    title: String,
    reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RunPlan {
    spec_path: String,
    eligible: Vec<RunPlanMilestone>,
    skipped: Vec<SkippedRunMilestone>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SpecArchivePrompt {
    spec_path: String,
    task_count: usize,
}

impl ModelTarget {
    fn config_role(self) -> HqRole {
        match self {
            Self::Supervisor => HqRole::Supervisor,
            Self::Executor => HqRole::Executor,
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Supervisor => "Supervisor",
            Self::Executor => "Executor",
        }
    }
}

pub(crate) struct HqContext {
    pub(crate) supervisor: Option<std::sync::Arc<dyn SupervisorAgent>>,
    pub(crate) executor: Option<std::sync::Arc<dyn ExecutorAgent>>,
    /// Headless agent handles -- executor and reviewer both run without a PTY.
    pub(crate) headless: std::collections::HashMap<String, agent_manager::HeadlessHandle>,
    debug: bool,
    state_rx: watch::Receiver<Option<WatchedState>>,
    pub(crate) display: Display,
    announced_completed_tasks: HashSet<String>,
}

mod context;

async fn prepare_spec_session_files() -> Result<()> {
    crate::project::touch_current_project().await.context(
        "Cannot start /spec because Ferrus is not initialized. Run `ferrus init` first.",
    )?;

    let path = std::path::Path::new(".ferrus/SPEC_TEMPLATE.md");
    if !tokio::fs::try_exists(path).await.unwrap_or(false) {
        tokio::fs::write(path, crate::templates::SPEC_TEMPLATE)
            .await
            .context("Failed to write .ferrus/SPEC_TEMPLATE.md")?;
    }

    crate::project::clear_last_spec_path()
        .await
        .context("Failed to clear spec handoff metadata")
}

async fn build_run_plan(spec_path: &str) -> Result<RunPlan> {
    let spec = specs::load_spec(spec_path).await?;
    let mut eligible = Vec::new();
    let mut skipped = Vec::new();

    for item in spec.milestone_plan() {
        let milestone = item.milestone;
        match item.readiness {
            MilestoneReadiness::Done => skipped.push(SkippedRunMilestone {
                id: milestone.id,
                marker: milestone.marker,
                title: milestone.title,
                reason: "done".to_string(),
            }),
            MilestoneReadiness::Pending => skipped.push(SkippedRunMilestone {
                id: milestone.id,
                marker: milestone.marker,
                title: milestone.title,
                reason: format!("waiting for {}", item.blocked_by.join(", ")),
            }),
            MilestoneReadiness::Ready => {
                if let Some(task) =
                    crate::project::find_non_terminal_task_by_origin(spec_path, &milestone.id)
                        .await?
                {
                    skipped.push(SkippedRunMilestone {
                        id: milestone.id,
                        marker: milestone.marker,
                        title: milestone.title,
                        reason: format!("task {} is {}", task.id, task.status),
                    });
                } else {
                    eligible.push(RunPlanMilestone {
                        id: milestone.id,
                        marker: milestone.marker,
                        title: milestone.title,
                    });
                }
            }
        }
    }

    Ok(RunPlan {
        spec_path: spec.path,
        eligible,
        skipped,
    })
}

fn run_plan_lines(plan: &RunPlan, selected_count: usize) -> Vec<String> {
    let mut lines = vec![
        "Run plan".to_string(),
        format!("spec      : {}", plan.spec_path),
        format!("eligible  : {}", plan.eligible.len()),
        format!("selected  : {selected_count}"),
    ];

    if !plan.eligible.is_empty() {
        lines.push(String::new());
        lines.push("selected milestones:".to_string());
        for milestone in plan.eligible.iter().take(selected_count) {
            lines.push(format!(
                "  {}  {:<8} {}",
                milestone.marker, milestone.id, milestone.title
            ));
        }
    }

    if !plan.skipped.is_empty() {
        lines.push(String::new());
        lines.push("skipped milestones:".to_string());
        for milestone in &plan.skipped {
            lines.push(format!(
                "  {}  {:<8} {} ({})",
                milestone.marker, milestone.id, milestone.title, milestone.reason
            ));
        }
    }

    lines
}

fn run_plan_prompt_context(plan: &RunPlan, selected_count: usize) -> String {
    let mut lines = vec![
        format!("Spec: {}", plan.spec_path),
        format!("Task count: {selected_count}"),
        "Milestones:".to_string(),
    ];

    for milestone in plan.eligible.iter().take(selected_count) {
        lines.push(format!(
            "- Milestone ID: {}\n  Marker: {}\n  Title: {}",
            milestone.id, milestone.marker, milestone.title
        ));
    }

    lines.join("\n")
}

fn archive_spec_prompt_context(spec_path: &str, tasks: &[TaskRecord]) -> String {
    let mut lines = vec![
        format!("Spec path: {spec_path}"),
        format!("Task count: {}", tasks.len()),
        "Linked tasks:".to_string(),
    ];

    for task in tasks {
        lines.push(format!(
            "- Task ID: {}\n  Status: {}\n  Milestone ID: {}\n  Task path: {}\n  Run dir: {}",
            task.id,
            task.status,
            task.milestone_id.as_deref().unwrap_or("none"),
            task.path,
            crate::project::run_dir_for_task_display(&task.id),
        ));
    }

    lines.push(String::new());
    lines.push(
        "Review these files as needed, then draft one approved `## Outcome` section.".to_string(),
    );
    lines.join("\n")
}

async fn selected_spec_archive_prompt() -> Result<Option<SpecArchivePrompt>> {
    let selection = crate::project::read_project_selection().await?;
    let Some(spec_path) = selection
        .selected_spec
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    else {
        return Ok(None);
    };
    if !Path::new(spec_path).exists() {
        return Ok(None);
    }

    let spec = specs::load_spec(spec_path).await?;
    if spec.milestones.is_empty() || spec.milestones.iter().any(|milestone| !milestone.completed) {
        return Ok(None);
    }

    let tasks = crate::project::list_tasks_for_spec(spec_path).await?;
    if tasks.is_empty()
        || tasks.iter().any(|task| {
            task.status
                .parse::<crate::project::TaskStatus>()
                .map(|status| !status.is_terminal())
                .unwrap_or(true)
        })
        || !tasks.iter().any(task_has_unarchived_artifacts)
    {
        return Ok(None);
    }

    Ok(Some(SpecArchivePrompt {
        spec_path: spec.path,
        task_count: tasks.len(),
    }))
}

fn task_has_unarchived_artifacts(task: &TaskRecord) -> bool {
    let task_path = Path::new(&task.path);
    let task_file_in_checkout = task_path.starts_with(".ferrus/tasks") && task_path.exists();
    let run_dir_in_checkout = task_path.starts_with(".ferrus/tasks")
        && Path::new(&crate::project::run_dir_for_task_display(&task.id)).exists();
    task_file_in_checkout || run_dir_in_checkout
}

async fn new_task_ids_since(existing_task_ids: &HashSet<String>) -> Result<Vec<String>> {
    let tasks = crate::project::list_tasks().await?;
    let mut task_ids = tasks
        .into_iter()
        .filter(|task| !existing_task_ids.contains(&task.id))
        .map(|task| task.id)
        .collect::<Vec<_>>();
    task_ids.sort();
    Ok(task_ids)
}

fn selected_milestone_prompt_context(selected: &SelectedMilestone) -> String {
    format!(
        "spec_path: {}\nmilestone: {}\nmilestone_id: {}\ncompleted: {}\ndepends_on: {}",
        selected.spec_path,
        selected.milestone.display_title(),
        selected.milestone.id,
        if selected.milestone.completed {
            "yes"
        } else {
            "no"
        },
        selected.milestone.depends_on
    )
}

async fn reconcile_agent_pids() {
    use crate::state::agents::{AgentStatus, read_agents, write_agents};

    if let Ok(mut reg) = read_agents().await {
        let mut changed = false;
        for entry in &mut reg.agents {
            if entry.status == AgentStatus::Running {
                let alive = entry.pid.map(platform::pid_is_alive).unwrap_or(false);
                if !alive {
                    entry.pid = None;
                    entry.status = AgentStatus::Suspended;
                    changed = true;
                }
            }
        }
        if changed {
            let _ = write_agents(&reg).await;
        }
    }
}

mod workspace;
use workspace::*;

async fn executor_parallel_limit(configured: usize) -> Result<usize> {
    let configured = configured.max(1);
    if configured == 1 {
        return Ok(1);
    }

    let registration = crate::project::touch_current_project().await?;
    let project_root = PathBuf::from(&registration.metadata.workspace_dir);
    if git_is_work_tree(&project_root).await {
        Ok(configured)
    } else {
        Ok(1)
    }
}

fn occupied_executor_slots_from_handles<'a>(
    mut live_db_task_ids: HashSet<String>,
    live_headless_names: impl IntoIterator<Item = &'a str>,
) -> usize {
    let mut unscoped_live_handles = 0usize;
    for name in live_headless_names {
        if let Some(task_id) = task_id_from_scoped_agent_name(name) {
            live_db_task_ids.insert(task_id.to_string());
        } else {
            unscoped_live_handles += 1;
        }
    }
    live_db_task_ids.len() + unscoped_live_handles
}

fn task_id_from_scoped_agent_name(name: &str) -> Option<&str> {
    let mut parts = name.splitn(3, ':');
    let role = parts.next()?;
    parts.next()?;
    let task_id = parts.next()?;
    (role == ROLE_EXECUTOR && task_id.starts_with("t-")).then_some(task_id)
}

fn answered_human_owner_is_live(
    owner: &str,
    live_run_agents: &HashSet<String>,
    live_headless_owner: bool,
) -> bool {
    live_headless_owner || live_run_agents.contains(owner)
}

fn task_claim_blocks_spawn(
    task: &TaskRecord,
    expected_agent_id: &str,
    now: chrono::DateTime<chrono::Utc>,
    live_run_task_ids: &HashSet<String>,
) -> bool {
    if live_run_task_ids.contains(task.id.as_str()) {
        return true;
    }
    let Some(claimed_by) = task.claimed_by.as_deref() else {
        return false;
    };
    let lease_active = task
        .lease_until
        .as_deref()
        .and_then(|lease_until| chrono::DateTime::parse_from_rfc3339(lease_until).ok())
        .is_some_and(|lease_until| lease_until.with_timezone(&chrono::Utc) > now);
    claimed_by != expected_agent_id && lease_active
}

fn select_human_question(
    questions: Vec<crate::project::HumanQuestion>,
    task_id: Option<&str>,
) -> Result<crate::project::HumanQuestion> {
    if let Some(task_id) = task_id {
        return questions
            .into_iter()
            .find(|question| question.task_id == task_id)
            .ok_or_else(|| anyhow::anyhow!("Task {task_id} is not waiting for a human answer."));
    }

    questions
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No task is currently waiting for a human answer."))
}

fn is_resettable_task_status(status: &str) -> bool {
    status
        .parse::<crate::project::TaskStatus>()
        .is_ok_and(crate::project::TaskStatus::is_resettable)
}

fn is_executor_ready_task_status(status: &str) -> bool {
    status
        .parse::<crate::project::TaskStatus>()
        .is_ok_and(crate::project::TaskStatus::is_executor_ready)
}

fn select_executor_spawn_tasks<F>(
    ready_tasks: &[TaskRecord],
    slots: usize,
    mut is_live: F,
) -> Vec<&TaskRecord>
where
    F: FnMut(&TaskRecord) -> bool,
{
    ready_tasks
        .iter()
        .filter(|task| !is_live(task))
        .take(slots)
        .collect()
}

async fn actionable_consultation_tasks(
    tasks: &[TaskRecord],
    slots: usize,
    live_supervisor_task_ids: &HashSet<String>,
) -> Result<Vec<TaskRecord>> {
    let mut consultation_tasks = Vec::new();
    for task in tasks
        .iter()
        .filter(|task| task.status == crate::project::TaskStatus::Consultation.as_str())
    {
        if live_supervisor_task_ids.contains(&task.id)
            || consultation_response_is_ready(&task.id).await?
        {
            continue;
        }
        consultation_tasks.push(task.clone());
        if consultation_tasks.len() == slots {
            break;
        }
    }
    Ok(consultation_tasks)
}

async fn answered_consultation_tasks(tasks: &[TaskRecord]) -> Result<Vec<TaskRecord>> {
    let mut answered_tasks = Vec::new();
    for task in tasks
        .iter()
        .filter(|task| task.status == crate::project::TaskStatus::Consultation.as_str())
    {
        if consultation_response_is_ready(&task.id).await? {
            answered_tasks.push(task.clone());
        }
    }
    Ok(answered_tasks)
}

async fn consultation_response_is_ready(task_id: &str) -> Result<bool> {
    match store::read_consult_response_for_run_dir(&scoped_run_dir(task_id)).await {
        Ok(response) => Ok(!response.trim().is_empty()),
        Err(err) if is_not_found_error(&err) => Ok(false),
        Err(err) => Err(err),
    }
}

fn scoped_run_dir(task_id: &str) -> String {
    format!(".ferrus/runs/{task_id}")
}

fn is_not_found_error(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|err| err.kind() == std::io::ErrorKind::NotFound)
}

fn is_review_or_consultation_task_status(status: &str) -> bool {
    matches!(
        status.parse::<crate::project::TaskStatus>().ok(),
        Some(crate::project::TaskStatus::Reviewing | crate::project::TaskStatus::Consultation)
    )
}

fn completed_task_ids(tasks: &[TaskRecord]) -> impl Iterator<Item = String> + '_ {
    tasks
        .iter()
        .filter(|task| task.status == crate::project::TaskStatus::Complete.as_str())
        .map(|task| task.id.clone())
}

async fn latest_executor_workspace_for_task(
    task_id: &str,
) -> Result<Option<agent_manager::HeadlessWorkspace>> {
    let Some(run) = crate::project::list_runs(1000)
        .await?
        .into_iter()
        .find(|run| {
            run.task_id == task_id
                && run.role == ROLE_EXECUTOR
                && !run.workspace_path.trim().is_empty()
        })
    else {
        return Ok(None);
    };

    let workspace_dir = PathBuf::from(run.workspace_path);
    if !tokio::fs::try_exists(&workspace_dir).await? {
        tracing::debug!(
            task_id,
            workspace = %workspace_dir.display(),
            "executor workspace no longer exists; consultation will use canonical workspace"
        );
        return Ok(None);
    }

    let registration = crate::project::touch_current_project().await?;
    Ok(Some(agent_manager::HeadlessWorkspace {
        workspace_dir,
        project_root: PathBuf::from(registration.metadata.workspace_dir),
    }))
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
