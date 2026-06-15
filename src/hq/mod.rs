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
    DEFAULT_AGENT_INDEX, ENV_AGENT_ID, ENV_BASELINE_TREE, ENV_TASK_ID, ROLE_EXECUTOR,
    ROLE_SUPERVISOR, agent_id,
};
use crate::agents::{AgentRunMode, ExecutorAgent, SupervisorAgent};
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
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    let hq_config = load_hq_config_from_config().await;
    let supervisor_type = hq_config
        .as_ref()
        .map(|hq| hq.supervisor_name().to_string())
        .unwrap_or_default();
    let executor_type = hq_config
        .as_ref()
        .map(|hq| hq.executor_name().to_string())
        .unwrap_or_default();
    let (supervisor_version, executor_version) = load_agent_versions(hq_config.as_ref()).await;

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
                    Some(cmd) => {
                        let line = cmd.as_str();
                        if line.trim().is_empty() {
                            continue;
                        }
                        if line.trim() == "/quit" {
                            ctx.display.muted("Bye.");
                            break Ok(());
                        }
                        if let Err(err) = dispatch(line, &mut ctx).await {
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

async fn load_agent_versions(hq: Option<&HqConfig>) -> (String, String) {
    let Some(hq) = hq else {
        return (String::new(), String::new());
    };
    let supervisor = match hq.supervisor_agent() {
        Ok(agent) => match agent.version_command() {
            Ok(command) => load_agent_version_from_version_command(command).await,
            Err(_) => String::new(),
        },
        Err(_) => String::new(),
    };
    let executor = match hq.executor_agent() {
        Ok(agent) => match agent.version_command() {
            Ok(command) => load_agent_version_from_version_command(command).await,
            Err(_) => String::new(),
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

async fn dispatch(line: &str, ctx: &mut HqContext) -> Result<()> {
    // When state is AwaitingHuman, non-command input is treated as the human's answer.
    if !line.starts_with('/') {
        if ctx.has_pending_human_question().await? {
            return ctx.answer(line.to_string()).await;
        }
        anyhow::bail!("Commands must start with '/' — try /status, /task, /quit");
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
                        "  {name} ({status}) — tail logs: {}",
                        handle.log_path.display()
                    ));
                }
                ctx.display.info_block(lines);
            }
        }
        ShellCommand::Tasks => {
            let tasks = crate::project::list_tasks().await?;
            ctx.display
                .info_block(crate::runtime_table::task_lines(&tasks));
        }
        ShellCommand::Run { limit } => ctx.run_batch_plan(limit).await?,
        ShellCommand::Runs { limit } => {
            let runs = crate::project::list_runs(limit).await?;
            ctx.display
                .info_block(crate::runtime_table::run_lines(&runs));
        }
        ShellCommand::Events { limit, run_id } => {
            let events = crate::project::list_events(limit, run_id.clone()).await?;
            ctx.display.info_block(crate::runtime_table::event_lines(
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
                "  /spec              Draft, approve, and save a feature specification\n",
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
                "  /reset             Reset state to Idle (clears task files)\n",
                "  /init              Initialize ferrus in the current directory\n",
                "  /register          Register agent configs and permissions\n",
                "  /model <role> <model>\n",
                "                     Update the configured model override\n",
                "  /model <role> --clear\n",
                "                     Clear the configured model override\n",
                "  /quit              Exit HQ\n",
                "\n",
                "When an agent asks a question (state = AwaitingHuman):\n",
                "  Type your answer and press Enter (no slash prefix needed).",
            ));
        }
        ShellCommand::Reset => ctx.reset().await?,
        ShellCommand::Stop => ctx.stop().await?,
        ShellCommand::Plan => ctx.plan().await?,
        ShellCommand::Task { manual } => ctx.task(manual, true).await?,
        ShellCommand::Milestones => ctx.milestones().await?,
        ShellCommand::ResetSpec => ctx.reset_spec_selection().await?,
        ShellCommand::Spec => ctx.spec().await?,
        ShellCommand::Supervisor => ctx.supervisor_interactive().await?,
        ShellCommand::Executor => ctx.executor_interactive().await?,
        ShellCommand::Resume => ctx.resume().await?,
        ShellCommand::Review => ctx.review().await?,
        ShellCommand::Attach { name } => {
            if let Some(handle) = ctx.headless.get(&name) {
                let log = handle.log_path.display().to_string();
                ctx.display.info(format!(
                    "{name} runs headlessly — no terminal to attach.\n\
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
    /// Headless agent handles — executor and reviewer both run without a PTY.
    pub(crate) headless: std::collections::HashMap<String, agent_manager::HeadlessHandle>,
    debug: bool,
    state_rx: watch::Receiver<Option<WatchedState>>,
    pub(crate) display: Display,
    announced_completed_tasks: HashSet<String>,
}

impl HqContext {
    fn new(state_rx: watch::Receiver<Option<WatchedState>>, display: Display, debug: bool) -> Self {
        Self {
            supervisor: None,
            executor: None,
            headless: std::collections::HashMap::new(),
            debug,
            state_rx,
            display,
            announced_completed_tasks: HashSet::new(),
        }
    }

    async fn seed_completed_task_announcements(&mut self) -> Result<()> {
        let tasks = crate::project::list_tasks().await?;
        self.announced_completed_tasks
            .extend(completed_task_ids(&tasks));
        Ok(())
    }

    fn set_hq_config(&mut self, hq: &HqConfig) {
        self.supervisor = hq.supervisor_agent().ok();
        self.executor = hq.executor_agent().ok();
    }

    fn executor_agent_id(&self) -> Result<String> {
        self.executor_agent_id_for_index(DEFAULT_AGENT_INDEX)
    }

    fn executor_agent_id_for_index(&self, index: u32) -> Result<String> {
        let executor = self
            .executor
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Executor agent is not configured"))?;
        Ok(agent_id(ROLE_EXECUTOR, executor.name(), index))
    }

    fn supervisor_agent_id(&self) -> Result<String> {
        let supervisor = self
            .supervisor
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Supervisor agent is not configured"))?;
        Ok(agent_id(
            ROLE_SUPERVISOR,
            supervisor.name(),
            DEFAULT_AGENT_INDEX,
        ))
    }

    async fn ensure_hq_config(&mut self) -> Result<()> {
        if self.supervisor.is_some() && self.executor.is_some() {
            return Ok(());
        }

        let config = Config::load().await?;
        let hq = config.hq.ok_or_else(|| {
            anyhow::anyhow!(
                "No [hq.supervisor] / [hq.executor] sections in ferrus.toml. Add:\n[hq.supervisor]\nagent = \"claude-code\"\nmodel = \"\"\n\n[hq.executor]\nagent = \"codex\"\nmodel = \"\""
            )
        })?;
        self.set_hq_config(&hq);
        Ok(())
    }

    async fn reload_hq_config(&mut self) -> Result<()> {
        let config = Config::load().await?;
        let hq = config.hq.ok_or_else(|| {
            anyhow::anyhow!("No [hq.supervisor] / [hq.executor] sections in ferrus.toml.")
        })?;
        self.set_hq_config(&hq);
        Ok(())
    }

    async fn update_model(&mut self, target: ModelTarget, model: Option<&str>) -> Result<()> {
        self.ensure_hq_config().await?;
        update_hq_agent_config(target.config_role(), None, Some(model)).await?;
        self.reload_hq_config().await?;
        if let Some(model) = model {
            self.display.info(format!(
                "{} model set to \"{model}\"",
                target.display_name()
            ));
        } else {
            self.display
                .info(format!("{} model cleared", target.display_name()));
        }
        Ok(())
    }

    async fn mark_agent_running(
        &self,
        role: &str,
        agent_type: &str,
        name: &str,
        pid: Option<u32>,
    ) -> Result<()> {
        use agents::{AgentEntry, AgentStatus, read_agents, write_agents};

        let mut reg = read_agents().await?;
        reg.upsert(AgentEntry {
            role: role.to_string(),
            agent_type: agent_type.to_string(),
            name: name.to_string(),
            pid,
            status: AgentStatus::Running,
            started_at: Some(chrono::Utc::now()),
        });
        write_agents(&reg).await
    }

    async fn mark_agent_suspended(&self, name: &str) -> Result<()> {
        use agents::{AgentStatus, read_agents, write_agents};

        let mut reg = read_agents().await?;
        if let Some(entry) = reg.by_name_mut(name) {
            entry.pid = None;
            entry.status = AgentStatus::Suspended;
        }
        write_agents(&reg).await
    }

    async fn spawn_interactive_command(
        &mut self,
        role: &str,
        agent_type: &str,
        name: &str,
        command: std::process::Command,
    ) -> Result<()> {
        use std::process::Stdio;
        use tokio::process::Command;

        let mut cmd = Command::from(command);
        let ack_rx = self.display.suspend();
        let _ = ack_rx.await;
        let mut guard = ResumeGuard::new(self.display.clone());
        let program = cmd.as_std().get_program().to_string_lossy().into_owned();

        let mut child = cmd
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("Failed to spawn {program}"))?;
        let stderr = capture_interactive_stderr(&mut child);
        self.mark_agent_running(role, agent_type, name, child.id())
            .await?;

        let status = child
            .wait()
            .await
            .with_context(|| format!("Failed to wait for {program}"))?;
        let stderr = finish_interactive_stderr(stderr).await;
        clear_primary_screen();
        guard.resume_now();
        self.mark_agent_suspended(name).await?;
        if !status.success() {
            anyhow::bail!(interactive_exit_error(role, agent_type, status, &stderr));
        }
        Ok(())
    }

    async fn stop_interactive_child(
        &self,
        child: &mut tokio::process::Child,
        message: &str,
    ) -> Result<()> {
        self.display.muted(message);
        if tokio::time::timeout(std::time::Duration::from_millis(1500), child.wait())
            .await
            .is_ok()
        {
            return Ok(());
        }

        if let Some(pid) = child.id() {
            platform::signal_process(pid, platform::ShutdownSignal::Terminate);
        }
        if tokio::time::timeout(std::time::Duration::from_millis(800), child.wait())
            .await
            .is_ok()
        {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            return Ok(());
        }

        let _ = child.kill().await;
        let _ = child.wait().await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        Ok(())
    }

    async fn prepare_headless_slot(&mut self, name: &str) -> bool {
        let existing_is_alive = self
            .headless
            .get(name)
            .map(agent_manager::HeadlessHandle::is_alive);
        if existing_is_alive == Some(true) {
            self.display.info(format!("{name} is already running."));
            return false;
        }
        if existing_is_alive == Some(false) {
            self.reap_headless(name).await;
        }
        true
    }

    fn store_headless_handle(&mut self, name: &str, handle: agent_manager::HeadlessHandle) {
        self.display.muted(format!(
            "  • Started {name}…\n  ╰─ Logs: {}\n\n",
            handle.log_path.display()
        ));
        self.headless.insert(name.to_string(), handle);
    }

    async fn spawn_headless_supervisor_for_task(
        &mut self,
        name: &str,
        prompt: &str,
        task_id: &str,
    ) -> Result<()> {
        self.spawn_headless_supervisor_for_task_with_workspace(name, prompt, task_id, None)
            .await
    }

    async fn spawn_headless_supervisor_for_task_with_workspace(
        &mut self,
        name: &str,
        prompt: &str,
        task_id: &str,
        workspace: Option<agent_manager::HeadlessWorkspace>,
    ) -> Result<()> {
        if !self.prepare_headless_slot(name).await {
            return Ok(());
        }

        let agent = std::sync::Arc::clone(
            self.supervisor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Supervisor agent is not configured"))?,
        );
        let handle = agent_manager::spawn_headless_supervisor_with_env_and_workspace(
            agent.as_ref(),
            name,
            prompt,
            self.debug,
            vec![
                (ENV_AGENT_ID, name.to_string()),
                (ENV_TASK_ID, task_id.to_string()),
            ],
            workspace,
        )
        .await?;
        self.store_headless_handle(name, handle);
        Ok(())
    }

    async fn reconcile_runtime_schedule(&mut self) -> Result<()> {
        self.reap_exited_headless().await;

        let _ = crate::project::recover_runtime_state().await;
        let tasks = crate::project::list_tasks().await?;
        self.announce_completed_tasks(&tasks);
        if !tasks.iter().any(|task| {
            is_executor_ready_task_status(&task.status)
                || is_review_or_consultation_task_status(&task.status)
        }) {
            return Ok(());
        }

        self.ensure_hq_config().await?;
        let config = Config::load().await?;
        let max_parallel = config.limits.max_parallel_tasks.max(1);
        self.schedule_consultation_tasks(&tasks, max_parallel)
            .await?;
        self.schedule_reviewing_tasks(&tasks, max_parallel).await?;
        self.schedule_queued_tasks_from(tasks, max_parallel, false)
            .await?;
        Ok(())
    }

    fn announce_completed_tasks(&mut self, tasks: &[TaskRecord]) {
        for task_id in completed_task_ids(tasks) {
            if self.announced_completed_tasks.insert(task_id.clone()) {
                self.display.success(format!("Task {task_id} completed."));
            }
        }
    }

    async fn reap_exited_headless(&mut self) {
        let exited = self
            .headless
            .iter()
            .filter(|(_, handle)| !handle.is_alive())
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        for name in exited {
            self.reap_headless(&name).await;
        }
    }

    async fn spawn_headless_executor_for_task(
        &mut self,
        name: &str,
        prompt: &str,
        index: u32,
        task_id: &str,
    ) -> Result<()> {
        if !self.prepare_headless_slot(name).await {
            return Ok(());
        }

        let agent = std::sync::Arc::clone(
            self.executor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Executor agent is not configured"))?,
        );
        agent.validate_interactive_launch(ROLE_EXECUTOR, DEFAULT_AGENT_INDEX)?;
        let workspace = prepare_executor_workspace(task_id).await?;
        let mut env = vec![
            (ENV_AGENT_ID, name.to_string()),
            (ENV_TASK_ID, task_id.to_string()),
        ];
        if let Some(baseline_tree) = workspace.baseline_tree.as_deref() {
            env.push((ENV_BASELINE_TREE, baseline_tree.to_string()));
        }
        let handle = agent_manager::spawn_headless_executor_with_env(
            agent.as_ref(),
            name,
            prompt,
            index,
            self.debug,
            env,
            Some(agent_manager::HeadlessWorkspace {
                workspace_dir: workspace.workspace_dir.clone(),
                project_root: workspace.project_root.clone(),
            }),
        )
        .await?;
        self.store_headless_handle(name, handle);
        Ok(())
    }

    async fn resume(&mut self) -> Result<()> {
        let _ = crate::project::recover_runtime_state().await;
        let tasks = crate::project::list_tasks().await?;
        let has_runtime_work = tasks.iter().any(|task| {
            is_executor_ready_task_status(&task.status)
                || is_review_or_consultation_task_status(&task.status)
        });
        if has_runtime_work {
            self.ensure_hq_config().await?;
            let config = Config::load().await?;
            let max_parallel = config.limits.max_parallel_tasks.max(1);
            let consultation = self
                .schedule_consultation_tasks(&tasks, max_parallel)
                .await?;
            let reviewing = self.schedule_reviewing_tasks(&tasks, max_parallel).await?;
            let executor = self
                .schedule_queued_tasks_from(tasks, max_parallel, true)
                .await?;
            if consultation + reviewing + executor == 0 {
                self.display.info(
                    "No additional runtime task session started. Use /tasks to inspect work.",
                );
            }
            return Ok(());
        }

        self.display
            .info("No resumable SQLite task found. Use /task or /run to queue work.");
        Ok(())
    }

    async fn review(&mut self) -> Result<()> {
        let tasks = crate::project::list_tasks().await?;
        if tasks
            .iter()
            .any(|task| task.status == crate::project::TaskStatus::Reviewing.as_str())
        {
            self.ensure_hq_config().await?;
            let config = Config::load().await?;
            let max_parallel = config.limits.max_parallel_tasks.max(1);
            let spawned = self.schedule_reviewing_tasks(&tasks, max_parallel).await?;
            if spawned == 0 {
                self.display
                    .info("No reviewer session started. Reviewing task(s) may already be claimed.");
            }
            return Ok(());
        }

        anyhow::bail!("No SQLite reviewing task found. Use /status.")
    }

    async fn check(&mut self, force: bool) -> Result<()> {
        let _ = force;
        self.run_hq_checks_without_state().await?;
        Ok(())
    }

    async fn run_hq_checks_without_state(&self) -> Result<()> {
        let config = Config::load().await?;
        if config.checks.commands.is_empty() {
            self.display
                .info("Checks passed. Warning: no check commands are configured in ferrus.toml.");
            return Ok(());
        }

        let result = runner::run_checks(&config.checks.commands).await?;
        if result.passed {
            self.display
                .info("All configured checks passed. Task state was not modified.");
        } else {
            let failed = result
                .commands
                .iter()
                .filter(|cmd| !cmd.passed)
                .map(|cmd| format!("- `{}`", cmd.command))
                .collect::<Vec<_>>()
                .join("\n");
            self.display.error(format!(
                "HQ checks failed. Task state was not modified.\n\nFailed commands:\n{failed}"
            ));
        }
        Ok(())
    }

    async fn reset(&mut self) -> Result<()> {
        self.do_reset(true).await
    }

    async fn do_reset(&mut self, prompt: bool) -> Result<()> {
        let tasks = crate::project::list_tasks().await?;
        let resettable = tasks
            .iter()
            .filter(|task| is_resettable_task_status(&task.status))
            .cloned()
            .collect::<Vec<_>>();
        let running_agents = self
            .headless
            .values()
            .filter(|handle| handle.is_alive())
            .count();
        if prompt && (!resettable.is_empty() || running_agents > 0) {
            let reply_rx = self.display.confirm(format!(
                "Reset {task_count} non-terminal task(s) and stop {running_agents} running agent session(s)?",
                task_count = resettable.len()
            ));
            let confirmed = reply_rx.await.unwrap_or(false);
            if !confirmed {
                self.display.muted("Reset cancelled.");
                return Ok(());
            }
        }

        self.shutdown_all_headless().await;

        let mut reg = agents::read_agents().await?;
        for entry in &mut reg.agents {
            entry.pid = None;
            entry.status = agents::AgentStatus::Suspended;
        }
        agents::write_agents(&reg).await?;

        for task in &resettable {
            store::clear_scoped_task_artifacts(&task.path, &format!(".ferrus/runs/{}", task.id))
                .await?;
            crate::project::record_task_status_with_origin(
                &task.id,
                &task.path,
                crate::project::TaskStatus::Reset,
                task.spec_path.as_deref(),
                task.milestone_id.as_deref(),
            )
            .await?;
        }
        crate::project::record_runtime_event_best_effort(
            None,
            "hq_reset",
            serde_json::json!({
                "reset_task_count": resettable.len(),
                "stopped_agent_count": running_agents,
            }),
        )
        .await;

        if prompt {
            self.display.info(format!(
                "Runtime reset. {} non-terminal task(s) marked reset.",
                resettable.len()
            ));
        } else {
            tracing::debug!(reset_task_count = resettable.len(), "runtime reset");
        }
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        let reply_rx = self.display.confirm("Stop all running agents?");
        let confirmed = reply_rx.await.unwrap_or(false);
        if !confirmed {
            self.display.muted("Stop cancelled.");
            return Ok(());
        }

        self.shutdown_all_headless().await;

        let mut reg = agents::read_agents().await?;
        for entry in &mut reg.agents {
            entry.pid = None;
            entry.status = agents::AgentStatus::Suspended;
        }
        agents::write_agents(&reg).await?;

        self.display.muted("All agent sessions stopped.");
        Ok(())
    }

    async fn spawn_interactive_supervisor(
        &mut self,
        name: &str,
        prompt: Option<&str>,
    ) -> Result<()> {
        let agent = std::sync::Arc::clone(
            self.supervisor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Supervisor agent is not configured"))?,
        );
        agent.validate_interactive_launch(ROLE_SUPERVISOR, DEFAULT_AGENT_INDEX)?;
        self.spawn_interactive_command(
            ROLE_SUPERVISOR,
            agent.name(),
            name,
            agent
                .spawn(AgentRunMode::Interactive { prompt })
                .with_context(|| {
                    format!(
                        "Failed to resolve launcher for supervisor agent {}",
                        agent.name()
                    )
                })?,
        )
        .await
    }

    async fn spawn_interactive_supervisor_until_task_enqueued(
        &mut self,
        name: &str,
        prompt: &str,
        existing_task_ids: &HashSet<String>,
    ) -> Result<Option<String>> {
        Ok(self
            .spawn_interactive_supervisor_until_tasks_enqueued(
                name,
                prompt,
                existing_task_ids,
                1,
                "Task enqueued — returning to HQ…",
            )
            .await?
            .into_iter()
            .next())
    }

    async fn spawn_interactive_supervisor_until_tasks_enqueued(
        &mut self,
        name: &str,
        prompt: &str,
        existing_task_ids: &HashSet<String>,
        expected_count: usize,
        stop_message: &str,
    ) -> Result<Vec<String>> {
        use std::process::Stdio;
        use tokio::process::Command;

        let agent = std::sync::Arc::clone(
            self.supervisor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Supervisor agent is not configured"))?,
        );
        agent.validate_interactive_launch(ROLE_SUPERVISOR, DEFAULT_AGENT_INDEX)?;
        let mut cmd = Command::from(
            agent
                .spawn(AgentRunMode::Interactive {
                    prompt: Some(prompt),
                })
                .with_context(|| {
                    format!(
                        "Failed to resolve launcher for supervisor agent {}",
                        agent.name()
                    )
                })?,
        );

        let ack_rx = self.display.suspend();
        let _ = ack_rx.await;
        let mut guard = ResumeGuard::new(self.display.clone());
        let program = cmd.as_std().get_program().to_string_lossy().into_owned();

        let mut child = cmd
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("Failed to spawn {program}"))?;
        let stderr = capture_interactive_stderr(&mut child);
        self.mark_agent_running(ROLE_SUPERVISOR, agent.name(), name, child.id())
            .await?;

        let mut created_task_ids = Vec::new();
        let mut child_status = None;
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(300));
        loop {
            tokio::select! {
                status = child.wait() => {
                    child_status = Some(status.with_context(|| format!("Failed to wait for {program}"))?);
                    break;
                }
                _ = ticker.tick() => {
                    let task_ids = new_task_ids_since(existing_task_ids).await?;
                    if task_ids.len() >= expected_count {
                        created_task_ids = task_ids;
                        if let Some(status) = child
                            .try_wait()
                            .with_context(|| format!("Failed to inspect {program} status"))?
                        {
                            child_status = Some(status);
                            break;
                        }
                        self.stop_interactive_child(
                            &mut child,
                            stop_message,
                        )
                        .await?;
                        break;
                    }
                }
            }
        }

        let stderr = finish_interactive_stderr(stderr).await;
        clear_primary_screen();
        guard.resume_now();
        self.mark_agent_suspended(name).await?;
        if let Some(status) = child_status
            && !status.success()
        {
            anyhow::bail!(interactive_exit_error(
                ROLE_SUPERVISOR,
                agent.name(),
                status,
                &stderr
            ));
        }
        Ok(created_task_ids)
    }

    async fn spawn_interactive_executor(&mut self, name: &str, prompt: Option<&str>) -> Result<()> {
        let agent = std::sync::Arc::clone(
            self.executor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Executor agent is not configured"))?,
        );
        agent.validate_interactive_launch(ROLE_EXECUTOR, DEFAULT_AGENT_INDEX)?;
        self.spawn_interactive_command(
            ROLE_EXECUTOR,
            agent.name(),
            name,
            agent
                .spawn(AgentRunMode::Interactive { prompt })
                .with_context(|| {
                    format!(
                        "Failed to resolve launcher for executor agent {}",
                        agent.name()
                    )
                })?,
        )
        .await
    }

    async fn plan(&mut self) -> Result<()> {
        self.ensure_hq_config().await?;
        let agent = std::sync::Arc::clone(
            self.supervisor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Supervisor agent is not configured"))?,
        );

        self.display.info(format!(
            "Spawning supervisor ({}) for free-form planning…",
            agent.name()
        ));

        let supervisor_id = self.supervisor_agent_id()?;
        self.spawn_interactive_supervisor(
            &supervisor_id,
            Some(agent_manager::supervisor_plan_prompt()),
        )
        .await
    }

    async fn task(&mut self, manual: bool, confirm_selected_milestone: bool) -> Result<()> {
        self.ensure_hq_config().await?;

        let selection = crate::project::read_project_selection().await?;
        let selected = if manual {
            TaskMilestoneSelection::UseFallback
        } else {
            self.selected_milestone_for_task(&selection, confirm_selected_milestone)
                .await?
        };
        let selected = match selected {
            TaskMilestoneSelection::UseFallback => None,
            TaskMilestoneSelection::Use(selected) => Some(selected),
            TaskMilestoneSelection::Stop => return Ok(()),
        };

        let supervisor = std::sync::Arc::clone(
            self.supervisor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Supervisor agent is not configured"))?,
        );

        self.display
            .info(format!("Spawning supervisor ({})…", supervisor.name()));
        if selected.is_none() {
            self.display
                .info("Collaborate with the supervisor to define the task.");
        }

        let prompt = selected.as_ref().map(|selected| {
            agent_manager::supervisor_task_prompt_for_milestone(&selected_milestone_prompt_context(
                selected,
            ))
        });
        let prompt = match prompt.as_deref() {
            Some(prompt) => prompt,
            None => agent_manager::supervisor_task_prompt(),
        };

        let existing_task_ids = crate::project::list_tasks()
            .await?
            .into_iter()
            .map(|task| task.id)
            .collect::<HashSet<_>>();
        let supervisor_id = self.supervisor_agent_id()?;
        self.spawn_interactive_supervisor_until_task_enqueued(
            &supervisor_id,
            prompt,
            &existing_task_ids,
        )
        .await?;

        let scheduled = self.schedule_queued_tasks().await?;
        if scheduled == 0 {
            self.display
                .info("No queued task started. Use /tasks to inspect pending work.");
        }
        Ok(())
    }

    async fn run_batch_plan(&mut self, limit: Option<usize>) -> Result<()> {
        if limit == Some(0) {
            self.display.error("/run --limit must be greater than 0.");
            return Ok(());
        }

        let selection = crate::project::read_project_selection().await?;
        let Some(spec_path) = selection
            .selected_spec
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
        else {
            self.display
                .error("No selected spec. Run /milestones or /spec before /run.");
            return Ok(());
        };

        let plan = build_run_plan(spec_path).await?;
        if plan.eligible.is_empty() {
            self.display.info_block(run_plan_lines(&plan, 0));
            return Ok(());
        }

        let available = plan.eligible.len();
        let requested = limit.unwrap_or(available);
        let selected_count = requested.min(available);
        if let Some(limit) = limit
            && limit > available
        {
            self.display.info(format!(
                "/run --limit {limit} requested, but only {available} ready milestone(s) are eligible."
            ));
            let reply_rx = self
                .display
                .confirm_continue(format!("Proceed with {available}?"));
            if !reply_rx.await.unwrap_or(false) {
                self.display.muted("Run planning cancelled.");
                return Ok(());
            }
        }

        self.display
            .info_block(run_plan_lines(&plan, selected_count));
        self.launch_batch_task_supervisor(&plan, selected_count)
            .await?;
        Ok(())
    }

    async fn launch_batch_task_supervisor(
        &mut self,
        plan: &RunPlan,
        selected_count: usize,
    ) -> Result<()> {
        if selected_count == 0 {
            return Ok(());
        }

        self.ensure_hq_config().await?;
        let supervisor = std::sync::Arc::clone(
            self.supervisor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Supervisor agent is not configured"))?,
        );
        let context = run_plan_prompt_context(plan, selected_count);
        let prompt = agent_manager::supervisor_batch_task_prompt(&context, selected_count);

        self.display.info(format!(
            "Spawning supervisor ({}) for batch task preparation…",
            supervisor.name()
        ));
        self.display.tip(
            "Review each task draft with the supervisor; approved tasks will be queued as pending.",
        );

        let existing_task_ids = crate::project::list_tasks()
            .await?
            .into_iter()
            .map(|task| task.id)
            .collect::<HashSet<_>>();
        let supervisor_id = self.supervisor_agent_id()?;
        self.spawn_interactive_supervisor_until_tasks_enqueued(
            &supervisor_id,
            &prompt,
            &existing_task_ids,
            selected_count,
            "Batch tasks enqueued — returning to HQ…",
        )
        .await?;
        self.display
            .info("Batch preparation session finished. Use /tasks to inspect queued tasks.");
        self.schedule_queued_tasks().await?;
        Ok(())
    }

    async fn schedule_queued_tasks(&mut self) -> Result<usize> {
        self.ensure_hq_config().await?;
        let config = Config::load().await?;
        let max_parallel = config.limits.max_parallel_tasks.max(1);
        let tasks = crate::project::list_tasks().await?;
        self.schedule_queued_tasks_from(tasks, max_parallel, true)
            .await
    }

    async fn schedule_reviewing_tasks(
        &mut self,
        tasks: &[TaskRecord],
        max_parallel: usize,
    ) -> Result<usize> {
        let reviewing_count = tasks
            .iter()
            .filter(|task| task.status == crate::project::TaskStatus::Reviewing.as_str())
            .count();
        if reviewing_count == 0 {
            return Ok(0);
        }

        let running = self.running_supervisor_count();
        let slots = max_parallel.saturating_sub(running);
        if slots == 0 {
            return Ok(0);
        }

        let now = chrono::Utc::now();
        let prompt = agent_manager::reviewer_prompt();
        let mut spawned = 0usize;
        let mut spawn_error = None;
        let review_tasks = tasks
            .iter()
            .filter(|task| task.status == crate::project::TaskStatus::Reviewing.as_str())
            .filter(|task| !task_has_active_external_claim(task, now))
            .take(slots)
            .cloned()
            .collect::<Vec<_>>();

        for task in &review_tasks {
            let name = self.supervisor_agent_id_for_task(&task.id)?;
            if self
                .headless
                .get(&name)
                .is_some_and(agent_manager::HeadlessHandle::is_alive)
            {
                continue;
            }

            match self
                .spawn_headless_supervisor_for_task(&name, prompt, &task.id)
                .await
            {
                Ok(()) => {
                    spawned += 1;
                }
                Err(err) => {
                    spawn_error = Some(err);
                    break;
                }
            }
        }

        if let Some(err) = spawn_error {
            self.display.error(format!(
                "Could not start more reviewer sessions after starting {spawned} task(s): {err}",
            ));
        }
        Ok(spawned)
    }

    async fn schedule_consultation_tasks(
        &mut self,
        tasks: &[TaskRecord],
        max_parallel: usize,
    ) -> Result<usize> {
        let consultation_count = tasks
            .iter()
            .filter(|task| task.status == crate::project::TaskStatus::Consultation.as_str())
            .count();
        if consultation_count == 0 {
            return Ok(0);
        }

        let running = self.running_supervisor_count();
        let slots = max_parallel.saturating_sub(running);
        if slots == 0 {
            return Ok(0);
        }

        let prompt = agent_manager::consultant_prompt();
        let mut spawned = 0usize;
        let mut spawn_error = None;
        let consultation_tasks = tasks
            .iter()
            .filter(|task| task.status == crate::project::TaskStatus::Consultation.as_str())
            .take(slots)
            .cloned()
            .collect::<Vec<_>>();

        for task in &consultation_tasks {
            let name = self.supervisor_agent_id_for_task(&task.id)?;
            if self
                .headless
                .get(&name)
                .is_some_and(agent_manager::HeadlessHandle::is_alive)
            {
                continue;
            }

            let workspace = latest_executor_workspace_for_task(&task.id).await?;
            match self
                .spawn_headless_supervisor_for_task_with_workspace(
                    &name, prompt, &task.id, workspace,
                )
                .await
            {
                Ok(()) => {
                    spawned += 1;
                }
                Err(err) => {
                    spawn_error = Some(err);
                    break;
                }
            }
        }

        if let Some(err) = spawn_error {
            self.display.error(format!(
                "Could not start more consultation supervisor sessions after starting {spawned} task(s): {err}",
            ));
        }
        Ok(spawned)
    }

    async fn schedule_queued_tasks_from(
        &mut self,
        tasks: Vec<TaskRecord>,
        max_parallel: usize,
        report_waiting: bool,
    ) -> Result<usize> {
        let now = chrono::Utc::now();
        let ready_tasks = tasks
            .into_iter()
            .filter(|task| is_executor_ready_task_status(&task.status))
            .filter(|task| !task_has_active_external_claim(task, now))
            .collect::<Vec<_>>();
        let ready_count = ready_tasks.len();
        if ready_count == 0 {
            return Ok(0);
        }

        let running = self.running_executor_count();
        let slots = max_parallel.saturating_sub(running);
        if slots == 0 {
            if report_waiting {
                self.display.info(format!(
                    "{ready_count} executor-ready task(s) waiting; executor parallelism limit is {max_parallel}."
                ));
            }
            return Ok(0);
        }

        let mut spawned = 0usize;
        let mut spawn_error = None;
        let prompt = agent_manager::executor_prompt();
        let spawn_tasks = select_executor_spawn_tasks(&ready_tasks, slots, |task| {
            let Ok(name) = self.executor_agent_id_for_task(&task.id) else {
                return false;
            };
            self.headless
                .get(&name)
                .is_some_and(agent_manager::HeadlessHandle::is_alive)
        });

        for task in spawn_tasks {
            if spawned >= slots {
                break;
            }
            let index = u32::try_from(spawned + 1).context("Executor index exceeds u32 range")?;
            let name = self.executor_agent_id_for_task(&task.id)?;
            if self
                .headless
                .get(&name)
                .is_some_and(agent_manager::HeadlessHandle::is_alive)
            {
                continue;
            }

            match self
                .spawn_headless_executor_for_task(&name, prompt, index, &task.id)
                .await
            {
                Ok(()) => {
                    spawned += 1;
                }
                Err(err) => {
                    spawn_error = Some(err);
                    break;
                }
            }
        }

        if let Some(err) = spawn_error {
            self.display.error(format!(
                "Could not start more executor sessions after starting {spawned} task(s): {err}",
            ));
        }
        Ok(spawned)
    }

    fn running_executor_count(&self) -> usize {
        self.headless
            .iter()
            .filter(|(name, handle)| name.starts_with(ROLE_EXECUTOR) && handle.is_alive())
            .count()
    }

    fn running_supervisor_count(&self) -> usize {
        self.headless
            .iter()
            .filter(|(name, handle)| name.starts_with(ROLE_SUPERVISOR) && handle.is_alive())
            .count()
    }

    fn executor_agent_id_for_task(&self, task_id: &str) -> Result<String> {
        let executor = self
            .executor
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Executor agent is not configured"))?;
        Ok(format!("{}:{}:{}", ROLE_EXECUTOR, executor.name(), task_id))
    }

    fn supervisor_agent_id_for_task(&self, task_id: &str) -> Result<String> {
        let supervisor = self
            .supervisor
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Supervisor agent is not configured"))?;
        Ok(format!(
            "{}:{}:{}",
            ROLE_SUPERVISOR,
            supervisor.name(),
            task_id
        ))
    }

    async fn selected_milestone_for_task(
        &self,
        selection: &ProjectSelection,
        confirm: bool,
    ) -> Result<TaskMilestoneSelection> {
        let Some(spec_path) = selection
            .selected_spec
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
        else {
            return Ok(TaskMilestoneSelection::UseFallback);
        };
        if !Path::new(spec_path).exists() {
            self.display.error(format!(
                "Selected spec no longer exists:\n{spec_path}\n\nRun /milestones to select a valid spec."
            ));
            return Ok(TaskMilestoneSelection::Stop);
        }

        let plan = build_run_plan(spec_path).await?;
        let Some(next) = plan.eligible.first() else {
            self.display.info_block(run_plan_lines(&plan, 0));
            self.display
                .muted("No ready milestone is available. Use /task --manual for an ad-hoc task.");
            return Ok(TaskMilestoneSelection::Stop);
        };
        let spec = specs::load_spec(&plan.spec_path).await?;
        let milestone = spec
            .milestones
            .iter()
            .find(|milestone| milestone.id == next.id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Ready milestone {} disappeared", next.id))?;
        let selected = SelectedMilestone {
            spec_path: spec.path.clone(),
            spec_display: specs::spec_display_name(&spec.path),
            milestone,
        };

        if !confirm {
            return Ok(TaskMilestoneSelection::Use(selected));
        }
        self.display.muted(format!(
            "\n  • Using next ready milestone\n  ╰─ {} / {}\n",
            selected.spec_path,
            selected.milestone.display_title()
        ));
        let reply_rx = self.display.confirm_yes("Proceed?");
        if reply_rx.await.unwrap_or(true) {
            Ok(TaskMilestoneSelection::Use(selected))
        } else {
            self.display.muted("Task cancelled.");
            Ok(TaskMilestoneSelection::Stop)
        }
    }

    async fn reset_spec_selection(&mut self) -> Result<()> {
        let selection = crate::project::read_project_selection().await?;
        if selection.selected_spec.is_none() {
            self.display.muted("No selected spec to reset.");
            return Ok(());
        }

        crate::project::write_project_selection(&crate::project::ProjectSelection::default())
            .await?;

        self.display
            .muted("Selected spec reset. /task will use manual task definition.");
        Ok(())
    }

    async fn milestones(&mut self) -> Result<()> {
        let specs = specs::list_spec_paths().await?;
        if specs.is_empty() {
            self.display
                .error("No specs found in the configured spec directory.");
            return Ok(());
        }

        let options = specs
            .iter()
            .map(|path| format!("{}  ({path})", specs::spec_display_name(path)))
            .collect();
        let Some(spec_idx) = self
            .display
            .select("Select spec:", options)
            .await
            .unwrap_or(None)
        else {
            self.display.muted("Milestone selection cancelled.");
            return Ok(());
        };

        let spec = specs::load_spec(&specs[spec_idx]).await?;
        if spec.milestones.is_empty() {
            self.display
                .error("Selected spec has no milestones with IDs.");
            return Ok(());
        }

        crate::project::write_project_selection(&crate::project::ProjectSelection {
            selected_spec: Some(spec.path.clone()),
        })
        .await?;

        self.display
            .muted(format!("\n  • Selected spec\n  ╰─ {}\n", spec.path));

        let reply_rx = self
            .display
            .confirm("Create task from the next ready milestone now?");
        if reply_rx.await.unwrap_or(false) {
            self.task(false, false).await?;
        }
        Ok(())
    }

    async fn spec(&mut self) -> Result<()> {
        use std::process::Stdio;
        use tokio::process::Command;

        self.ensure_hq_config().await?;
        prepare_spec_session_files().await?;

        let supervisor = std::sync::Arc::clone(
            self.supervisor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Supervisor agent is not configured"))?,
        );

        self.display.info(format!(
            "Spawning supervisor ({}) for specification drafting…",
            supervisor.name()
        ));
        self.display
            .info("Collaborate with the supervisor to draft and approve the specification.");

        let mut cmd = Command::from(
            supervisor
                .spawn(AgentRunMode::Interactive {
                    prompt: Some(agent_manager::supervisor_spec_prompt()),
                })
                .with_context(|| {
                    format!(
                        "Failed to resolve launcher for supervisor agent {}",
                        supervisor.name()
                    )
                })?,
        );

        supervisor.validate_interactive_launch(ROLE_SUPERVISOR, DEFAULT_AGENT_INDEX)?;
        let ack_rx = self.display.suspend();
        let _ = ack_rx.await;
        let mut resume_guard = ResumeGuard::new(self.display.clone());
        let program = cmd.as_std().get_program().to_string_lossy().into_owned();
        let mut child = cmd
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("Failed to spawn {program}"))?;
        let stderr = capture_interactive_stderr(&mut child);
        let supervisor_id = self.supervisor_agent_id()?;
        self.mark_agent_running(
            ROLE_SUPERVISOR,
            supervisor.name(),
            &supervisor_id,
            child.id(),
        )
        .await?;

        let mut created_path = None;
        let mut child_status = None;
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(300));
        loop {
            tokio::select! {
                status = child.wait() => {
                    child_status = Some(status.with_context(|| format!("Failed to wait for {program}"))?);
                    break;
                }
                _ = ticker.tick() => {
                    if let Ok(Some(path)) = crate::project::read_last_spec_path().await
                        && !path.is_empty()
                    {
                        created_path = Some(path);
                        self.stop_interactive_child(
                            &mut child,
                            "Spec created — waiting for supervisor to exit…",
                        )
                        .await?;
                        break;
                    }
                }
            }
        }
        let stderr = finish_interactive_stderr(stderr).await;
        clear_primary_screen();
        resume_guard.resume_now();

        self.mark_agent_suspended(&supervisor_id).await?;
        if let Some(status) = child_status
            && !status.success()
        {
            anyhow::bail!(interactive_exit_error(
                ROLE_SUPERVISOR,
                supervisor.name(),
                status,
                &stderr
            ));
        }

        if created_path.is_none()
            && let Ok(Some(path)) = crate::project::read_last_spec_path().await
            && !path.is_empty()
        {
            created_path = Some(path);
        }

        if let Some(path) = created_path {
            self.display
                .muted(format!("\n  • Specification ready\n  ╰─ {path}\n"));
            self.display
                .tip("Tip: Use /task to queue the next ready milestone.");
        } else {
            self.display
                .info("No specification created. Re-run /spec when ready.");
        }
        Ok(())
    }

    async fn supervisor_interactive(&mut self) -> Result<()> {
        self.ensure_hq_config().await?;
        let agent = std::sync::Arc::clone(
            self.supervisor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Supervisor agent is not configured"))?,
        );

        self.display.info(format!(
            "Spawning supervisor ({}) interactively…",
            agent.name()
        ));

        let supervisor_id = self.supervisor_agent_id()?;
        self.spawn_interactive_supervisor(&supervisor_id, None)
            .await
    }

    async fn executor_interactive(&mut self) -> Result<()> {
        self.ensure_hq_config().await?;
        let agent = std::sync::Arc::clone(
            self.executor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Executor agent is not configured"))?,
        );

        self.display.info(format!(
            "Spawning executor ({}) interactively…",
            agent.name()
        ));

        let executor_id = self.executor_agent_id()?;
        self.spawn_interactive_executor(&executor_id, None).await
    }

    /// Handle a raw-text answer from the user when state is AwaitingHuman.
    async fn answer(&mut self, response: String) -> Result<()> {
        if response.trim().is_empty() {
            anyhow::bail!("Answer cannot be empty.");
        }
        if self.has_scoped_human_question().await? {
            return self.answer_scoped_human_question(response).await;
        }

        anyhow::bail!("No task is currently waiting for a human answer.")
    }

    async fn has_pending_human_question(&self) -> Result<bool> {
        if self.has_scoped_human_question().await? {
            return Ok(true);
        }
        Ok(false)
    }

    async fn has_scoped_human_question(&self) -> Result<bool> {
        match crate::project::list_human_questions().await {
            Ok(questions) => Ok(!questions.is_empty()),
            Err(err) => {
                tracing::debug!(error = ?err, "failed to list scoped human questions");
                Ok(false)
            }
        }
    }

    async fn answer_scoped_human_question(&mut self, response: String) -> Result<()> {
        let Some(question) = crate::project::list_human_questions()
            .await?
            .into_iter()
            .next()
        else {
            anyhow::bail!("No task is currently waiting for a human answer.");
        };

        store::write_answer_for_run_dir(&question.run_dir, &response).await?;
        self.display
            .info(format!("Answer recorded for {}.", question.task_id));

        let owner = crate::project::task_human_question_owner(&question.task_id).await?;
        let agent_alive = owner
            .as_deref()
            .and_then(|agent_id| self.headless.get(agent_id))
            .is_some_and(agent_manager::HeadlessHandle::is_alive);

        if agent_alive {
            let owner = owner.as_deref().unwrap_or("agent");
            self.display.info(format!(
                "Waiting for {owner} to receive it via /wait_for_answer…"
            ));
            return Ok(());
        }

        let Some(owner) = owner else {
            self.display
                .info("No recorded answer waiter found. Use /tasks to inspect the awaiting task.");
            return Ok(());
        };

        self.relaunch_human_answer_waiter(&owner, &question.task_id)
            .await?;
        self.display.info(format!(
            "Relaunched {owner} to receive the answer via /wait_for_answer."
        ));
        Ok(())
    }

    async fn relaunch_human_answer_waiter(&mut self, owner: &str, task_id: &str) -> Result<()> {
        if owner.starts_with(ROLE_EXECUTOR) {
            return self
                .spawn_headless_executor_for_task(
                    owner,
                    agent_manager::executor_wait_for_answer_prompt(),
                    DEFAULT_AGENT_INDEX,
                    task_id,
                )
                .await;
        }

        if owner.starts_with(ROLE_SUPERVISOR) {
            let workspace = latest_executor_workspace_for_task(task_id).await?;
            return self
                .spawn_headless_supervisor_for_task_with_workspace(
                    owner,
                    agent_manager::supervisor_wait_for_answer_prompt(),
                    task_id,
                    workspace,
                )
                .await;
        }

        anyhow::bail!("Cannot relaunch unknown human answer waiter {owner}");
    }

    async fn reap_headless(&mut self, name: &str) {
        if let Some(handle) = self.headless.remove(name) {
            handle.reap().await;
        }
    }

    async fn shutdown_all_headless(&mut self) {
        let handles: Vec<_> = self.headless.drain().map(|(_, handle)| handle).collect();
        for handle in handles {
            handle.terminate().await;
        }
    }
}

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

#[derive(Debug, Clone)]
struct ExecutorWorkspace {
    project_root: PathBuf,
    workspace_dir: PathBuf,
    baseline_tree: Option<String>,
}

async fn prepare_executor_workspace(task_id: &str) -> Result<ExecutorWorkspace> {
    let registration = crate::project::touch_current_project().await?;
    let project_root = PathBuf::from(&registration.metadata.workspace_dir);
    if !git_is_work_tree(&project_root).await {
        anyhow::bail!(
            "Cannot start isolated executor workspace: {} is not a git worktree.",
            project_root.display()
        );
    }

    let workspace_dir = registration.data_dir.join("worktrees").join(task_id);
    let baseline_path = executor_workspace_baseline_path(&registration.data_dir, task_id);
    if tokio::fs::try_exists(&workspace_dir).await? {
        if git_is_work_tree(&workspace_dir).await {
            copy_canonical_agent_config_files(&project_root, &workspace_dir).await?;
            let mut baseline_tree = read_executor_workspace_baseline_tree(&baseline_path).await?;
            if baseline_tree.is_none() && !git_has_head(&workspace_dir).await {
                let captured = capture_executor_workspace_baseline_tree(&workspace_dir).await?;
                write_executor_workspace_baseline_tree(&baseline_path, &captured).await?;
                baseline_tree = Some(captured);
            }
            return Ok(ExecutorWorkspace {
                project_root,
                workspace_dir,
                baseline_tree,
            });
        }
        anyhow::bail!(
            "Cannot start isolated executor workspace: {} already exists and is not a git worktree.",
            workspace_dir.display()
        );
    }

    let parent = workspace_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("workspace path has no parent"))?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("Failed to create {}", parent.display()))?;

    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(&project_root)
        .args(["worktree", "add"]);
    if git_has_head(&project_root).await {
        command.args(["--detach"]).arg(&workspace_dir).arg("HEAD");
    } else {
        command.arg("--orphan").arg(&workspace_dir);
    }
    let output = command
        .output()
        .await
        .context("Failed to run git worktree add")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "Failed to create isolated executor workspace at {}: {}",
            workspace_dir.display(),
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        );
    }
    seed_executor_workspace_from_canonical_changes(&project_root, &workspace_dir).await?;
    let baseline_tree = capture_executor_workspace_baseline_tree(&workspace_dir).await?;
    write_executor_workspace_baseline_tree(&baseline_path, &baseline_tree).await?;

    Ok(ExecutorWorkspace {
        project_root,
        workspace_dir,
        baseline_tree: Some(baseline_tree),
    })
}

