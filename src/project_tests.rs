use super::*;
use tempfile::TempDir;

async fn setup_project() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    let workspace = dir.path();
    let data_dir = workspace.join(".ferrus/projects/test-project");
    std::fs::create_dir_all(workspace.join(".ferrus")).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    write_toml(
        &workspace.join(".ferrus/project.toml"),
        &LocalProjectRef {
            project_id: "test-project".to_string(),
            name: "test".to_string(),
            data_dir: path_string(&data_dir),
        },
    )
    .await
    .unwrap();
    write_toml(
        &data_dir.join("project.toml"),
        &ProjectMetadata {
            id: "test-project".to_string(),
            name: "test".to_string(),
            workspace_dir: path_string(workspace),
            ferrus_dir: path_string(&workspace.join(".ferrus")),
            vcs: None,
            origin_repo: None,
            default_branch: None,
            current_head: None,
            created_at: timestamp(),
            last_opened_at: timestamp(),
            version: PROJECT_VERSION,
        },
    )
    .await
    .unwrap();
    std::env::set_current_dir(workspace).unwrap();
    initialize_database(&data_dir.join("ferrus.db"))
        .await
        .unwrap();

    record_task_status(
        "t-001",
        ".ferrus/tasks/t-001.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();

    (dir, previous)
}

fn teardown(previous: PathBuf) {
    std::env::set_current_dir(previous).unwrap();
}

#[path = "project_tests/graph_and_schema.rs"]
mod graph_and_schema;

#[path = "project_tests/migration_and_archive.rs"]
mod migration_and_archive;

#[path = "project_tests/task_lifecycle.rs"]
mod task_lifecycle;

#[path = "project_tests/recovery_and_runs.rs"]
mod recovery_and_runs;
