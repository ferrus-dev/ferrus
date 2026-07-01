use crate::project::{EventRecord, ProjectListEntry, RunRecord, TaskRecord};

pub fn project_lines(projects: &[ProjectListEntry]) -> Vec<String> {
    if projects.is_empty() {
        return vec!["No projects registered in ~/.ferrus/projects.".to_string()];
    }

    let mut lines = vec![format!(
        "{:<29} {:<22} {:<8} {:<20} {:<36} Data dir",
        "ID", "Name", "DB", "Last opened", "Workspace"
    )];
    lines.extend(projects.iter().map(|project| {
        let db_status = if project.database_exists {
            "ok"
        } else {
            "missing"
        };
        let workspace = project
            .workspace_dir
            .as_deref()
            .or(project.error.as_deref())
            .unwrap_or("-");
        format!(
            "{:<29} {:<22} {:<8} {:<20} {:<36} {}",
            project.id,
            compact(project.name.as_deref().unwrap_or("-"), 22),
            db_status,
            project.last_opened_at.as_deref().unwrap_or("-"),
            compact(workspace, 36),
            project.data_dir.display()
        )
    }));
    lines
}

pub fn task_lines(tasks: &[TaskRecord]) -> Vec<String> {
    if tasks.is_empty() {
        return vec!["No tasks recorded in ferrus.db.".to_string()];
    }

    let mut lines = vec![format!(
        "{:<14} {:<14} {:<14} {:<14} {:<14} {:<24} {:<22} {:<22} {:<6} {:<7} {:<28} Path",
        "ID",
        "Status",
        "Spec",
        "Milestone",
        "Paused",
        "Claimed by",
        "Lease until",
        "Heartbeat",
        "Checks",
        "Reviews",
        "Failure"
    )];
    lines.extend(tasks.iter().map(|task| {
        format!(
            "{:<14} {:<14} {:<14} {:<14} {:<14} {:<24} {:<22} {:<22} {:<6} {:<7} {:<28} {}",
            task.id,
            task.status,
            compact(task.spec_path.as_deref().unwrap_or("-"), 14),
            task.milestone_id.as_deref().unwrap_or("-"),
            task.paused_status.as_deref().unwrap_or("-"),
            task.claimed_by.as_deref().unwrap_or("-"),
            task.lease_until.as_deref().unwrap_or("-"),
            task.last_heartbeat.as_deref().unwrap_or("-"),
            task.check_retries,
            task.review_cycles,
            compact(task.failure_reason.as_deref().unwrap_or("-"), 28),
            task.path
        )
    }));
    lines
}

pub fn run_lines(runs: &[RunRecord]) -> Vec<String> {
    if runs.is_empty() {
        return vec!["No runs recorded in ferrus.db.".to_string()];
    }

    let mut lines = vec![format!(
        "{:<31} {:<10} {:<10} {:<12} {:<12} {:<8} {:<20} {:<20} Workspace",
        "ID", "Task", "Role", "Agent", "Status", "PID", "Started", "Updated"
    )];
    lines.extend(runs.iter().map(|run| {
        format!(
            "{:<31} {:<10} {:<10} {:<12} {:<12} {:<8} {:<20} {:<20} {}",
            run.id,
            run.task_id,
            run.role,
            compact(&run.agent, 12),
            run.status,
            run.pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_string()),
            run.started_at,
            run.updated_at,
            run.workspace_path
        )
    }));
    lines
}

pub fn event_lines(events: &[EventRecord], run_filter: Option<&str>) -> Vec<String> {
    if events.is_empty() {
        return vec![match run_filter {
            Some(run_id) => format!("No events recorded for run {run_id}."),
            None => "No events recorded in ferrus.db.".to_string(),
        }];
    }

    let mut lines = vec![format!(
        "{:<6} {:<31} {:<24} {:<20} Payload",
        "ID", "Run", "Type", "Created"
    )];
    lines.extend(events.iter().map(|event| {
        format!(
            "{:<6} {:<31} {:<24} {:<20} {}",
            event.id,
            event.run_id.as_deref().unwrap_or("-"),
            event.event_type,
            event.created_at,
            compact(&event.payload_json, 96)
        )
    }));
    lines
}

fn compact(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let mut shortened: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        shortened.push_str("...");
    }
    shortened
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_lines_include_empty_message() {
        assert_eq!(
            task_lines(&[]),
            vec!["No tasks recorded in ferrus.db.".to_string()]
        );
    }

    #[test]
    fn task_lines_include_origin_fields() {
        let tasks = vec![TaskRecord {
            id: "t-001".to_string(),
            path: ".ferrus/tasks/t-001.md".to_string(),
            spec_path: Some("docs/specs/spec.md".to_string()),
            milestone_id: Some("m1.0".to_string()),
            status: "pending".to_string(),
            paused_status: None,
            claimed_by: None,
            lease_until: None,
            last_heartbeat: None,
            check_retries: 0,
            review_cycles: 0,
            failure_reason: None,
        }];

        let lines = task_lines(&tasks);

        assert!(lines[0].contains("Spec"));
        assert!(lines[0].contains("Milestone"));
        assert!(lines[1].contains("docs/specs/sp"));
        assert!(lines[1].contains("m1.0"));
    }

    #[test]
    fn project_lines_include_registry_fields() {
        let projects = vec![ProjectListEntry {
            id: "P123".to_string(),
            name: Some("ferrus".to_string()),
            workspace_dir: Some("/tmp/ferrus".to_string()),
            data_dir: "/tmp/.ferrus/projects/P123".into(),
            database_exists: true,
            last_opened_at: Some("2026-05-17T10:00:00Z".to_string()),
            error: None,
        }];

        let lines = project_lines(&projects);

        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains("P123"));
        assert!(lines[1].contains("ferrus"));
        assert!(lines[1].contains("ok"));
        assert!(lines[1].contains("/tmp/ferrus"));
    }

    #[test]
    fn run_lines_include_core_runtime_fields() {
        let runs = vec![RunRecord {
            id: "r-123".to_string(),
            task_id: "t-001".to_string(),
            role: "executor".to_string(),
            agent: "codex".to_string(),
            status: "running".to_string(),
            started_at: "2026-05-17T10:00:00Z".to_string(),
            updated_at: "2026-05-17T10:01:00Z".to_string(),
            pid: Some(42),
            workspace_path: "/tmp/ferrus".to_string(),
        }];

        let lines = run_lines(&runs);

        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("Started"));
        assert!(lines[1].contains("r-123"));
        assert!(lines[1].contains("executor"));
        assert!(lines[1].contains("42"));
    }

    #[test]
    fn event_lines_can_report_filtered_empty_state() {
        assert_eq!(
            event_lines(&[], Some("r-123")),
            vec!["No events recorded for run r-123.".to_string()]
        );
    }

    #[test]
    fn event_lines_compact_long_payloads() {
        let events = vec![EventRecord {
            id: 7,
            run_id: Some("r-123".to_string()),
            event_type: "run_started".to_string(),
            payload_json: "x".repeat(120),
            created_at: "2026-05-17T10:00:00Z".to_string(),
        }];

        let lines = event_lines(&events, None);

        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains("run_started"));
        assert!(lines[1].ends_with("..."));
    }
}