async fn seed_executor_workspace_from_canonical_changes(
    project_root: &Path,
    workspace_dir: &Path,
) -> Result<()> {
    if git_has_head(project_root).await {
        apply_canonical_tracked_diff(project_root, workspace_dir).await?;
    }
    copy_canonical_untracked_files(project_root, workspace_dir).await?;
    copy_canonical_agent_config_files(project_root, workspace_dir).await
}

fn executor_workspace_baseline_path(data_dir: &Path, task_id: &str) -> PathBuf {
    data_dir
        .join("worktrees")
        .join(".baseline-trees")
        .join(format!("{task_id}.txt"))
}

async fn read_executor_workspace_baseline_tree(path: &Path) -> Result<Option<String>> {
    if !tokio::fs::try_exists(path).await? {
        return Ok(None);
    }
    let value = tokio::fs::read_to_string(path).await.with_context(|| {
        format!(
            "Failed to read executor workspace baseline {}",
            path.display()
        )
    })?;
    Ok(Some(value.trim().to_string()).filter(|value| !value.is_empty()))
}

async fn write_executor_workspace_baseline_tree(path: &Path, baseline_tree: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    tokio::fs::write(path, format!("{baseline_tree}\n"))
        .await
        .with_context(|| {
            format!(
                "Failed to write executor workspace baseline {}",
                path.display()
            )
        })
}

