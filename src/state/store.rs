use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::agent_id::ENV_PROJECT_ROOT;

const FERRUS_DIR: &str = ".ferrus";
const LOGS_DIR: &str = ".ferrus/logs";
const LOCAL_PROJECT_TOML: &str = ".ferrus/project.toml";

#[derive(Deserialize, Serialize)]
struct LocalProjectRef {
    data_dir: String,
}

#[derive(Deserialize, Serialize)]
struct ProjectMetadata {
    workspace_dir: String,
}

fn path(filename: &str) -> PathBuf {
    resolve_project_path(Path::new(FERRUS_DIR).join(filename))
}

pub fn resolve_project_path(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if path.is_absolute() || !starts_with_ferrus_dir(path) {
        return path.to_path_buf();
    }
    if path == Path::new(LOCAL_PROJECT_TOML) {
        return path.to_path_buf();
    }
    project_root_from_env()
        .map(|root| root.join(path))
        .or_else(|| canonical_project_root_from_local_project_ref().map(|root| root.join(path)))
        .unwrap_or_else(|| path.to_path_buf())
}

fn starts_with_ferrus_dir(path: &Path) -> bool {
    path.components()
        .next()
        .and_then(|component| match component {
            std::path::Component::Normal(value) => value.to_str(),
            _ => None,
        })
        == Some(FERRUS_DIR)
}

fn project_root_from_env() -> Option<PathBuf> {
    std::env::var(ENV_PROJECT_ROOT)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn canonical_project_root_from_local_project_ref() -> Option<PathBuf> {
    let contents = std::fs::read_to_string(LOCAL_PROJECT_TOML).ok()?;
    let local_ref = toml::from_str::<LocalProjectRef>(&contents).ok()?;
    let metadata_path = Path::new(&local_ref.data_dir).join("project.toml");
    let metadata = std::fs::read_to_string(metadata_path).ok()?;
    let metadata = toml::from_str::<ProjectMetadata>(&metadata).ok()?;
    Some(PathBuf::from(metadata.workspace_dir))
}

pub async fn read_task() -> Result<String> {
    read_file("TASK.md").await
}

pub async fn read_task_template() -> Result<String> {
    read_file("TASK.md").await
}

pub async fn read_task_at(task_path: &str) -> Result<String> {
    read_path(Path::new(task_path)).await
}

#[allow(dead_code)]
pub async fn clear_task_mirror() -> Result<()> {
    Ok(())
}

pub async fn read_review_for_run_dir(run_dir: &str) -> Result<String> {
    read_path_or_empty(&Path::new(run_dir).join("REVIEW.md")).await
}

pub async fn write_review_for_run_dir(run_dir: &str, content: &str) -> Result<()> {
    write_path(&run_file(run_dir, "REVIEW.md"), content).await
}

pub async fn read_submission_for_run_dir(run_dir: &str) -> Result<String> {
    read_path_or_empty(&run_file(run_dir, "SUBMISSION.md")).await
}

pub async fn write_submission_for_run_dir(run_dir: &str, content: &str) -> Result<()> {
    write_path(&run_file(run_dir, "SUBMISSION.md"), content).await
}

pub async fn read_patch_for_run_dir(run_dir: &str) -> Result<String> {
    read_path_or_empty(&run_file(run_dir, "PATCH.diff")).await
}

pub async fn write_patch_for_run_dir(run_dir: &str, content: &str) -> Result<()> {
    write_path(&run_file(run_dir, "PATCH.diff"), content).await
}

pub async fn clear_patch_for_run_dir(run_dir: &str) -> Result<()> {
    remove_path_if_exists(&run_file(run_dir, "PATCH.diff")).await
}

pub async fn read_integration_error_for_run_dir(run_dir: &str) -> Result<String> {
    read_path_or_empty(&run_file(run_dir, "INTEGRATION_ERROR.md")).await
}

pub async fn write_integration_error_for_run_dir(run_dir: &str, content: &str) -> Result<()> {
    write_path(&run_file(run_dir, "INTEGRATION_ERROR.md"), content).await
}

pub async fn clear_integration_error_for_run_dir(run_dir: &str) -> Result<()> {
    remove_path_if_exists(&run_file(run_dir, "INTEGRATION_ERROR.md")).await
}

pub async fn clear_scoped_task_artifacts(task_path: &str, run_dir: &str) -> Result<()> {
    remove_path_if_exists(Path::new(task_path)).await?;
    remove_dir_if_exists(Path::new(run_dir)).await
}

/// Write a full check log to `.ferrus/logs/check_{attempt}_{scope}_{ts}.txt`.
/// Creates the logs directory if it doesn't exist. Returns the file path.
pub async fn write_check_log(attempt: u32, ts: u64, scope: &str, content: &str) -> Result<PathBuf> {
    let logs_dir = resolve_project_path(LOGS_DIR);
    tokio::fs::create_dir_all(&logs_dir)
        .await
        .with_context(|| format!("Failed to create {}", logs_dir.display()))?;
    let filename = format!("check_{attempt}_{}_{ts}.txt", sanitize_log_component(scope));
    let p = logs_dir.join(&filename);
    tokio::fs::write(&p, content)
        .await
        .with_context(|| format!("Failed to write {}", p.display()))?;
    Ok(p)
}

fn sanitize_log_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

#[allow(dead_code)]
pub async fn clear_review_mirror() -> Result<()> {
    write_file("REVIEW.md", "").await
}

#[allow(dead_code)]
pub async fn clear_submission_mirror() -> Result<()> {
    write_file("SUBMISSION.md", "").await
}

pub async fn write_question_for_run_dir(run_dir: &str, content: &str) -> Result<()> {
    write_path(&run_file(run_dir, "QUESTION.md"), content).await
}

pub async fn read_question_for_run_dir(run_dir: &str) -> Result<String> {
    read_path(&run_file(run_dir, "QUESTION.md")).await
}

pub async fn clear_question_for_run_dir(run_dir: &str) -> Result<()> {
    write_path(&run_file(run_dir, "QUESTION.md"), "").await
}

pub async fn read_answer_for_run_dir(run_dir: &str) -> Result<String> {
    read_path(&run_file(run_dir, "ANSWER.md")).await
}

pub async fn write_answer_for_run_dir(run_dir: &str, content: &str) -> Result<()> {
    write_path(&run_file(run_dir, "ANSWER.md"), content).await
}

pub async fn clear_answer_for_run_dir(run_dir: &str) -> Result<()> {
    write_path(&run_file(run_dir, "ANSWER.md"), "").await
}

pub async fn write_consult_request_for_run_dir(run_dir: &str, content: &str) -> Result<()> {
    write_path(&run_file(run_dir, "CONSULT_REQUEST.md"), content).await
}

pub async fn read_consult_request_for_run_dir(run_dir: &str) -> Result<String> {
    read_path(&run_file(run_dir, "CONSULT_REQUEST.md")).await
}

pub async fn clear_consult_request_for_run_dir(run_dir: &str) -> Result<()> {
    write_path(&run_file(run_dir, "CONSULT_REQUEST.md"), "").await
}

#[allow(dead_code)]
pub async fn clear_consult_request_mirror() -> Result<()> {
    write_file("CONSULT_REQUEST.md", "").await
}

pub async fn read_consult_response_for_run_dir(run_dir: &str) -> Result<String> {
    read_path(&run_file(run_dir, "CONSULT_RESPONSE.md")).await
}

pub async fn write_consult_response_for_run_dir(run_dir: &str, content: &str) -> Result<()> {
    write_path(&run_file(run_dir, "CONSULT_RESPONSE.md"), content).await
}

pub async fn clear_consult_response_for_run_dir(run_dir: &str) -> Result<()> {
    write_path(&run_file(run_dir, "CONSULT_RESPONSE.md"), "").await
}

#[allow(dead_code)]
pub async fn clear_consult_response_mirror() -> Result<()> {
    write_file("CONSULT_RESPONSE.md", "").await
}

#[allow(dead_code)]
pub async fn clear_question_mirror() -> Result<()> {
    write_file("QUESTION.md", "").await
}

#[allow(dead_code)]
pub async fn clear_answer_mirror() -> Result<()> {
    write_file("ANSWER.md", "").await
}

async fn read_file(filename: &str) -> Result<String> {
    let p = path(filename);
    read_path(&p).await
}

async fn write_file(filename: &str, content: &str) -> Result<()> {
    let p = path(filename);
    write_path(&p, content).await
}

async fn read_path(path: &Path) -> Result<String> {
    let path = resolve_project_path(path);
    tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("Failed to read {}", path.display()))
}

