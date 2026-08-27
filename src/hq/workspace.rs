use super::*;

#[derive(Debug, Clone)]
pub(super) struct ExecutorWorkspace {
    pub(super) project_root: PathBuf,
    pub(super) workspace_dir: PathBuf,
    pub(super) baseline_tree: Option<String>,
}

pub(super) async fn prepare_executor_workspace(task_id: &str) -> Result<ExecutorWorkspace> {
    let registration = crate::project::touch_current_project().await?;
    let project_root = PathBuf::from(&registration.metadata.workspace_dir);
    if !git_is_work_tree(&project_root).await {
        return Ok(ExecutorWorkspace {
            workspace_dir: project_root.clone(),
            project_root,
            baseline_tree: None,
        });
    }

    let workspace_dir = registration.data_dir.join("worktrees").join(task_id);
    let baseline_path = executor_workspace_baseline_path(&registration.data_dir, task_id);
    if tokio::fs::try_exists(&workspace_dir).await? {
        if git_is_work_tree(&workspace_dir).await {
            copy_canonical_agent_config_files(&project_root, &workspace_dir).await?;
            let mut baseline_tree = read_executor_workspace_baseline_tree(&baseline_path).await?;
            if baseline_tree.is_none() {
                let captured = capture_executor_workspace_baseline_tree(&workspace_dir).await?;
                persist_executor_workspace_baseline(
                    &project_root,
                    &registration.data_dir,
                    &baseline_path,
                    task_id,
                    &captured,
                )
                .await?;
                baseline_tree = Some(captured);
            }
            if let Some(baseline_tree) = baseline_tree.as_deref() {
                crate::project::pin_executor_baseline_tree(&project_root, task_id, baseline_tree)
                    .await?;
            }
            return Ok(ExecutorWorkspace {
                project_root,
                workspace_dir,
                baseline_tree,
            });
        }
        anyhow::bail!(
            "Cannot start isolated executor workspace: {} already exists and is not a git worktree.",
            workspace_dir.display()
        );
    }

    let parent = workspace_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("workspace path has no parent"))?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("Failed to create {}", parent.display()))?;

    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(&project_root)
        .args(["worktree", "add"]);
    if git_has_head(&project_root).await {
        command.args(["--detach"]).arg(&workspace_dir).arg("HEAD");
    } else {
        command.arg("--orphan").arg(&workspace_dir);
    }
    let output = command
        .output()
        .await
        .context("Failed to run git worktree add")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "Failed to create isolated executor workspace at {}: {}",
            workspace_dir.display(),
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        );
    }
    seed_executor_workspace_from_canonical_changes(&project_root, &workspace_dir).await?;
    let baseline_tree = capture_executor_workspace_baseline_tree(&workspace_dir).await?;
    persist_executor_workspace_baseline(
        &project_root,
        &registration.data_dir,
        &baseline_path,
        task_id,
        &baseline_tree,
    )
    .await?;

    Ok(ExecutorWorkspace {
        project_root,
        workspace_dir,
        baseline_tree: Some(baseline_tree),
    })
}

async fn seed_executor_workspace_from_canonical_changes(
    project_root: &Path,
    workspace_dir: &Path,
) -> Result<()> {
    if git_has_head(project_root).await {
        apply_canonical_tracked_diff(project_root, workspace_dir).await?;
    } else {
        copy_canonical_index_files(project_root, workspace_dir).await?;
    }
    copy_canonical_untracked_files(project_root, workspace_dir).await?;
    copy_canonical_agent_config_files(project_root, workspace_dir).await
}

pub(super) fn executor_workspace_baseline_path(data_dir: &Path, task_id: &str) -> PathBuf {
    data_dir
        .join("worktrees")
        .join(".baseline-trees")
        .join(format!("{task_id}.txt"))
}

async fn read_executor_workspace_baseline_tree(path: &Path) -> Result<Option<String>> {
    if !tokio::fs::try_exists(path).await? {
        return Ok(None);
    }
    let value = tokio::fs::read_to_string(path).await.with_context(|| {
        format!(
            "Failed to read executor workspace baseline {}",
            path.display()
        )
    })?;
    Ok(Some(value.trim().to_string()).filter(|value| !value.is_empty()))
}

async fn write_executor_workspace_baseline_tree(path: &Path, baseline_tree: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    tokio::fs::write(path, format!("{baseline_tree}\n"))
        .await
        .with_context(|| {
            format!(
                "Failed to write executor workspace baseline {}",
                path.display()
            )
        })
}