async fn capture_executor_workspace_baseline_tree(workspace_dir: &Path) -> Result<String> {
    let add = Command::new("git")
        .arg("-C")
        .arg(workspace_dir)
        .args(["add", "-A", "."])
        .output()
        .await
        .context("Failed to stage executor workspace baseline")?;
    if !add.status.success() {
        let stderr = String::from_utf8_lossy(&add.stderr).trim().to_string();
        anyhow::bail!(
            "Failed to stage executor workspace baseline at {}: {}",
            workspace_dir.display(),
            if stderr.is_empty() {
                add.status.to_string()
            } else {
                stderr
            }
        );
    }

    let tree = Command::new("git")
        .arg("-C")
        .arg(workspace_dir)
        .arg("write-tree")
        .output()
        .await
        .context("Failed to capture executor workspace baseline tree")?;
    if !tree.status.success() {
        let stderr = String::from_utf8_lossy(&tree.stderr).trim().to_string();
        anyhow::bail!(
            "Failed to capture executor workspace baseline tree at {}: {}",
            workspace_dir.display(),
            if stderr.is_empty() {
                tree.status.to_string()
            } else {
                stderr
            }
        );
    }

    let mut reset_index = Command::new("git");
    reset_index.arg("-C").arg(workspace_dir).arg("read-tree");
    if git_has_head(workspace_dir).await {
        reset_index.arg("HEAD");
    } else {
        reset_index.arg("--empty");
    }
    let reset_index = reset_index
        .output()
        .await
        .context("Failed to restore executor workspace index")?;
    if !reset_index.status.success() {
        let stderr = String::from_utf8_lossy(&reset_index.stderr)
            .trim()
            .to_string();
        anyhow::bail!(
            "Failed to restore executor workspace index at {}: {}",
            workspace_dir.display(),
            if stderr.is_empty() {
                reset_index.status.to_string()
            } else {
                stderr
            }
        );
    }

    Ok(String::from_utf8_lossy(&tree.stdout).trim().to_string())
}

