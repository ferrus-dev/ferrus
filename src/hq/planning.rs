//! Milestone planning and specification session context.

use super::*;

pub(super) enum TaskMilestoneSelection {
    UseFallback,
    Use(SelectedMilestone),
    Stop,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RunPlanMilestone {
    pub(super) id: String,
    pub(super) marker: String,
    pub(super) title: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SkippedRunMilestone {
    pub(super) id: String,
    pub(super) marker: String,
    pub(super) title: String,
    pub(super) reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RunPlan {
    pub(super) spec_path: String,
    pub(super) eligible: Vec<RunPlanMilestone>,
    pub(super) skipped: Vec<SkippedRunMilestone>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SpecArchivePrompt {
    pub(super) spec_path: String,
    pub(super) task_count: usize,
}

pub(super) async fn prepare_spec_session_files() -> Result<()> {
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

pub(super) async fn build_run_plan(spec_path: &str) -> Result<RunPlan> {
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

pub(super) fn run_plan_lines(plan: &RunPlan, selected_count: usize) -> Vec<String> {
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

pub(super) fn run_plan_prompt_context(plan: &RunPlan, selected_count: usize) -> String {
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

pub(super) fn archive_spec_prompt_context(spec_path: &str, tasks: &[TaskRecord]) -> String {
    let mut lines = vec![
        format!("Spec path: {spec_path}"),
        format!("Task count: {}", tasks.len()),
        format!(
            "Failed attempts skipped for Outcome: {} (retained in archive history)",
            tasks.iter().filter(|task| task.status == "failed").count()
        ),
        "Linked tasks to inspect for Outcome:".to_string(),
    ];

    for task in tasks.iter().filter(|task| task.status != "failed") {
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

pub(super) async fn selected_spec_archive_prompt() -> Result<Option<SpecArchivePrompt>> {
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

pub(super) fn task_has_unarchived_artifacts(task: &TaskRecord) -> bool {
    let task_path = Path::new(&task.path);
    let task_file_in_checkout = task_path.starts_with(".ferrus/tasks") && task_path.exists();
    let run_dir_in_checkout = task_path.starts_with(".ferrus/tasks")
        && Path::new(&crate::project::run_dir_for_task_display(&task.id)).exists();
    task_file_in_checkout || run_dir_in_checkout
}

pub(super) async fn new_task_ids_since(existing_task_ids: &HashSet<String>) -> Result<Vec<String>> {
    let tasks = crate::project::list_tasks().await?;
    let mut task_ids = tasks
        .into_iter()
        .filter(|task| !existing_task_ids.contains(&task.id))
        .map(|task| task.id)
        .collect::<Vec<_>>();
    task_ids.sort();
    Ok(task_ids)
}

pub(super) fn selected_milestone_prompt_context(selected: &SelectedMilestone) -> String {
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
