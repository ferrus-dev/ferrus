//! Tests for input.

use super::*;

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

#[tokio::test]
async fn selected_spec_archive_prompt_requires_closed_unarchived_spec() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::fs::create_dir_all(dir.path().join(".ferrus/tasks")).unwrap();
    std::fs::create_dir_all(dir.path().join(".ferrus/runs/t-001")).unwrap();
    std::fs::create_dir_all(dir.path().join("docs/specs")).unwrap();
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

    let spec_path = "docs/specs/spec.md";
    tokio::fs::write(
        spec_path,
        "## Milestones\n\
         - [x] #1.0 Done\n\
           - ID: m1.0\n\
           - Depends on: none\n",
    )
    .await
    .unwrap();
    tokio::fs::write(".ferrus/tasks/t-001.md", "task")
        .await
        .unwrap();
    tokio::fs::write(".ferrus/runs/t-001/SUBMISSION.md", "done")
        .await
        .unwrap();
    crate::project::write_project_selection(&ProjectSelection {
        selected_spec: Some(spec_path.to_string()),
    })
    .await
    .unwrap();
    crate::project::record_task_status_with_origin(
        "t-001",
        ".ferrus/tasks/t-001.md",
        crate::project::TaskStatus::Complete,
        Some(spec_path),
        Some("m1.0"),
    )
    .await
    .unwrap();

    assert_eq!(
        selected_spec_archive_prompt().await.unwrap(),
        Some(SpecArchivePrompt {
            spec_path: spec_path.to_string(),
            task_count: 1,
        })
    );

    tokio::fs::remove_file(".ferrus/tasks/t-001.md")
        .await
        .unwrap();
    tokio::fs::remove_dir_all(".ferrus/runs/t-001")
        .await
        .unwrap();
    crate::project::record_task_status_with_origin(
        "t-001",
        &data_dir
            .join("archive/specs/spec/tasks/t-001.md")
            .to_string_lossy(),
        crate::project::TaskStatus::Complete,
        Some(spec_path),
        Some("m1.0"),
    )
    .await
    .unwrap();
    tokio::fs::create_dir_all(".ferrus/runs/t-001")
        .await
        .unwrap();
    tokio::fs::write(".ferrus/runs/t-001/SUBMISSION.md", "stale checkout copy")
        .await
        .unwrap();

    assert_eq!(selected_spec_archive_prompt().await.unwrap(), None);
    std::env::set_current_dir(previous).unwrap();
}
