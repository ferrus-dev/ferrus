use super::*;

mod interaction;
mod scheduling;
mod specs;

impl HqContext {
    pub(super) fn new(
        state_rx: watch::Receiver<Option<WatchedState>>,
        display: Display,
        debug: bool,
    ) -> Self {
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

    pub(super) async fn seed_completed_task_announcements(&mut self) -> Result<()> {
        let tasks = crate::project::list_tasks().await?;
        self.announced_completed_tasks
            .extend(completed_task_ids(&tasks));
        Ok(())
    }

    pub(super) fn set_hq_config(&mut self, hq: &HqConfig) {
        self.supervisor = hq.supervisor_agent().ok();
        self.executor = hq.executor_agent().ok();
    }

    pub(super) fn executor_agent_id(&self) -> Result<String> {
        self.executor_agent_id_for_index(DEFAULT_AGENT_INDEX)
    }

    pub(super) fn executor_agent_id_for_index(&self, index: u32) -> Result<String> {
        let executor = self
            .executor
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Executor agent is not configured"))?;
        Ok(agent_id(ROLE_EXECUTOR, executor.name(), index))
    }

    pub(super) fn supervisor_agent_id(&self) -> Result<String> {
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

    pub(super) async fn ensure_hq_config(&mut self) -> Result<()> {
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

    pub(super) async fn reload_hq_config(&mut self) -> Result<()> {
        let config = Config::load().await?;
        let hq = config.hq.ok_or_else(|| {
            anyhow::anyhow!("No [hq.supervisor] / [hq.executor] sections in ferrus.toml.")
        })?;
        self.set_hq_config(&hq);
        Ok(())
    }

    pub(super) async fn update_model(
        &mut self,
        target: ModelTarget,
        model: Option<&str>,
    ) -> Result<()> {
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

    pub(super) async fn mark_agent_running(
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

    pub(super) async fn mark_agent_suspended(&self, name: &str) -> Result<()> {
        use agents::{AgentStatus, read_agents, write_agents};

        let mut reg = read_agents().await?;
        if let Some(entry) = reg.by_name_mut(name) {
            entry.pid = None;
            entry.status = AgentStatus::Suspended;
        }
        write_agents(&reg).await
    }

    pub(super) async fn spawn_interactive_command(
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

    pub(super) async fn stop_interactive_child(
        &self,
        child: &mut tokio::process::Child,
        message: &str,
    ) -> Result<()> {
        if !message.is_empty() {
            self.display.muted(message);
        }
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

    pub(super) async fn prepare_headless_slot(&mut self, name: &str) -> bool {
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

    pub(super) fn store_headless_handle(
        &mut self,
        name: &str,
        handle: agent_manager::HeadlessHandle,
    ) {
        self.display.muted(format!(
            "  • Started {name}...\n  ╰─ Logs: {}\n\n",
            handle.log_path.display()
        ));
        self.headless.insert(name.to_string(), handle);
    }

    pub(super) async fn spawn_headless_supervisor_for_task(
        &mut self,
        name: &str,
        prompt: &str,
        task_id: &str,
    ) -> Result<()> {
        self.spawn_headless_supervisor_for_task_with_workspace(name, prompt, task_id, None)
            .await
    }

    pub(super) async fn spawn_headless_supervisor_for_task_with_workspace(
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

    pub(super) async fn reconcile_runtime_schedule(&mut self) -> Result<()> {
        self.reap_exited_headless().await;

        let _ = crate::project::recover_runtime_state().await;
        let tasks = crate::project::list_tasks().await?;
        let answered_human_waiters = crate::project::list_answered_human_waiters().await?;
        self.announce_completed_tasks(&tasks);
        if !tasks.iter().any(|task| {
            is_executor_ready_task_status(&task.status)
                || is_review_or_consultation_task_status(&task.status)
        }) && answered_human_waiters.is_empty()
        {
            return Ok(());
        }

        self.ensure_hq_config().await?;
        let config = Config::load().await?;
        let max_parallel = config.limits.max_parallel_tasks.max(1);
        self.schedule_answered_human_tasks(&answered_human_waiters, max_parallel)
            .await?;
        self.schedule_consultation_tasks(&tasks, max_parallel)
            .await?;
        self.schedule_answered_consultation_tasks(&tasks, max_parallel)
            .await?;
        self.schedule_reviewing_tasks(&tasks, max_parallel).await?;
        self.schedule_queued_tasks_from(tasks, max_parallel, false)
            .await?;
        Ok(())
    }

    pub(super) fn announce_completed_tasks(&mut self, tasks: &[TaskRecord]) {
        for task_id in completed_task_ids(tasks) {
            if self.announced_completed_tasks.insert(task_id.clone()) {
                self.display.success(format!("Task {task_id} completed."));
            }
        }
    }

    pub(super) async fn reap_exited_headless(&mut self) {
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

    pub(super) async fn spawn_headless_executor_for_task(
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

        // Gate the dispatch against the per-work-phase budget before committing
        // to any setup. This bounds the respawn loop of a session that keeps
        // exiting without reaching review. The counter itself is incremented
        // only after the session actually starts (below), so a failed
        // worktree/process setup does not burn the budget.
        let max_dispatches = Config::load().await?.limits.max_executor_dispatches;
        if let crate::project::ExecutorDispatchOutcome::LimitExceeded { dispatches } =
            crate::project::enforce_executor_dispatch_limit(task_id, max_dispatches).await?
        {
            self.display.error(format!(
                "Task {task_id} reached the executor dispatch limit ({dispatches}/{max_dispatches}) \
                 for this work phase without reaching review.\n\nState is now Failed. Inspect with \
                 /tasks; adjust limits.max_executor_dispatches or refine the task before retrying."
            ));
            return Ok(());
        }

        let workspace = prepare_executor_workspace(task_id).await?;
        let _ = crate::repository_graph_runtime::schedule_task_baseline_pin(
            task_id,
            &workspace.workspace_dir,
            workspace.baseline_tree.as_deref(),
        )
        .await;
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

        // The session is now running: account for this dispatch against the
        // per-work-phase budget gated above. Done last so setup failures don't
        // consume the budget.
        let dispatches = crate::project::record_executor_dispatch(task_id).await?;
        tracing::debug!(task_id, dispatches, max_dispatches, "executor dispatch");
        Ok(())
    }

    pub(super) async fn resume(&mut self) -> Result<()> {
        let _ = crate::project::recover_runtime_state().await;
        let tasks = crate::project::list_tasks().await?;
        let answered_human_waiters = crate::project::list_answered_human_waiters().await?;
        let has_runtime_work = tasks.iter().any(|task| {
            is_executor_ready_task_status(&task.status)
                || is_review_or_consultation_task_status(&task.status)
        }) || !answered_human_waiters.is_empty();
        if has_runtime_work {
            self.ensure_hq_config().await?;
            let config = Config::load().await?;
            let max_parallel = config.limits.max_parallel_tasks.max(1);
            let human_answer = self
                .schedule_answered_human_tasks(&answered_human_waiters, max_parallel)
                .await?;
            let consultation = self
                .schedule_consultation_tasks(&tasks, max_parallel)
                .await?;
            let consultation_executor = self
                .schedule_answered_consultation_tasks(&tasks, max_parallel)
                .await?;
            let reviewing = self.schedule_reviewing_tasks(&tasks, max_parallel).await?;
            let executor = self
                .schedule_queued_tasks_from(tasks, max_parallel, true)
                .await?;
            if human_answer + consultation + consultation_executor + reviewing + executor == 0 {
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

    pub(super) async fn review(&mut self) -> Result<()> {
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

    pub(super) async fn check(&mut self, force: bool) -> Result<()> {
        let _ = force;
        self.run_hq_checks_without_state().await?;
        Ok(())
    }

    pub(super) async fn run_hq_checks_without_state(&self) -> Result<()> {
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

    pub(super) async fn reset(&mut self) -> Result<()> {
        self.do_reset(true).await
    }

    pub(super) async fn do_reset(&mut self, prompt: bool) -> Result<()> {
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
            crate::repository_graph_runtime::release_submitted_tree_pin_for_task_best_effort(
                &task.id,
            )
            .await;
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

    pub(super) async fn stop(&mut self) -> Result<()> {
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

    pub(super) async fn spawn_interactive_supervisor(
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

    pub(super) async fn spawn_interactive_supervisor_until_task_enqueued(
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
                "Task enqueued -- returning to HQ...",
            )
            .await?
            .into_iter()
            .next())
    }

    pub(super) async fn spawn_interactive_supervisor_until_tasks_enqueued(
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

    pub(super) async fn spawn_interactive_executor(
        &mut self,
        name: &str,
        prompt: Option<&str>,
    ) -> Result<()> {
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

    pub(super) async fn plan(&mut self) -> Result<()> {
        self.ensure_hq_config().await?;
        let agent = std::sync::Arc::clone(
            self.supervisor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Supervisor agent is not configured"))?,
        );

        self.display.info(format!(
            "Spawning supervisor ({}) for free-form planning...",
            agent.name()
        ));

        let supervisor_id = self.supervisor_agent_id()?;
        self.spawn_interactive_supervisor(
            &supervisor_id,
            Some(agent_manager::supervisor_plan_prompt()),
        )
        .await
    }

    pub(super) async fn task(
        &mut self,
        manual: bool,
        confirm_selected_milestone: bool,
    ) -> Result<()> {
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
            .info(format!("Spawning supervisor ({})...", supervisor.name()));
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

    pub(super) async fn run_batch_plan(&mut self, limit: Option<usize>) -> Result<()> {
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

    pub(super) async fn launch_batch_task_supervisor(
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
            "Spawning supervisor ({}) for batch task preparation...",
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
            "Batch tasks enqueued -- returning to HQ...",
        )
        .await?;
        self.display
            .info("Batch preparation session finished. Use /tasks to inspect queued tasks.");
        self.schedule_queued_tasks().await?;
        Ok(())
    }
}