async fn persist_executor_workspace_baseline(
    project_root: &Path,
    data_dir: &Path,
    path: &Path,
    task_id: &str,
    baseline_tree: &str,
) -> Result<()> {
    crate::project::pin_executor_baseline_tree(project_root, task_id, baseline_tree).await?;
    if let Err(err) = write_executor_workspace_baseline_tree(path, baseline_tree).await {
        let _ = crate::project::remove_executor_baseline(project_root, data_dir, task_id).await;
        return Err(err);
    }
    Ok(())
}

async fn capture_executor_workspace_baseline_tree(workspace_dir: &Path) -> Result<String> {
    let workspace_dir = workspace_dir.to_path_buf();
    let tree = tokio::task::spawn_blocking(move || {
        crate::repository_graph::source::capture_worktree_tree(workspace_dir)
    })
    .await??;
    Ok(tree.value().to_string())
}

async fn apply_canonical_tracked_diff(project_root: &Path, workspace_dir: &Path) -> Result<()> {
    let diff = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["diff", "--binary", "HEAD", "--"])
        .output()
        .await
        .context("Failed to capture canonical workspace diff")?;
    if !diff.status.success() {
        let stderr = String::from_utf8_lossy(&diff.stderr).trim().to_string();
        anyhow::bail!(
            "Failed to capture canonical workspace diff from {}: {}",
            project_root.display(),
            if stderr.is_empty() {
                diff.status.to_string()
            } else {
                stderr
            }
        );
    }
    if diff.stdout.is_empty() {
        return Ok(());
    }

    let mut apply = Command::new("git")
        .arg("-C")
        .arg(workspace_dir)
        .args(["apply", "--whitespace=nowarn"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "Failed to start git apply for executor workspace {}",
                workspace_dir.display()
            )
        })?;
    if let Some(mut stdin) = apply.stdin.take() {
        use tokio::io::AsyncWriteExt;

        stdin
            .write_all(&diff.stdout)
            .await
            .context("Failed to stream canonical workspace diff")?;
    }
    let output = apply
        .wait_with_output()
        .await
        .context("Failed to apply canonical workspace diff")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "Failed to seed executor workspace {} with approved canonical changes: {}",
            workspace_dir.display(),
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        );
    }
    Ok(())
}

async fn copy_canonical_untracked_files(project_root: &Path, workspace_dir: &Path) -> Result<()> {
    copy_canonical_listed_files(
        project_root,
        workspace_dir,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        "untracked",
        false,
    )
    .await
}

async fn copy_canonical_index_files(project_root: &Path, workspace_dir: &Path) -> Result<()> {
    copy_canonical_listed_files(
        project_root,
        workspace_dir,
        &["ls-files", "--cached", "-z"],
        "index",
        true,
    )
    .await
}

async fn copy_canonical_listed_files(
    project_root: &Path,
    workspace_dir: &Path,
    arguments: &[&str],
    source_kind: &str,
    skip_missing: bool,
) -> Result<()> {
    let workspace_dir = tokio::fs::canonicalize(workspace_dir)
        .await
        .unwrap_or_else(|_| workspace_dir.to_path_buf());
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(arguments)
        .output()
        .await
        .with_context(|| format!("Failed to list canonical {source_kind} files"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "Failed to list canonical {source_kind} files from {}: {}",
            project_root.display(),
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        );
    }

    for relative in output.stdout.split(|byte| *byte == 0) {
        if relative.is_empty() {
            continue;
        }
        let relative = PathBuf::from(String::from_utf8_lossy(relative).into_owned());
        let source = project_root.join(&relative);
        let canonical_source = tokio::fs::canonicalize(&source)
            .await
            .unwrap_or_else(|_| source.clone());
        if canonical_source.starts_with(&workspace_dir) {
            continue;
        }
        let metadata = match tokio::fs::symlink_metadata(&source).await {
            Ok(metadata) => metadata,
            Err(error) if skip_missing && error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to stat {}", source.display()));
            }
        };
        if metadata.is_dir() {
            continue;
        }
        let destination = workspace_dir.join(&relative);
        copy_canonical_entry(&source, &destination, metadata).await?;
    }
    Ok(())
}

async fn copy_canonical_entry(
    source: &Path,
    destination: &Path,
    source_metadata: std::fs::Metadata,
) -> Result<()> {
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    if source_metadata.file_type().is_symlink() {
        let target = tokio::fs::read_link(source)
            .await
            .with_context(|| format!("Failed to read symlink {}", source.display()))?;
        remove_existing_file_or_symlink(destination).await?;
        recreate_symlink(
            target,
            destination.to_path_buf(),
            source_metadata.file_type(),
        )
        .await?;
        return Ok(());
    }
    if tokio::fs::symlink_metadata(destination)
        .await
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        remove_existing_file_or_symlink(destination).await?;
    }
    tokio::fs::copy(source, destination)
        .await
        .with_context(|| {
            format!(
                "Failed to copy canonical file {} to {}",
                source.display(),
                destination.display()
            )
        })?;
    Ok(())
}

