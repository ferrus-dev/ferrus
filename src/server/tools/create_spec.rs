use anyhow::{Context, Result};
use neva::prelude::*;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tracing::info;

use crate::{config::Config, project, specs};

use super::tool_err;

pub const DESCRIPTION: &str = "Create an approved feature specification. Writes Markdown to the \
     configured [spec].directory and records the created path for Ferrus HQ. Must only be called \
     after explicit user approval of the final spec text.";

pub const INPUT_SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "markdown": {
            "type": "string",
            "description": "Full approved feature specification in Markdown, following ferrus://spec_template"
        }
    },
    "required": ["markdown"]
}"#;

pub async fn handler(markdown: String) -> Result<String, Error> {
    run(markdown).await.map_err(tool_err)
}

async fn run(markdown: String) -> Result<String> {
    project::clear_last_spec_path().await?;

    if markdown.trim().is_empty() {
        anyhow::bail!("Cannot create spec: markdown content is empty.");
    }

    let config = Config::load().await?;
    let directory = config.spec.directory.trim();
    if directory.is_empty() {
        anyhow::bail!("Cannot create spec: ferrus.toml [spec].directory is empty.");
    }

    let spec_dir = PathBuf::from(directory);
    tokio::fs::create_dir_all(&spec_dir)
        .await
        .with_context(|| format!("Failed to create spec directory {}", spec_dir.display()))?;

    let title = extract_title(&markdown).unwrap_or("spec");
    let slug = slugify(title);
    let date = chrono::Utc::now().format("%Y-%m-%d");
    let base_name = format!("{date}-{slug}");

    let content = ensure_trailing_newline(markdown);
    let path = create_unique_spec_file(&spec_dir, &base_name, content.as_bytes()).await?;

    let display_path = storage_path(&path);
    project::write_last_spec_path(&display_path).await?;
    let selection = specs::first_incomplete_selection(&display_path).await?;
    let first_milestone_id = specs::first_incomplete_milestone_id(&display_path).await?;
    project::write_project_selection(&selection).await?;

    info!("Spec created at {}", display_path);
    let selection = first_milestone_id
        .as_deref()
        .map(|id| format!(" First incomplete milestone: {id}."))
        .unwrap_or_else(|| " No incomplete milestone found.".to_string());
    Ok(format!("Spec created at {display_path}.{selection}"))
}

fn extract_title(markdown: &str) -> Option<&str> {
    markdown.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("# ")
            .map(str::trim)
            .filter(|title| !title.is_empty())
    })
}

fn slugify(title: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;

    for ch in title.chars().flat_map(char::to_lowercase) {
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

async fn create_unique_spec_file(dir: &Path, base_name: &str, content: &[u8]) -> Result<PathBuf> {
    let mut candidate = spec_path_candidate(dir, base_name, 1);
    let mut suffix = 2;
    loop {
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
            .await
        {
            Ok(mut file) => {
                file.write_all(content)
                    .await
                    .with_context(|| format!("Failed to write spec {}", candidate.display()))?;
                file.flush()
                    .await
                    .with_context(|| format!("Failed to flush spec {}", candidate.display()))?;
                return Ok(candidate);
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                candidate = spec_path_candidate(dir, base_name, suffix);
                suffix += 1;
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("Failed to create spec {}", candidate.display()));
            }
        }
    }
}

fn spec_path_candidate(dir: &Path, base_name: &str, suffix: u32) -> PathBuf {
    if suffix == 1 {
        dir.join(format!("{base_name}.md"))
    } else {
        dir.join(format!("{base_name}-{suffix}.md"))
    }
}

fn storage_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn ensure_trailing_newline(mut markdown: String) -> String {
    if !markdown.ends_with('\n') {
        markdown.push('\n');
    }
    markdown
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn extracts_first_h1_title() {
        let markdown = "intro\n# Feature Spec\n\n## Goal\n...";
        assert_eq!(extract_title(markdown), Some("Feature Spec"));
    }

    #[test]
    fn slugifies_title_for_filename() {
        assert_eq!(slugify("Ferrus /spec: v2!"), "ferrus-spec-v2");
    }

    #[test]
    fn slugify_falls_back_for_non_ascii_title() {
        assert_eq!(slugify("спека"), "spec");
    }

    #[test]
    fn spec_path_candidate_adds_suffix_after_first_candidate() {
        let dir = Path::new("docs/specs");
        assert_eq!(
            spec_path_candidate(dir, "2026-04-26-feature", 1),
            dir.join("2026-04-26-feature.md")
        );
        assert_eq!(
            spec_path_candidate(dir, "2026-04-26-feature", 2),
            dir.join("2026-04-26-feature-2.md")
        );
    }

    #[tokio::test]
    async fn create_spec_writes_selection_without_state_json() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        let data_dir = dir.path().join(".ferrus/projects/test-project");
        std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        let local_ref = crate::project::LocalProjectRef {
            project_id: "test-project".to_string(),
            name: "test".to_string(),
            data_dir: data_dir.display().to_string(),
        };
        let local_ref = toml::to_string_pretty(&local_ref).unwrap();
        std::fs::write(dir.path().join(".ferrus/project.toml"), local_ref).unwrap();
        std::fs::write(
            dir.path().join("ferrus.toml"),
            r#"
[checks]
commands = []

[limits]
"#,
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let response = run(r#"# Runtime Selection

## Milestones

- [ ] #1.0 first milestone
  - ID: m1.0
  - Depends on: none
"#
        .to_string())
        .await
        .unwrap();

        assert!(response.contains("Spec created at docs/specs/"));
        assert!(response.contains("First incomplete milestone: m1.0."));
        crate::test_support::assert_no_state_json();
        let selection = project::read_project_selection().await.unwrap();
        assert!(
            selection
                .selected_spec
                .as_deref()
                .is_some_and(|path| path.starts_with("docs/specs/"))
        );
        assert!(
            project::read_last_spec_path()
                .await
                .unwrap()
                .unwrap()
                .contains("docs/specs/")
        );

        std::env::set_current_dir(previous).unwrap();
    }
}