async fn read_path_or_empty(path: &Path) -> Result<String> {
    let path = resolve_project_path(path);
    match tokio::fs::read_to_string(&path).await {
        Ok(contents) => Ok(contents),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(err).with_context(|| format!("Failed to read {}", path.display())),
    }
}

async fn write_path(path: &Path, content: &str) -> Result<()> {
    let path = resolve_project_path(path);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    tokio::fs::write(&path, content)
        .await
        .with_context(|| format!("Failed to write {}", path.display()))
}

async fn remove_path_if_exists(path: &Path) -> Result<()> {
    let path = resolve_project_path(path);
    match tokio::fs::remove_file(&path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("Failed to remove {}", path.display())),
    }
}

async fn remove_dir_if_exists(path: &Path) -> Result<()> {
    let path = resolve_project_path(path);
    match tokio::fs::remove_dir_all(&path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("Failed to remove {}", path.display())),
    }
}

fn run_file(run_dir: &str, filename: &str) -> PathBuf {
    Path::new(run_dir).join(filename)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        (dir, previous)
    }

    fn teardown(previous: PathBuf) {
        std::env::set_current_dir(previous).unwrap();
    }

    #[tokio::test]
    async fn scoped_artifacts_are_written_without_rewriting_task_template() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        write_file("TASK.md", "task template").await.unwrap();
        write_path(Path::new(".ferrus/tasks/t-001.md"), "task body")
            .await
            .unwrap();
        write_review_for_run_dir(".ferrus/runs/t-001", "review body")
            .await
            .unwrap();
        write_submission_for_run_dir(".ferrus/runs/t-001", "submission body")
            .await
            .unwrap();
        write_question_for_run_dir(".ferrus/runs/t-001", "question body")
            .await
            .unwrap();
        write_answer_for_run_dir(".ferrus/runs/t-001", "answer body")
            .await
            .unwrap();
        write_consult_request_for_run_dir(".ferrus/runs/t-001", "consult request body")
            .await
            .unwrap();
        write_consult_response_for_run_dir(".ferrus/runs/t-001", "consult response body")
            .await
            .unwrap();

        assert_eq!(
            read_task_at(".ferrus/tasks/t-001.md").await.unwrap(),
            "task body"
        );
        assert_eq!(
            read_review_for_run_dir(".ferrus/runs/t-001").await.unwrap(),
            "review body"
        );
        assert_eq!(
            read_submission_for_run_dir(".ferrus/runs/t-001")
                .await
                .unwrap(),
            "submission body"
        );
        assert_eq!(
            read_question_for_run_dir(".ferrus/runs/t-001")
                .await
                .unwrap(),
            "question body"
        );
        assert_eq!(
            read_answer_for_run_dir(".ferrus/runs/t-001").await.unwrap(),
            "answer body"
        );
        assert_eq!(
            read_consult_request_for_run_dir(".ferrus/runs/t-001")
                .await
                .unwrap(),
            "consult request body"
        );
        assert_eq!(
            read_consult_response_for_run_dir(".ferrus/runs/t-001")
                .await
                .unwrap(),
            "consult response body"
        );
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/TASK.md").await.unwrap(),
            "task template"
        );
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/tasks/t-001.md")
                .await
                .unwrap(),
            "task body"
        );
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-001/REVIEW.md")
                .await
                .unwrap(),
            "review body"
        );
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-001/SUBMISSION.md")
                .await
                .unwrap(),
            "submission body"
        );
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-001/QUESTION.md")
                .await
                .unwrap(),
            "question body"
        );
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-001/ANSWER.md")
                .await
                .unwrap(),
            "answer body"
        );
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-001/CONSULT_REQUEST.md")
                .await
                .unwrap(),
            "consult request body"
        );
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-001/CONSULT_RESPONSE.md")
                .await
                .unwrap(),
            "consult response body"
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn check_logs_include_scope_to_avoid_parallel_overwrites() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;

        let first = write_check_log(1, 123, "r:first", "first log")
            .await
            .unwrap();
        let second = write_check_log(1, 123, "r:second", "second log")
            .await
            .unwrap();

        assert_ne!(first, second);
        assert_eq!(
            first.file_name().and_then(|name| name.to_str()),
            Some("check_1_r_first_123.txt")
        );
        assert_eq!(
            second.file_name().and_then(|name| name.to_str()),
            Some("check_1_r_second_123.txt")
        );
        assert_eq!(tokio::fs::read_to_string(first).await.unwrap(), "first log");
        assert_eq!(
            tokio::fs::read_to_string(second).await.unwrap(),
            "second log"
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn scoped_artifact_reads_do_not_depend_on_active_state() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;

        write_path(Path::new(".ferrus/tasks/t-002.md"), "second task")
            .await
            .unwrap();
        write_path(Path::new(".ferrus/runs/t-002/REVIEW.md"), "second review")
            .await
            .unwrap();

        assert_eq!(
            read_task_at(".ferrus/tasks/t-002.md").await.unwrap(),
            "second task"
        );
        assert_eq!(
            read_review_for_run_dir(".ferrus/runs/t-002").await.unwrap(),
            "second review"
        );
        assert_eq!(
            read_review_for_run_dir(".ferrus/runs/t-missing")
                .await
                .unwrap(),
            ""
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn worktree_project_pointer_resolves_scoped_artifacts_to_canonical_workspace() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        let canonical = dir.path().join("canonical");
        let worktree = dir.path().join("worktree");
        let data_dir = dir.path().join("runtime");
        std::fs::create_dir_all(canonical.join(".ferrus/tasks")).unwrap();
        std::fs::create_dir_all(worktree.join(".ferrus")).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        let local_ref = LocalProjectRef {
            data_dir: data_dir.to_string_lossy().into_owned(),
        };
        std::fs::write(
            worktree.join(".ferrus/project.toml"),
            toml::to_string(&local_ref).unwrap(),
        )
        .unwrap();
        let metadata = ProjectMetadata {
            workspace_dir: canonical.to_string_lossy().into_owned(),
        };
        std::fs::write(
            data_dir.join("project.toml"),
            toml::to_string(&metadata).unwrap(),
        )
        .unwrap();
        tokio::fs::write(canonical.join(".ferrus/tasks/t-002.md"), "canonical task")
            .await
            .unwrap();
        std::env::set_current_dir(&worktree).unwrap();

        assert_eq!(
            resolve_project_path(".ferrus/tasks/t-002.md"),
            canonical.join(".ferrus/tasks/t-002.md")
        );
        assert_eq!(
            read_task_at(".ferrus/tasks/t-002.md").await.unwrap(),
            "canonical task"
        );

        teardown(previous);
    }
}