async fn apply_canonical_tracked_diff(project_root: &Path, workspace_dir: &Path) -> Result<()> {
    let diff = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["diff", "--binary", "HEAD", "--"])
        .output()
        .await
        .context("Failed to capture canonical workspace diff")?;
    if !diff.status.success() {
        let stderr = String::from_utf8_lossy(&diff.stderr).trim().to_string();
        anyhow::bail!(
            "Failed to capture canonical workspace diff from {}: {}",
            project_root.display(),
            if stderr.is_empty() {
                diff.status.to_string()
            } else {
                stderr
            }
        );
    }
    if diff.stdout.is_empty() {
        return Ok(());
    }

    let mut apply = Command::new("git")
        .arg("-C")
        .arg(workspace_dir)
        .args(["apply", "--whitespace=nowarn"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "Failed to start git apply for executor workspace {}",
                workspace_dir.display()
            )
        })?;
    if let Some(mut stdin) = apply.stdin.take() {
        use tokio::io::AsyncWriteExt;

        stdin
            .write_all(&diff.stdout)
            .await
            .context("Failed to stream canonical workspace diff")?;
    }
    let output = apply
        .wait_with_output()
        .await
        .context("Failed to apply canonical workspace diff")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "Failed to seed executor workspace {} with approved canonical changes: {}",
            workspace_dir.display(),
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        );
    }
    Ok(())
}

