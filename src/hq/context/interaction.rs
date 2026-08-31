use super::super::*;

impl HqContext {
    pub(in crate::hq) async fn supervisor_interactive(&mut self) -> Result<()> {
        self.ensure_hq_config().await?;
        let agent = std::sync::Arc::clone(
            self.supervisor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Supervisor agent is not configured"))?,
        );

        self.display.info(format!(
            "Spawning supervisor ({}) interactively...",
            agent.name()
        ));

        let supervisor_id = self.supervisor_agent_id()?;
        self.spawn_interactive_supervisor(&supervisor_id, None)
            .await
    }

    pub(in crate::hq) async fn executor_interactive(&mut self) -> Result<()> {
        self.ensure_hq_config().await?;
        let agent = std::sync::Arc::clone(
            self.executor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Executor agent is not configured"))?,
        );

        self.display.info(format!(
            "Spawning executor ({}) interactively...",
            agent.name()
        ));

        let executor_id = self.executor_agent_id()?;
        self.spawn_interactive_executor(&executor_id, None).await
    }

    /// Handle a raw-text answer from the user when state is AwaitingHuman.
    pub(in crate::hq) async fn answer(&mut self, response: String) -> Result<()> {
        if response.trim().is_empty() {
            anyhow::bail!("Answer cannot be empty.");
        }
        self.answer_scoped_human_question(response).await
    }

    pub(in crate::hq) async fn has_pending_human_question(&self) -> Result<bool> {
        if self.has_scoped_human_question().await? {
            return Ok(true);
        }
        Ok(false)
    }

    pub(in crate::hq) async fn has_scoped_human_question(&self) -> Result<bool> {
        match crate::project::list_human_questions().await {
            Ok(questions) => Ok(!questions.is_empty()),
            Err(err) => {
                tracing::debug!(error = ?err, "failed to list scoped human questions");
                Ok(false)
            }
        }
    }

    pub(in crate::hq) async fn answer_scoped_human_question(
        &mut self,
        response: String,
    ) -> Result<()> {
        self.answer_scoped_human_question_for_task(response, None)
            .await
    }

    pub(in crate::hq) async fn answer_scoped_human_question_for_task(
        &mut self,
        response: String,
        task_id: Option<&str>,
    ) -> Result<()> {
        if response.trim().is_empty() {
            anyhow::bail!("Answer cannot be empty.");
        }
        let question =
            select_human_question(crate::project::list_human_questions().await?, task_id)?;

        crate::project::record_scoped_human_answer(&question, &response).await?;
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
                "Waiting for {owner} to receive it via /wait_for_answer..."
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

    pub(in crate::hq) async fn relaunch_human_answer_waiter(
        &mut self,
        owner: &str,
        task_id: &str,
    ) -> Result<()> {
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

    pub(in crate::hq) async fn reap_headless(&mut self, name: &str) {
        if let Some(handle) = self.headless.remove(name) {
            handle.reap().await;
        }
    }

    pub(in crate::hq) async fn shutdown_all_headless(&mut self) {
        let handles: Vec<_> = self.headless.drain().map(|(_, handle)| handle).collect();
        for handle in handles {
            handle.terminate().await;
        }
    }
}
