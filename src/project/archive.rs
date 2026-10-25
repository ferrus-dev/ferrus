//! Specification archive manifests and filesystem staging.

use super::*;

#[derive(Debug, Serialize)]
pub(super) struct SpecArchiveManifest {
    pub(super) spec_path: String,
    pub(super) archived_at: String,
    pub(super) tasks: Vec<SpecArchiveTaskManifest>,
}

#[derive(Debug, Serialize)]
pub(super) struct SpecArchiveTaskManifest {
    pub(super) id: String,
    pub(super) status: String,
    pub(super) milestone_id: Option<String>,
    pub(super) original_task_path: String,
    pub(super) archived_task_path: String,
    pub(super) original_run_dir: String,
    pub(super) archived_run_dir: String,
}

impl SpecArchiveManifest {
    pub(super) fn new(spec_path: &str, archived_at: &str, tasks: &[TaskRecord]) -> Self {
        let tasks = tasks
            .iter()
            .map(|task| SpecArchiveTaskManifest {
                id: task.id.clone(),
                status: task.status.clone(),
                milestone_id: task.milestone_id.clone(),
                original_task_path: task.path.clone(),
                archived_task_path: format!("tasks/{}.md", task.id),
                original_run_dir: run_dir_for_task(&task.id),
                archived_run_dir: format!("runs/{}", task.id),
            })
            .collect();
        Self {
            spec_path: spec_path.to_string(),
            archived_at: archived_at.to_string(),
            tasks,
        }
    }
}

pub(super) async fn unique_spec_archive_dir(
    data_dir: &Path,
    spec_path: &str,
    closed_at: &str,
) -> Result<PathBuf> {
    let slug = spec_archive_slug(spec_path);
    let safe_time = closed_at.replace([':', '-'], "").replace('T', "-");
    let base = data_dir
        .join("archive")
        .join("specs")
        .join(format!("{slug}-{safe_time}"));
    let mut candidate = base.clone();
    let mut suffix = 2u32;
    while tokio::fs::metadata(&candidate).await.is_ok() {
        candidate = data_dir
            .join("archive")
            .join("specs")
            .join(format!("{slug}-{safe_time}-{suffix}"));
        suffix += 1;
    }

    Ok(candidate)
}

pub(super) fn spec_archive_slug(spec_path: &str) -> String {
    let stem = Path::new(spec_path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("spec");
    let mut slug = String::new();
    let mut previous_dash = false;
    for ch in stem.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            previous_dash = false;
        } else if !previous_dash && !slug.is_empty() {
            slug.push('-');
            previous_dash = true;
        }
    }

    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "spec".to_string()
    } else {
        slug.to_string()
    }
}

pub(super) fn stage_spec_archive_files(
    archive_dir: &Path,
    spec_path: &str,
    tasks: &[TaskRecord],
    manifest: &SpecArchiveManifest,
) -> Result<(usize, usize)> {
    let tasks_dir = archive_dir.join("tasks");
    let runs_dir = archive_dir.join("runs");
    std::fs::create_dir_all(&tasks_dir)
        .with_context(|| format!("Failed to create {}", tasks_dir.display()))?;
    std::fs::create_dir_all(&runs_dir)
        .with_context(|| format!("Failed to create {}", runs_dir.display()))?;

    std::fs::copy(spec_path, archive_dir.join("spec.md")).with_context(|| {
        format!(
            "Failed to copy spec {} to {}",
            spec_path,
            archive_dir.join("spec.md").display()
        )
    })?;

    let manifest_text =
        toml::to_string_pretty(manifest).context("Failed to render archive manifest")?;
    std::fs::write(archive_dir.join("manifest.toml"), manifest_text).with_context(|| {
        format!(
            "Failed to write archive manifest {}",
            archive_dir.join("manifest.toml").display()
        )
    })?;

    let mut archived_tasks = 0usize;
    let mut archived_runs = 0usize;
    for task in tasks {
        let task_path = Path::new(&task.path);
        if registry::checkout_task_artifact_path(task_path) && task_path.exists() {
            copy_path_recursive(task_path, &tasks_dir.join(format!("{}.md", task.id)))?;
            archived_tasks += 1;
        }

        let run_dir = PathBuf::from(run_dir_for_task(&task.id));
        if registry::checkout_task_artifact_path(task_path) && run_dir.exists() {
            copy_path_recursive(&run_dir, &runs_dir.join(&task.id))?;
            archived_runs += 1;
        }
    }

    Ok((archived_tasks, archived_runs))
}

pub(super) fn cleanup_checkout_archive_artifacts(tasks: &[TaskRecord]) -> Result<()> {
    for task in tasks {
        let task_path = Path::new(&task.path);
        if registry::checkout_task_artifact_path(task_path) && task_path.exists() {
            remove_path_recursive(task_path)?;
        }

        let run_dir = PathBuf::from(run_dir_for_task(&task.id));
        if registry::checkout_task_artifact_path(task_path) && run_dir.exists() {
            remove_path_recursive(&run_dir)?;
        }
    }

    Ok(())
}

pub(super) fn copy_path_recursive(from: &Path, to: &Path) -> Result<()> {
    let metadata =
        std::fs::metadata(from).with_context(|| format!("Failed to inspect {}", from.display()))?;
    if metadata.is_dir() {
        std::fs::create_dir_all(to)
            .with_context(|| format!("Failed to create {}", to.display()))?;
        for entry in
            std::fs::read_dir(from).with_context(|| format!("Failed to read {}", from.display()))?
        {
            let entry = entry?;
            copy_path_recursive(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else {
        std::fs::copy(from, to)
            .with_context(|| format!("Failed to copy {} to {}", from.display(), to.display()))?;
    }

    Ok(())
}

pub(super) fn remove_path_recursive(path: &Path) -> Result<()> {
    let metadata =
        std::fs::metadata(path).with_context(|| format!("Failed to inspect {}", path.display()))?;
    if metadata.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
    .with_context(|| format!("Failed to remove {}", path.display()))
}
