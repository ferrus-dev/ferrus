use super::super::*;

impl HqContext {
    pub(super) async fn schedule_queued_tasks(&mut self) -> Result<usize> {
        self.ensure_hq_config().await?;
        let config = Config::load().await?;
        let max_parallel = config.limits.max_parallel_tasks.max(1);
        let tasks = crate::project::list_tasks().await?;
        self.schedule_queued_tasks_from(tasks, max_parallel, true)
            .await
    }

    pub(super) async fn schedule_reviewing_tasks(
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
        let live_run_task_ids = crate::project::live_active_run_task_ids().await?;
        let prompt = agent_manager::reviewer_prompt();
        let mut spawned = 0usize;
        let mut spawn_error = None;
        let mut review_tasks = Vec::new();
        for task in tasks
            .iter()
            .filter(|task| task.status == crate::project::TaskStatus::Reviewing.as_str())
        {
            let expected_agent_id = self.supervisor_agent_id_for_task(&task.id)?;
            if task_claim_blocks_spawn(task, &expected_agent_id, now, &live_run_task_ids) {
                continue;
            }
            review_tasks.push(task.clone());
            if review_tasks.len() == slots {
                break;
            }
        }

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

    pub(super) async fn schedule_consultation_tasks(
        &mut self,
        tasks: &[TaskRecord],
        max_parallel: usize,
    ) -> Result<usize> {
        let running = self.running_supervisor_count();
        let slots = max_parallel.saturating_sub(running);
        if slots == 0 {
            return Ok(0);
        }

        let prompt = agent_manager::consultant_prompt();
        let mut spawned = 0usize;
        let mut spawn_error = None;
        let live_supervisor_task_ids =
            crate::project::live_active_run_task_ids_for_role(ROLE_SUPERVISOR).await?;
        let consultation_tasks =
            actionable_consultation_tasks(tasks, slots, &live_supervisor_task_ids).await?;
        if consultation_tasks.is_empty() {
            return Ok(0);
        }

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

    pub(super) async fn schedule_answered_consultation_tasks(
        &mut self,
        tasks: &[TaskRecord],
        max_parallel: usize,
    ) -> Result<usize> {
        let max_parallel = executor_parallel_limit(max_parallel).await?;
        let running = self.occupied_executor_slots().await?;
        let slots = max_parallel.saturating_sub(running);
        if slots == 0 {
            return Ok(0);
        }

        let now = chrono::Utc::now();
        let live_run_task_ids = crate::project::live_active_run_task_ids().await?;
        let answered_tasks = answered_consultation_tasks(tasks).await?;
        let mut spawn_tasks = Vec::new();
        for task in answered_tasks {
            let name = self.executor_agent_id_for_task(&task.id)?;
            if task_claim_blocks_spawn(&task, &name, now, &live_run_task_ids)
                || self
                    .headless
                    .get(&name)
                    .is_some_and(agent_manager::HeadlessHandle::is_alive)
            {
                continue;
            }
            spawn_tasks.push((task, name));
            if spawn_tasks.len() == slots {
                break;
            }
        }

        let mut spawned = 0usize;
        let mut spawn_error = None;
        for (task, name) in spawn_tasks {
            let index = u32::try_from(spawned + 1).context("Executor index exceeds u32 range")?;
            match self
                .spawn_headless_executor_for_task(
                    &name,
                    agent_manager::executor_wait_for_consult_prompt(),
                    index,
                    &task.id,
                )
                .await
            {
                Ok(()) => spawned += 1,
                Err(err) => {
                    spawn_error = Some(err);
                    break;
                }
            }
        }

        if let Some(err) = spawn_error {
            self.display.error(format!(
                "Could not resume more consultation executors after starting {spawned} task(s): {err}",
            ));
        }
        Ok(spawned)
    }

    pub(super) async fn schedule_answered_human_tasks(
        &mut self,
        waiters: &[crate::project::AnsweredHumanWaiter],
        max_parallel: usize,
    ) -> Result<usize> {
        if waiters.is_empty() {
            return Ok(0);
        }

        let live_run_agents = crate::project::live_active_run_agents().await?;
        let executor_max_parallel = executor_parallel_limit(max_parallel).await?;
        let mut executor_slots =
            executor_max_parallel.saturating_sub(self.occupied_executor_slots().await?);
        let mut supervisor_slots = max_parallel.saturating_sub(self.running_supervisor_count());
        let mut spawned = 0usize;

        for waiter in waiters {
            if answered_human_owner_is_live(
                &waiter.awaiting_human_by,
                &live_run_agents,
                self.headless
                    .get(&waiter.awaiting_human_by)
                    .is_some_and(agent_manager::HeadlessHandle::is_alive),
            ) {
                continue;
            }

            let slot = if waiter.awaiting_human_by.starts_with(ROLE_EXECUTOR) {
                &mut executor_slots
            } else if waiter.awaiting_human_by.starts_with(ROLE_SUPERVISOR) {
                &mut supervisor_slots
            } else {
                tracing::warn!(
                    task_id = waiter.task_id,
                    owner = waiter.awaiting_human_by,
                    "cannot resume answered human question for unknown agent role"
                );
                continue;
            };
            if *slot == 0 {
                continue;
            }

            match self
                .relaunch_human_answer_waiter(&waiter.awaiting_human_by, &waiter.task_id)
                .await
            {
                Ok(()) => {
                    *slot -= 1;
                    spawned += 1;
                }
                Err(err) => {
                    self.display.error(format!(
                        "Could not resume {} for answered task {}: {err}",
                        waiter.awaiting_human_by, waiter.task_id
                    ));
                }
            }
        }

        Ok(spawned)
    }

    pub(super) async fn schedule_queued_tasks_from(
        &mut self,
        tasks: Vec<TaskRecord>,
        max_parallel: usize,
        report_waiting: bool,
    ) -> Result<usize> {
        let now = chrono::Utc::now();
        let live_run_task_ids = crate::project::live_active_run_task_ids().await?;
        let max_parallel = executor_parallel_limit(max_parallel).await?;
        let mut ready_tasks = Vec::new();
        for task in tasks
            .into_iter()
            .filter(|task| is_executor_ready_task_status(&task.status))
        {
            let expected_agent_id = self.executor_agent_id_for_task(&task.id)?;
            if !task_claim_blocks_spawn(&task, &expected_agent_id, now, &live_run_task_ids) {
                ready_tasks.push(task);
            }
        }
        let ready_count = ready_tasks.len();
        if ready_count == 0 {
            return Ok(0);
        }

        let running = self.occupied_executor_slots().await?;
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

    pub(super) async fn occupied_executor_slots(&self) -> Result<usize> {
        let live_db_task_ids =
            crate::project::live_active_run_task_ids_for_role(ROLE_EXECUTOR).await?;
        Ok(occupied_executor_slots_from_handles(
            live_db_task_ids,
            self.headless.iter().filter_map(|(name, handle)| {
                (name.starts_with(ROLE_EXECUTOR) && handle.is_alive()).then_some(name.as_str())
            }),
        ))
    }

    pub(super) fn running_supervisor_count(&self) -> usize {
        self.headless
            .iter()
            .filter(|(name, handle)| name.starts_with(ROLE_SUPERVISOR) && handle.is_alive())
            .count()
    }

    pub(super) fn executor_agent_id_for_task(&self, task_id: &str) -> Result<String> {
        let executor = self
            .executor
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Executor agent is not configured"))?;
        Ok(format!("{}:{}:{}", ROLE_EXECUTOR, executor.name(), task_id))
    }

    pub(super) fn supervisor_agent_id_for_task(&self, task_id: &str) -> Result<String> {
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
}