async fn copy_canonical_untracked_files(project_root: &Path, workspace_dir: &Path) -> Result<()> {
    let workspace_dir = tokio::fs::canonicalize(workspace_dir)
        .await
        .unwrap_or_else(|_| workspace_dir.to_path_buf());
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .output()
        .await
        .context("Failed to list canonical untracked files")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "Failed to list canonical untracked files from {}: {}",
            project_root.display(),
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        );
    }

    for relative in output.stdout.split(|byte| *byte == 0) {
        if relative.is_empty() {
            continue;
        }
        let relative = PathBuf::from(String::from_utf8_lossy(relative).into_owned());
        let source = project_root.join(&relative);
        let canonical_source = tokio::fs::canonicalize(&source)
            .await
            .unwrap_or_else(|_| source.clone());
        if canonical_source.starts_with(&workspace_dir) {
            continue;
        }
        let metadata = tokio::fs::symlink_metadata(&source)
            .await
            .with_context(|| format!("Failed to stat {}", source.display()))?;
        if metadata.is_dir() {
            continue;
        }
        let destination = workspace_dir.join(&relative);
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        tokio::fs::copy(&source, &destination)
            .await
            .with_context(|| {
                format!(
                    "Failed to copy canonical untracked file {} to {}",
                    source.display(),
                    destination.display()
                )
            })?;
    }
    Ok(())
}