async fn remove_existing_file_or_symlink(path: &Path) -> Result<()> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to stat {}", path.display()));
        }
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        anyhow::bail!("Refusing to replace directory {}", path.display());
    }
    #[cfg(windows)]
    if {
        use std::os::windows::fs::FileTypeExt;
        metadata.file_type().is_symlink_dir()
    } {
        return tokio::fs::remove_dir(path)
            .await
            .with_context(|| format!("Failed to remove symlink {}", path.display()));
    }
    tokio::fs::remove_file(path)
        .await
        .with_context(|| format!("Failed to remove file or symlink {}", path.display()))
}

#[cfg(unix)]
async fn recreate_symlink(
    target: PathBuf,
    destination: PathBuf,
    _file_type: std::fs::FileType,
) -> Result<()> {
    let displayed = destination.clone();
    tokio::task::spawn_blocking(move || std::os::unix::fs::symlink(target, destination))
        .await
        .context("Failed to join symlink creation task")?
        .with_context(|| format!("Failed to create symlink {}", displayed.display()))
}

#[cfg(windows)]
async fn recreate_symlink(
    target: PathBuf,
    destination: PathBuf,
    file_type: std::fs::FileType,
) -> Result<()> {
    use std::os::windows::fs::FileTypeExt;

    let displayed = destination.clone();
    tokio::task::spawn_blocking(move || {
        if file_type.is_symlink_dir() {
            std::os::windows::fs::symlink_dir(target, destination)
        } else {
            std::os::windows::fs::symlink_file(target, destination)
        }
    })
    .await
    .context("Failed to join symlink creation task")?
    .with_context(|| format!("Failed to create symlink {}", displayed.display()))
}

async fn copy_canonical_agent_config_files(
    project_root: &Path,
    workspace_dir: &Path,
) -> Result<()> {
    for relative in [
        ".ferrus/project.toml",
        ".claude/mcp-supervisor.json",
        ".claude/mcp-executor.json",
        ".claude/settings.local.json",
        ".codex/config.toml",
        ".qwen/settings.json",
        "opencode.json",
    ] {
        copy_canonical_file_if_present(project_root, workspace_dir, Path::new(relative)).await?;
    }
    Ok(())
}

async fn copy_canonical_file_if_present(
    project_root: &Path,
    workspace_dir: &Path,
    relative: &Path,
) -> Result<()> {
    let source = project_root.join(relative);
    let Ok(metadata) = tokio::fs::symlink_metadata(&source).await else {
        return Ok(());
    };
    if metadata.is_dir() {
        return Ok(());
    }

    let destination = workspace_dir.join(relative);
    copy_canonical_entry(&source, &destination, metadata).await
}

pub(super) async fn git_is_work_tree(path: &Path) -> bool {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .await;
    matches!(output, Ok(output) if output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true")
}

async fn git_has_head(path: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .await
        .is_ok_and(|output| output.status.success())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn untracked_symlinks_are_recreated_without_following_targets() {
        use std::os::unix::fs::symlink;

        let canonical = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(canonical.path())
            .status()
            .unwrap();
        std::fs::write(canonical.path().join("target.txt"), "target").unwrap();
        std::fs::create_dir(canonical.path().join("target-dir")).unwrap();
        symlink("target.txt", canonical.path().join("file-link")).unwrap();
        symlink("target-dir", canonical.path().join("dir-link")).unwrap();

        copy_canonical_untracked_files(canonical.path(), workspace.path())
            .await
            .unwrap();

        for (name, target) in [("file-link", "target.txt"), ("dir-link", "target-dir")] {
            let copied = workspace.path().join(name);
            assert!(
                std::fs::symlink_metadata(&copied)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(std::fs::read_link(copied).unwrap(), PathBuf::from(target));
        }
    }

    #[tokio::test]
    async fn agent_config_copy_never_writes_through_a_seeded_symlink() {
        use std::os::unix::fs::symlink;

        let canonical = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let shared = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(shared.path(), "shared config\n").unwrap();
        std::fs::create_dir_all(canonical.path().join(".codex")).unwrap();
        std::fs::create_dir_all(workspace.path().join(".codex")).unwrap();
        symlink(shared.path(), canonical.path().join(".codex/config.toml")).unwrap();
        symlink(shared.path(), workspace.path().join(".codex/config.toml")).unwrap();

        copy_canonical_agent_config_files(canonical.path(), workspace.path())
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(shared.path()).unwrap(),
            "shared config\n"
        );
        let copied = workspace.path().join(".codex/config.toml");
        assert!(
            std::fs::symlink_metadata(&copied)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_link(copied).unwrap(), shared.path());
    }
}
