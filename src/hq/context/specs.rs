use super::super::*;

impl HqContext {
    pub(super) async fn selected_milestone_for_task(
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

    pub(in crate::hq) async fn reset_spec_selection(&mut self) -> Result<()> {
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

    pub(in crate::hq) async fn milestones(&mut self) -> Result<()> {
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

    pub(in crate::hq) async fn spec(&mut self) -> Result<()> {
        use std::process::Stdio;
        use tokio::process::Command;

        self.ensure_hq_config().await?;
        if !self.archive_completed_selected_spec_before_spec().await? {
            return Ok(());
        }
        prepare_spec_session_files().await?;

        let supervisor = std::sync::Arc::clone(
            self.supervisor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Supervisor agent is not configured"))?,
        );

        self.display.info(format!(
            "Spawning supervisor ({}) for specification drafting...",
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
                            "Spec created -- waiting for supervisor to exit...",
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

    pub(super) async fn archive_completed_selected_spec_before_spec(&mut self) -> Result<bool> {
        let Some(prompt) = selected_spec_archive_prompt().await? else {
            return Ok(true);
        };

        self.display.muted(format!(
            "\n  • Selected spec is complete\n  ╰─ {} ({} linked task artifacts)\n",
            prompt.spec_path, prompt.task_count
        ));
        let reply_rx = self
            .display
            .confirm_yes("Archive it before creating a new spec?");
        if !reply_rx.await.unwrap_or(true) {
            return Ok(true);
        }

        if self.archive_spec().await? {
            Ok(true)
        } else {
            self.display
                .info("Spec creation cancelled because the selected spec was not archived.");
            Ok(false)
        }
    }

    pub(in crate::hq) async fn archive_spec(&mut self) -> Result<bool> {
        use std::process::Stdio;
        use tokio::process::Command;

        self.ensure_hq_config().await?;
        crate::project::clear_last_spec_archive_path()
            .await
            .context("Failed to clear spec archive handoff metadata")?;

        let selection = crate::project::read_project_selection().await?;
        let Some(spec_path) = selection
            .selected_spec
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
        else {
            self.display
                .error("No selected spec. Run /milestones before /archive-spec.");
            return Ok(false);
        };
        if !Path::new(spec_path).exists() {
            self.display.error(format!(
                "Selected spec no longer exists:\n{spec_path}\n\nRun /milestones to select a valid spec."
            ));
            return Ok(false);
        }

        let spec = specs::load_spec(spec_path).await?;
        let incomplete = spec
            .milestones
            .iter()
            .filter(|milestone| !milestone.completed)
            .map(|milestone| format!("{} ({})", milestone.display_title(), milestone.id))
            .collect::<Vec<_>>();
        if !incomplete.is_empty() {
            self.display.error(format!(
                "Cannot archive selected spec. Incomplete milestone(s): {}",
                incomplete.join(", ")
            ));
            return Ok(false);
        }

        let tasks = crate::project::list_tasks_for_spec(spec_path).await?;
        if tasks.is_empty() {
            self.display.error(format!(
                "No task rows are linked to selected spec:\n{spec_path}"
            ));
            return Ok(false);
        }
        let active = tasks
            .iter()
            .filter(|task| {
                task.status
                    .parse::<crate::project::TaskStatus>()
                    .map(|status| !status.is_terminal())
                    .unwrap_or(true)
            })
            .map(|task| format!("{} ({})", task.id, task.status))
            .collect::<Vec<_>>();
        if !active.is_empty() {
            self.display.error(format!(
                "Cannot archive selected spec. Non-terminal task(s): {}",
                active.join(", ")
            ));
            return Ok(false);
        }

        let supervisor = std::sync::Arc::clone(
            self.supervisor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Supervisor agent is not configured"))?,
        );
        let context = archive_spec_prompt_context(&spec.path, &tasks);
        let prompt = agent_manager::supervisor_archive_spec_prompt(&context);

        self.display.muted(format!(
            "\n  • Spawning supervisor ({}) for spec archival...\n",
            supervisor.name()
        ));

        supervisor.validate_interactive_launch(ROLE_SUPERVISOR, DEFAULT_AGENT_INDEX)?;
        let mut cmd = Command::from(
            supervisor
                .spawn(AgentRunMode::Interactive {
                    prompt: Some(&prompt),
                })
                .with_context(|| {
                    format!(
                        "Failed to resolve launcher for supervisor agent {}",
                        supervisor.name()
                    )
                })?,
        );

        let ack_rx = self.display.suspend();
        let _ = ack_rx.await;
        let mut resume_guard = ResumeGuard::new(self.display.clone());
        let program = cmd.as_std().get_program().to_string_lossy().into_owned();
        cmd.env(ENV_SUPERVISOR_MODE, SUPERVISOR_MODE_ARCHIVE);
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

        let mut archive_path = None;
        let mut child_status = None;
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(300));
        loop {
            tokio::select! {
                status = child.wait() => {
                    child_status = Some(status.with_context(|| format!("Failed to wait for {program}"))?);
                    break;
                }
                _ = ticker.tick() => {
                    if let Ok(Some(path)) = crate::project::read_last_spec_archive_path().await
                        && !path.is_empty()
                    {
                        archive_path = Some(path);
                        self.stop_interactive_child(&mut child, "")
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

        if archive_path.is_none()
            && let Ok(Some(path)) = crate::project::read_last_spec_archive_path().await
            && !path.is_empty()
        {
            archive_path = Some(path);
        }

        if let Some(path) = archive_path {
            self.display
                .muted(format!("\n  • Spec archived\n  ╰─ {path}\n"));
            Ok(true)
        } else {
            self.display
                .info("No spec archive created. Re-run /archive-spec when ready.");
            Ok(false)
        }
    }
}