async fn copy_canonical_agent_config_files(
    project_root: &Path,
    workspace_dir: &Path,
) -> Result<()> {
    for relative in [
        ".ferrus/project.toml",
        ".claude/mcp-supervisor.json",
        ".claude/mcp-executor.json",
        ".claude/settings.local.json",
        ".codex/config.toml",
        ".qwen/settings.json",
    ] {
        copy_canonical_file_if_present(project_root, workspace_dir, Path::new(relative)).await?;
    }
    Ok(())
}

async fn copy_canonical_file_if_present(
    project_root: &Path,
    workspace_dir: &Path,
    relative: &Path,
) -> Result<()> {
    let source = project_root.join(relative);
    let Ok(metadata) = tokio::fs::symlink_metadata(&source).await else {
        return Ok(());
    };
    if metadata.is_dir() {
        return Ok(());
    }

    let destination = workspace_dir.join(relative);
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    tokio::fs::copy(&source, &destination)
        .await
        .with_context(|| {
            format!(
                "Failed to copy canonical agent config {} to {}",
                source.display(),
                destination.display()
            )
        })?;
    Ok(())
}

async fn git_is_work_tree(path: &Path) -> bool {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .await;
    matches!(output, Ok(output) if output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true")
}

async fn git_has_head(path: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .await
        .is_ok_and(|output| output.status.success())
}

fn task_has_active_external_claim(task: &TaskRecord, now: chrono::DateTime<chrono::Utc>) -> bool {
    if task.claimed_by.is_none() {
        return false;
    }
    task.lease_until
        .as_deref()
        .and_then(|lease_until| chrono::DateTime::parse_from_rfc3339(lease_until).ok())
        .is_some_and(|lease_until| lease_until.with_timezone(&chrono::Utc) > now)
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
mod tests {
    use super::*;

    fn task_record(id: &str, status: crate::project::TaskStatus) -> TaskRecord {
        TaskRecord {
            id: id.to_string(),
            path: format!(".ferrus/tasks/{id}.md"),
            spec_path: None,
            milestone_id: None,
            status: status.as_str().to_string(),
            paused_status: None,
            claimed_by: None,
            lease_until: None,
            last_heartbeat: None,
            check_retries: 0,
            review_cycles: 0,
            failure_reason: None,
        }
    }

    #[cfg(unix)]
    fn failed_exit_status() -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;

        std::process::ExitStatus::from_raw(1 << 8)
    }

    #[cfg(windows)]
    fn failed_exit_status() -> std::process::ExitStatus {
        use std::os::windows::process::ExitStatusExt;

        std::process::ExitStatus::from_raw(1)
    }

    #[test]
    fn interactive_exit_error_names_role_agent_and_status() {
        let message = interactive_exit_error(
            ROLE_SUPERVISOR,
            "codex",
            failed_exit_status(),
            "broken config",
        );

        assert!(message.contains("supervisor agent (codex) exited with"));
        assert!(message.contains("stderr:\nbroken config"));
    }

    #[tokio::test]
    async fn run_plan_selects_ready_milestones_and_skips_existing_tasks() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
        std::fs::create_dir_all(dir.path().join("docs/specs")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let data_dir = dir.path().join("runtime");
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        let local_ref = crate::project::LocalProjectRef {
            project_id: "test-project".to_string(),
            name: "test".to_string(),
            data_dir: data_dir.to_string_lossy().into_owned(),
        };
        let local_ref_toml = toml::to_string_pretty(&local_ref).unwrap();
        tokio::fs::write(".ferrus/project.toml", local_ref_toml)
            .await
            .unwrap();
        let spec_path = "docs/specs/spec.md";
        tokio::fs::write(
            spec_path,
            "## Milestones\n\
             - [x] #1.0 Foundation\n\
               - ID: m1.0\n\
               - Depends on: none\n\n\
             - [ ] #1.1 Ready one\n\
               - ID: m1.1\n\
               - Depends on: m1.0\n\n\
             - [ ] #1.2 Already queued\n\
               - ID: m1.2\n\
               - Depends on: m1.0\n\n\
             - [ ] #2.0 Blocked\n\
               - ID: m2.0\n\
               - Depends on: m1.1\n",
        )
        .await
        .unwrap();
        crate::project::record_task_status_with_origin(
            "t-002",
            ".ferrus/tasks/t-002.md",
            crate::project::TaskStatus::Pending,
            Some(spec_path),
            Some("m1.2"),
        )
        .await
        .unwrap();

        let plan = build_run_plan(spec_path).await.unwrap();

        assert_eq!(plan.eligible.len(), 1);
        assert_eq!(plan.eligible[0].id, "m1.1");
        assert!(plan.skipped.iter().any(|milestone| {
            milestone.id == "m1.2" && milestone.reason == "task t-002 is pending"
        }));
        assert!(
            plan.skipped
                .iter()
                .any(|milestone| milestone.id == "m2.0" && milestone.reason == "waiting for m1.1")
        );

        std::env::set_current_dir(previous).unwrap();
    }

    #[tokio::test]
    async fn executor_workspace_includes_uncommitted_canonical_changes() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        std::process::Command::new("git")
            .args(["init"])
            .status()
            .unwrap();
        tokio::fs::write(".gitignore", ".ferrus/\n").await.unwrap();
        tokio::fs::write("tracked.txt", "base\n").await.unwrap();
        std::process::Command::new("git")
            .args(["add", ".gitignore", "tracked.txt"])
            .status()
            .unwrap();
        let commit_status = std::process::Command::new("git")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Test User",
                "commit",
                "-m",
                "initial",
            ])
            .status()
            .unwrap();
        assert!(commit_status.success());

        let data_dir = dir.path().join("runtime");
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        tokio::fs::create_dir_all(".ferrus").await.unwrap();
        let local_ref = crate::project::LocalProjectRef {
            project_id: "test-project".to_string(),
            name: "test".to_string(),
            data_dir: data_dir.to_string_lossy().into_owned(),
        };
        tokio::fs::write(
            ".ferrus/project.toml",
            toml::to_string_pretty(&local_ref).unwrap(),
        )
        .await
        .unwrap();
        let metadata = crate::project::ProjectMetadata {
            id: "test-project".to_string(),
            name: "test".to_string(),
            workspace_dir: dir.path().to_string_lossy().into_owned(),
            ferrus_dir: dir.path().join(".ferrus").to_string_lossy().into_owned(),
            vcs: Some("git".to_string()),
            origin_repo: None,
            default_branch: None,
            current_head: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            last_opened_at: "2026-01-01T00:00:00Z".to_string(),
            version: 1,
        };
        tokio::fs::write(
            data_dir.join("project.toml"),
            toml::to_string_pretty(&metadata).unwrap(),
        )
        .await
        .unwrap();

        tokio::fs::write("tracked.txt", "base\napproved\n")
            .await
            .unwrap();
        tokio::fs::write("new.txt", "new approved file\n")
            .await
            .unwrap();

        let workspace = prepare_executor_workspace("t-001").await.unwrap();

        let tracked_content =
            tokio::fs::read_to_string(workspace.workspace_dir.join("tracked.txt"))
                .await
                .unwrap()
                .replace("\r\n", "\n");
        assert_eq!(tracked_content, "base\napproved\n");
        assert_eq!(
            tokio::fs::read_to_string(workspace.workspace_dir.join("new.txt"))
                .await
                .unwrap(),
            "new approved file\n"
        );
        assert!(
            !workspace
                .workspace_dir
                .join("runtime/worktrees/t-001/tracked.txt")
                .exists()
        );

        std::env::set_current_dir(previous).unwrap();
    }

    #[tokio::test]
    async fn executor_workspace_starts_from_unborn_git_project() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        std::process::Command::new("git")
            .args(["init"])
            .status()
            .unwrap();
        tokio::fs::write(
            ".gitignore",
            ".ferrus/\n.codex/config.toml\n.claude/mcp-executor.json\n",
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(".codex").await.unwrap();
        tokio::fs::write(
            ".codex/config.toml",
            "[mcp_servers.ferrus-executor]\ncommand = \"ferrus\"\n",
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(".claude").await.unwrap();
        tokio::fs::write(
            ".claude/mcp-executor.json",
            "{\"mcpServers\":{\"ferrus-executor\":{\"command\":\"ferrus\",\"args\":[]}}}",
        )
        .await
        .unwrap();
        tokio::fs::write("seed.txt", "uncommitted project file\n")
            .await
            .unwrap();

        let data_dir = dir.path().join("runtime");
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        tokio::fs::create_dir_all(".ferrus").await.unwrap();
        let local_ref = crate::project::LocalProjectRef {
            project_id: "test-project".to_string(),
            name: "test".to_string(),
            data_dir: data_dir.to_string_lossy().into_owned(),
        };
        tokio::fs::write(
            ".ferrus/project.toml",
            toml::to_string_pretty(&local_ref).unwrap(),
        )
        .await
        .unwrap();
        let metadata = crate::project::ProjectMetadata {
            id: "test-project".to_string(),
            name: "test".to_string(),
            workspace_dir: dir.path().to_string_lossy().into_owned(),
            ferrus_dir: dir.path().join(".ferrus").to_string_lossy().into_owned(),
            vcs: Some("git".to_string()),
            origin_repo: None,
            default_branch: None,
            current_head: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            last_opened_at: "2026-01-01T00:00:00Z".to_string(),
            version: 1,
        };
        tokio::fs::write(
            data_dir.join("project.toml"),
            toml::to_string_pretty(&metadata).unwrap(),
        )
        .await
        .unwrap();

        let workspace = prepare_executor_workspace("t-unborn").await.unwrap();

        assert!(workspace.baseline_tree.is_some());
        assert_eq!(
            tokio::fs::read_to_string(workspace.workspace_dir.join("seed.txt"))
                .await
                .unwrap(),
            "uncommitted project file\n"
        );
        assert!(
            !workspace
                .workspace_dir
                .join("runtime/worktrees/t-unborn/seed.txt")
                .exists()
        );
        assert_eq!(
            tokio::fs::read_to_string(workspace.workspace_dir.join(".codex/config.toml"))
                .await
                .unwrap(),
            "[mcp_servers.ferrus-executor]\ncommand = \"ferrus\"\n"
        );
        assert_eq!(
            tokio::fs::read_to_string(workspace.workspace_dir.join(".claude/mcp-executor.json"))
                .await
                .unwrap(),
            "{\"mcpServers\":{\"ferrus-executor\":{\"command\":\"ferrus\",\"args\":[]}}}"
        );
        assert_eq!(
            tokio::fs::read_to_string(workspace.workspace_dir.join(".ferrus/project.toml"))
                .await
                .unwrap(),
            toml::to_string_pretty(&local_ref).unwrap()
        );
        let baseline_tree = workspace.baseline_tree.clone();
        assert_eq!(
            tokio::fs::read_to_string(executor_workspace_baseline_path(&data_dir, "t-unborn"))
                .await
                .unwrap()
                .trim(),
            baseline_tree.as_deref().unwrap()
        );

        let resumed_workspace = prepare_executor_workspace("t-unborn").await.unwrap();
        assert_eq!(resumed_workspace.baseline_tree, baseline_tree);

        std::env::set_current_dir(previous).unwrap();
    }

    #[tokio::test]
    async fn reset_marks_non_terminal_database_tasks_without_state_json() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let data_dir = dir.path().join("runtime");
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        let local_ref = crate::project::LocalProjectRef {
            project_id: "test-project".to_string(),
            name: "test".to_string(),
            data_dir: data_dir.to_string_lossy().into_owned(),
        };
        tokio::fs::write(
            ".ferrus/project.toml",
            toml::to_string_pretty(&local_ref).unwrap(),
        )
        .await
        .unwrap();
        crate::project::record_task_status(
            "t-001",
            ".ferrus/tasks/t-001.md",
            crate::project::TaskStatus::Pending,
        )
        .await
        .unwrap();
        crate::project::record_task_status(
            "t-002",
            ".ferrus/tasks/t-002.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        crate::project::record_task_status(
            "t-003",
            ".ferrus/tasks/t-003.md",
            crate::project::TaskStatus::Complete,
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(".ferrus/tasks").await.unwrap();
        tokio::fs::create_dir_all(".ferrus/runs/t-001")
            .await
            .unwrap();
        tokio::fs::create_dir_all(".ferrus/runs/t-002")
            .await
            .unwrap();
        tokio::fs::create_dir_all(".ferrus/runs/t-003")
            .await
            .unwrap();
        tokio::fs::write(".ferrus/tasks/t-001.md", "pending task")
            .await
            .unwrap();
        tokio::fs::write(".ferrus/tasks/t-002.md", "executing task")
            .await
            .unwrap();
        tokio::fs::write(".ferrus/tasks/t-003.md", "complete task")
            .await
            .unwrap();
        tokio::fs::write(".ferrus/runs/t-001/QUESTION.md", "stale question")
            .await
            .unwrap();
        tokio::fs::write(".ferrus/runs/t-002/CONSULT_REQUEST.md", "stale consult")
            .await
            .unwrap();
        tokio::fs::write(".ferrus/runs/t-003/SUBMISSION.md", "complete submission")
            .await
            .unwrap();

        let (_state_tx, state_rx) = watch::channel::<Option<WatchedState>>(None);
        let (msg_tx, _msg_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ctx = HqContext::new(state_rx, Display(msg_tx), false);
        ctx.do_reset(false).await.unwrap();

        crate::test_support::assert_no_state_json();
        let tasks = crate::project::list_tasks().await.unwrap();
        let status = |id: &str| {
            tasks
                .iter()
                .find(|task| task.id == id)
                .map(|task| task.status.as_str())
        };
        assert_eq!(status("t-001"), Some("reset"));
        assert_eq!(status("t-002"), Some("reset"));
        assert_eq!(status("t-003"), Some("complete"));
        assert!(!std::path::Path::new(".ferrus/tasks/t-001.md").exists());
        assert!(!std::path::Path::new(".ferrus/tasks/t-002.md").exists());
        assert!(std::path::Path::new(".ferrus/tasks/t-003.md").exists());
        assert!(!std::path::Path::new(".ferrus/runs/t-001").exists());
        assert!(!std::path::Path::new(".ferrus/runs/t-002").exists());
        assert!(std::path::Path::new(".ferrus/runs/t-003/SUBMISSION.md").exists());

        std::env::set_current_dir(previous).unwrap();
    }

    #[tokio::test]
    async fn reconcile_runtime_schedule_does_not_require_state_json() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let data_dir = dir.path().join("runtime");
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        let local_ref = crate::project::LocalProjectRef {
            project_id: "test-project".to_string(),
            name: "test".to_string(),
            data_dir: data_dir.to_string_lossy().into_owned(),
        };
        tokio::fs::write(
            ".ferrus/project.toml",
            toml::to_string_pretty(&local_ref).unwrap(),
        )
        .await
        .unwrap();
        crate::project::record_task_status(
            "t-001",
            ".ferrus/tasks/t-001.md",
            crate::project::TaskStatus::Complete,
        )
        .await
        .unwrap();

        let (_state_tx, state_rx) = watch::channel::<Option<WatchedState>>(None);
        let (msg_tx, _msg_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ctx = HqContext::new(state_rx, Display(msg_tx), false);
        ctx.seed_completed_task_announcements().await.unwrap();

        ctx.reconcile_runtime_schedule().await.unwrap();

        crate::test_support::assert_no_state_json();
        std::env::set_current_dir(previous).unwrap();
    }

    #[tokio::test]
    async fn reconcile_runtime_schedule_announces_new_completed_tasks_once() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let data_dir = dir.path().join("runtime");
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        let local_ref = crate::project::LocalProjectRef {
            project_id: "test-project".to_string(),
            name: "test".to_string(),
            data_dir: data_dir.to_string_lossy().into_owned(),
        };
        tokio::fs::write(
            ".ferrus/project.toml",
            toml::to_string_pretty(&local_ref).unwrap(),
        )
        .await
        .unwrap();
        crate::project::record_task_status(
            "t-001",
            ".ferrus/tasks/t-001.md",
            crate::project::TaskStatus::Pending,
        )
        .await
        .unwrap();

        let (_state_tx, state_rx) = watch::channel::<Option<WatchedState>>(None);
        let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ctx = HqContext::new(state_rx, Display(msg_tx), false);
        ctx.seed_completed_task_announcements().await.unwrap();

        crate::project::record_task_status(
            "t-001",
            ".ferrus/tasks/t-001.md",
            crate::project::TaskStatus::Complete,
        )
        .await
        .unwrap();

        ctx.reconcile_runtime_schedule().await.unwrap();

        match msg_rx
            .try_recv()
            .expect("completion message should be sent")
        {
            tui::UiMessage::Success(text) => assert_eq!(text, "Task t-001 completed."),
            _ => panic!("expected success message"),
        }
        assert!(msg_rx.try_recv().is_err());

        ctx.reconcile_runtime_schedule().await.unwrap();
        assert!(msg_rx.try_recv().is_err());

        std::env::set_current_dir(previous).unwrap();
    }

    #[tokio::test]
    async fn consultation_workspace_uses_latest_executor_task_worktree() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let data_dir = dir.path().join("runtime");
        let workspace_dir = data_dir.join("worktrees").join("t-010");
        tokio::fs::create_dir_all(&workspace_dir).await.unwrap();
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        let local_ref = crate::project::LocalProjectRef {
            project_id: "test-project".to_string(),
            name: "test".to_string(),
            data_dir: data_dir.to_string_lossy().into_owned(),
        };
        tokio::fs::write(
            ".ferrus/project.toml",
            toml::to_string_pretty(&local_ref).unwrap(),
        )
        .await
        .unwrap();
        let metadata = crate::project::ProjectMetadata {
            id: "test-project".to_string(),
            name: "test".to_string(),
            workspace_dir: dir.path().to_string_lossy().into_owned(),
            ferrus_dir: dir.path().join(".ferrus").to_string_lossy().into_owned(),
            vcs: Some("git".to_string()),
            origin_repo: None,
            default_branch: None,
            current_head: None,
            created_at: "2026-06-14T00:00:00Z".to_string(),
            last_opened_at: "2026-06-14T00:00:00Z".to_string(),
            version: 1,
        };
        tokio::fs::write(
            data_dir.join("project.toml"),
            toml::to_string_pretty(&metadata).unwrap(),
        )
        .await
        .unwrap();
        crate::project::record_task_status(
            "t-010",
            ".ferrus/tasks/t-010.md",
            crate::project::TaskStatus::Consultation,
        )
        .await
        .unwrap();
        crate::project::record_run_started_with_workspace(
            "run-executor-t-010",
            ROLE_EXECUTOR,
            "executor:codex:t-010",
            std::process::id(),
            workspace_dir.to_string_lossy().into_owned(),
        )
        .await
        .unwrap();
        crate::project::attach_running_run_to_task(
            "executor:codex:t-010",
            "t-010",
            ".ferrus/tasks/t-010.md",
        )
        .await
        .unwrap();

        let workspace = latest_executor_workspace_for_task("t-010")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(workspace.workspace_dir, workspace_dir);
        assert_eq!(
            workspace.project_root,
            std::fs::canonicalize(dir.path()).unwrap()
        );

        std::env::set_current_dir(previous).unwrap();
    }

    #[tokio::test]
    async fn human_answer_for_dead_waiter_is_not_restored_before_delivery() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let data_dir = dir.path().join("runtime");
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        let local_ref = crate::project::LocalProjectRef {
            project_id: "test-project".to_string(),
            name: "test".to_string(),
            data_dir: data_dir.to_string_lossy().into_owned(),
        };
        tokio::fs::write(
            ".ferrus/project.toml",
            toml::to_string_pretty(&local_ref).unwrap(),
        )
        .await
        .unwrap();
        crate::project::record_task_status(
            "t-011",
            ".ferrus/tasks/t-011.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        crate::project::claim_task(
            "t-011",
            ".ferrus/tasks/t-011.md",
            "executor:codex:t-011",
            60,
        )
        .await
        .unwrap();
        crate::project::record_task_human_question_requested(
            "t-011",
            crate::project::TaskStatus::Executing,
            "executor:codex:t-011",
        )
        .await
        .unwrap();
        store::write_question_for_run_dir(".ferrus/runs/t-011", "Which branch?")
            .await
            .unwrap();

        let (_state_tx, state_rx) = watch::channel::<Option<WatchedState>>(None);
        let (msg_tx, _msg_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ctx = HqContext::new(state_rx, Display(msg_tx), false);

        let error = ctx
            .answer_scoped_human_question("Use the short branch.".to_string())
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("Executor agent is not configured"));
        let task = crate::project::list_tasks()
            .await
            .unwrap()
            .into_iter()
            .find(|task| task.id == "t-011")
            .unwrap();
        assert_eq!(task.status, "awaiting_human");
        assert_eq!(
            store::read_answer_for_run_dir(".ferrus/runs/t-011")
                .await
                .unwrap(),
            "Use the short branch."
        );
        assert_eq!(
            store::read_question_for_run_dir(".ferrus/runs/t-011")
                .await
                .unwrap(),
            "Which branch?"
        );

        std::env::set_current_dir(previous).unwrap();
    }

    #[test]
    fn run_plan_prompt_context_uses_selected_prefix_only() {
        let plan = RunPlan {
            spec_path: "docs/specs/spec.md".to_string(),
            eligible: vec![
                RunPlanMilestone {
                    id: "m1.0".to_string(),
                    marker: "#1.0".to_string(),
                    title: "First task".to_string(),
                },
                RunPlanMilestone {
                    id: "m1.1".to_string(),
                    marker: "#1.1".to_string(),
                    title: "Second task".to_string(),
                },
            ],
            skipped: Vec::new(),
        };

        let context = run_plan_prompt_context(&plan, 1);

        assert!(context.contains("Spec: docs/specs/spec.md"));
        assert!(context.contains("Task count: 1"));
        assert!(context.contains("Milestone ID: m1.0"));
        assert!(!context.contains("Milestone ID: m1.1"));
    }

    #[test]
    fn run_plan_lines_do_not_report_batch_launch_as_unwired() {
        let plan = RunPlan {
            spec_path: "docs/specs/spec.md".to_string(),
            eligible: vec![RunPlanMilestone {
                id: "m1.0".to_string(),
                marker: "#1.0".to_string(),
                title: "First task".to_string(),
            }],
            skipped: Vec::new(),
        };

        let lines = run_plan_lines(&plan, 1).join("\n");

        assert!(!lines.contains("not wired"));
        assert!(lines.contains("selected  : 1"));
    }

    #[test]
    fn executor_spawn_selection_skips_live_tasks_before_slot_limit() {
        let tasks = vec![
            task_record("t-001", crate::project::TaskStatus::Pending),
            task_record("t-002", crate::project::TaskStatus::Pending),
        ];

        let selected = select_executor_spawn_tasks(&tasks, 1, |task| task.id == "t-001");

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, "t-002");
    }

    #[cfg(unix)]
    #[test]
    fn stale_pid_detection() {
        assert!(platform::pid_is_alive(std::process::id()));
        assert!(!platform::pid_is_alive(999999));
    }

    #[tokio::test]
    async fn plain_input_answers_first_scoped_human_question() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus/tasks")).unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus/runs/t-007")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let data_dir = dir.path().join(".ferrus/projects/test-project");
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        let local_ref = crate::project::LocalProjectRef {
            project_id: "test-project".to_string(),
            name: "test".to_string(),
            data_dir: data_dir.to_string_lossy().into_owned(),
        };
        tokio::fs::write(
            ".ferrus/project.toml",
            toml::to_string_pretty(&local_ref).unwrap(),
        )
        .await
        .unwrap();
        crate::project::record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        crate::project::record_task_human_question_requested(
            "t-007",
            crate::project::TaskStatus::Executing,
            "executor:codex:7",
        )
        .await
        .unwrap();
        store::write_question_for_run_dir(".ferrus/runs/t-007", "Need human input")
            .await
            .unwrap();

        let (_state_tx, state_rx) = watch::channel::<Option<WatchedState>>(None);
        let (msg_tx, _msg_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ctx = HqContext::new(state_rx, Display(msg_tx), false);

        let error = dispatch("Use option A", &mut ctx)
            .await
            .unwrap_err()
            .to_string();

        crate::test_support::assert_no_state_json();
        assert!(error.contains("Executor agent is not configured"));
        assert_eq!(
            store::read_answer_for_run_dir(".ferrus/runs/t-007")
                .await
                .unwrap(),
            "Use option A"
        );
        assert_eq!(
            store::read_question_for_run_dir(".ferrus/runs/t-007")
                .await
                .unwrap(),
            "Need human input"
        );
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "awaiting_human");

        std::env::set_current_dir(previous).unwrap();
    }

    #[tokio::test]
    async fn plain_input_answers_scoped_human_question_without_state_json() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus/tasks")).unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus/runs/t-009")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let data_dir = dir.path().join(".ferrus/projects/test-project");
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        let local_ref = crate::project::LocalProjectRef {
            project_id: "test-project".to_string(),
            name: "test".to_string(),
            data_dir: data_dir.to_string_lossy().into_owned(),
        };
        tokio::fs::write(
            ".ferrus/project.toml",
            toml::to_string_pretty(&local_ref).unwrap(),
        )
        .await
        .unwrap();
        crate::project::record_task_status(
            "t-009",
            ".ferrus/tasks/t-009.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        crate::project::record_task_human_question_requested(
            "t-009",
            crate::project::TaskStatus::Executing,
            "executor:codex:9",
        )
        .await
        .unwrap();
        store::write_question_for_run_dir(".ferrus/runs/t-009", "Need scoped input")
            .await
            .unwrap();

        let (_state_tx, state_rx) = watch::channel::<Option<WatchedState>>(None);
        let (msg_tx, _msg_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ctx = HqContext::new(state_rx, Display(msg_tx), false);

        let error = dispatch("Use scoped answer", &mut ctx)
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("Executor agent is not configured"));
        assert_eq!(
            store::read_answer_for_run_dir(".ferrus/runs/t-009")
                .await
                .unwrap(),
            "Use scoped answer"
        );
        assert_eq!(
            store::read_question_for_run_dir(".ferrus/runs/t-009")
                .await
                .unwrap(),
            "Need scoped input"
        );
        let task = crate::project::list_tasks()
            .await
            .unwrap()
            .into_iter()
            .find(|task| task.id == "t-009")
            .unwrap();
        assert_eq!(task.status, "awaiting_human");
        crate::test_support::assert_no_state_json();

        std::env::set_current_dir(previous).unwrap();
    }
}
