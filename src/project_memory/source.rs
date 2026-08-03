//! Project-scoped local sources for deterministic project-memory ingestion.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;

use crate::repository_graph::domain::{Digest, RepoPath};

use super::{
    documents::{
        ArchiveSourceDocument, RuntimeCheckDocument, RuntimeRunDocument, RuntimeSourceDocument,
        RuntimeTaskDocument, parse_spec_memory,
    },
    domain::{
        AuthorizedSourceDescriptor, AuthorizedSourceManifest, MemoryRecordId, MemorySourceCategory,
        MemorySourceLocator, MemoryStatusToken, ProjectId, ProjectNamespace, ProjectRef,
    },
    extractors::{built_in_extractor_set_digest, canonical_digest},
    policy::MemoryPolicy,
    ports::{MemoryContent, MemorySource, MemorySourceContent},
    query::{MemoryContentRequest, MemoryContentResponse, MemoryQueryError},
};

const MAX_SPEC_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ARCHIVE_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_SOURCES: usize = 100_000;
const MAX_RUNTIME_RECORDS: usize = 100_000;
const MAX_RUNTIME_SOURCE_BYTES: usize = 16 * 1024 * 1024;
const MAX_GIT_LIST_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
enum MaterialContent {
    TrackedSpec {
        absolute_path: PathBuf,
        category: MemorySourceCategory,
    },
    Sanitized(Vec<u8>),
}

#[derive(Debug, Clone)]
struct SourceMaterial {
    descriptor: AuthorizedSourceDescriptor,
    content: MaterialContent,
}

#[derive(Debug, Clone)]
pub struct LocalMemorySource {
    root: PathBuf,
    data_dir: PathBuf,
    project: ProjectRef,
    spec_directory: RepoPath,
    policy: MemoryPolicy,
    materials: Vec<SourceMaterial>,
    manifest: AuthorizedSourceManifest,
}

impl LocalMemorySource {
    /// Resolves the canonical workspace and machine-local data directory from
    /// the registered `.ferrus/project.toml` pointer without mutating runtime
    /// state.
    pub async fn discover_current() -> Result<Self> {
        #[derive(Deserialize)]
        struct LocalProjectRef {
            project_id: String,
            data_dir: String,
        }
        #[derive(Deserialize)]
        struct ProjectMetadata {
            id: String,
            workspace_dir: String,
        }

        let pointer: LocalProjectRef = toml::from_str(
            &tokio::fs::read_to_string(".ferrus/project.toml")
                .await
                .context(".ferrus/project.toml not found or invalid")?,
        )?;
        let data_dir = tokio::fs::canonicalize(&pointer.data_dir)
            .await
            .context("registered project data directory is unavailable")?;
        let metadata: ProjectMetadata =
            toml::from_str(&tokio::fs::read_to_string(data_dir.join("project.toml")).await?)?;
        if metadata.id != pointer.project_id {
            anyhow::bail!("registered project identity does not match project metadata");
        }
        let root = tokio::fs::canonicalize(metadata.workspace_dir).await?;
        let config = tokio::fs::read_to_string(root.join("ferrus.toml")).await?;
        let config: toml::Value = toml::from_str(&config)?;
        let spec_directory = config
            .get("spec")
            .and_then(|spec| spec.get("directory"))
            .and_then(toml::Value::as_str)
            .unwrap_or("docs/specs");
        let spec_directory = RepoPath::new(spec_directory)
            .context("[spec].directory must be repository-relative")?;
        let project = ProjectRef {
            namespace: ProjectNamespace::new("local:ferrus")?,
            project_id: ProjectId::new(pointer.project_id)?,
        };
        tokio::task::spawn_blocking(move || {
            Self::discover_scoped(
                root,
                data_dir,
                project,
                spec_directory,
                MemoryPolicy::default(),
            )
        })
        .await?
    }

    fn discover_scoped(
        root: PathBuf,
        data_dir: PathBuf,
        project: ProjectRef,
        spec_directory: RepoPath,
        policy: MemoryPolicy,
    ) -> Result<Self> {
        let root = fs::canonicalize(root).context("project memory root is unavailable")?;
        let data_dir =
            fs::canonicalize(data_dir).context("project memory data directory is unavailable")?;
        let mut materials = Vec::new();
        if policy.is_authorized(MemorySourceCategory::SpecificationStructure)
            || policy.is_authorized(MemorySourceCategory::ApprovedOutcome)
        {
            discover_tracked_specs(&root, &project, &spec_directory, &policy, &mut materials)?;
        }
        if policy.is_authorized(MemorySourceCategory::ArchiveManifest) {
            discover_archives(&data_dir, &project, &mut materials)?;
        }
        if policy.is_authorized(MemorySourceCategory::RuntimeProvenance) {
            discover_runtime(&data_dir, &project, &mut materials)?;
        }
        if materials.len() > MAX_SOURCES {
            anyhow::bail!("project memory source limit exceeded");
        }
        materials.sort_by_key(material_key);
        let mut manifest = AuthorizedSourceManifest {
            project: project.clone(),
            policy_digest: policy.digest(),
            source_set_digest: Digest::new("sha256", "00")?,
            extractor_set_digest: built_in_extractor_set_digest(),
            sources: materials
                .iter()
                .map(|material| material.descriptor.clone())
                .collect(),
        };
        manifest.source_set_digest = manifest.computed_source_set_digest()?;
        Ok(Self {
            root,
            data_dir,
            project,
            spec_directory,
            policy,
            materials,
            manifest,
        })
    }

    #[cfg(test)]
    pub(crate) fn discover_at(
        root: PathBuf,
        data_dir: PathBuf,
        project: ProjectRef,
        spec_directory: RepoPath,
        policy: MemoryPolicy,
    ) -> Result<Self> {
        Self::discover_scoped(root, data_dir, project, spec_directory, policy)
    }

    pub fn project(&self) -> &ProjectRef {
        &self.project
    }

    pub(super) fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

impl MemorySource for LocalMemorySource {
    type Error = anyhow::Error;

    fn manifest(&self) -> Result<AuthorizedSourceManifest, Self::Error> {
        Ok(self.manifest.clone())
    }

    fn read_verified(
        &self,
        source: &AuthorizedSourceDescriptor,
    ) -> Result<MemorySourceContent, Self::Error> {
        let material = self
            .materials
            .iter()
            .find(|material| material.descriptor == *source)
            .context("authorized project-memory source was not discovered")?;
        let bytes = match &material.content {
            MaterialContent::TrackedSpec {
                absolute_path,
                category,
            } => {
                let metadata = fs::symlink_metadata(absolute_path)?;
                if !metadata.file_type().is_file() || metadata.len() > MAX_SPEC_BYTES {
                    anyhow::bail!("tracked specification is no longer a bounded regular file");
                }
                let bytes = fs::read(absolute_path)?;
                let fingerprint = spec_fingerprint(*category, &bytes)?;
                if fingerprint != source.fingerprint {
                    anyhow::bail!("tracked specification changed after discovery");
                }
                bytes
            }
            MaterialContent::Sanitized(bytes) => {
                if canonical_digest(bytes) != source.fingerprint {
                    anyhow::bail!("sanitized project-memory source changed after discovery");
                }
                bytes.clone()
            }
        };
        Ok(MemorySourceContent { bytes })
    }

    fn revalidate(&self, manifest: &AuthorizedSourceManifest) -> Result<(), Self::Error> {
        let current = Self::discover_scoped(
            self.root.clone(),
            self.data_dir.clone(),
            self.project.clone(),
            self.spec_directory.clone(),
            self.policy.clone(),
        )?;
        if current.manifest != *manifest {
            anyhow::bail!("project-memory sources changed during indexing");
        }
        Ok(())
    }
}

impl MemoryContent for LocalMemorySource {
    fn content(
        &self,
        request: MemoryContentRequest,
    ) -> Result<MemoryContentResponse, MemoryQueryError> {
        if request.project != self.project {
            return Err(MemoryQueryError::SourceNotAuthorized);
        }
        let current_revision = self
            .manifest
            .revision_id()
            .map_err(|_| MemoryQueryError::ContentChanged)?;
        if request.revision_id != current_revision {
            return Err(MemoryQueryError::ContentChanged);
        }
        let material = self
            .materials
            .iter()
            .find(|material| {
                material.descriptor.category == request.source_category
                    && material.descriptor.locator == request.locator
                    && material.descriptor.fingerprint == request.expected_fingerprint
            })
            .ok_or(MemoryQueryError::ContentChanged)?;
        let content = self
            .read_verified(&material.descriptor)
            .map_err(|_| MemoryQueryError::ContentChanged)?;
        let (start, end) = match request.evidence.as_ref() {
            Some(super::domain::MemoryEvidenceLocator::Span(span)) => (
                usize::try_from(span.start.byte_offset)
                    .map_err(|_| MemoryQueryError::ContentChanged)?,
                usize::try_from(span.end.byte_offset)
                    .map_err(|_| MemoryQueryError::ContentChanged)?,
            ),
            _ => (0, content.bytes.len()),
        };
        let bytes = content
            .bytes
            .get(start..end)
            .ok_or(MemoryQueryError::ContentChanged)?;
        let text = std::str::from_utf8(bytes).map_err(|_| {
            MemoryQueryError::Backend(
                super::diagnostics::MemoryDiagnosticCode::new("content.nonutf8")
                    .expect("static diagnostic code is valid"),
            )
        })?;
        let mut length = text.len().min(request.max_bytes.get() as usize);
        while length > 0 && !text.is_char_boundary(length) {
            length -= 1;
        }
        Ok(MemoryContentResponse {
            verified_fingerprint: material.descriptor.fingerprint.clone(),
            bytes: text.as_bytes()[..length].to_vec(),
            truncated: length < text.len(),
        })
    }
}

fn discover_tracked_specs(
    root: &Path,
    project: &ProjectRef,
    spec_directory: &RepoPath,
    policy: &MemoryPolicy,
    materials: &mut Vec<SourceMaterial>,
) -> Result<()> {
    let output = Command::new("git")
        .current_dir(root)
        .args([
            "--literal-pathspecs",
            "ls-files",
            "--cached",
            "-z",
            "--",
            spec_directory.as_str(),
        ])
        .output()
        .context("failed to enumerate tracked specifications")?;
    if !output.status.success() {
        anyhow::bail!("git could not enumerate tracked specifications");
    }
    if output.stdout.len() > MAX_GIT_LIST_BYTES {
        anyhow::bail!("tracked specification listing exceeds the source budget");
    }
    for raw_path in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path =
            std::str::from_utf8(raw_path).context("tracked specification path is not UTF-8")?;
        if !path.ends_with(".md") {
            continue;
        }
        let path = RepoPath::new(path)?;
        let absolute_path = root.join(path.as_str());
        let metadata = fs::symlink_metadata(&absolute_path)?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_SPEC_BYTES {
            continue;
        }
        let absolute_path = fs::canonicalize(absolute_path)?;
        if !absolute_path.starts_with(root) {
            continue;
        }
        let bytes = fs::read(&absolute_path)?;
        let parsed = parse_spec_memory(std::str::from_utf8(&bytes).unwrap_or(""));
        if policy.is_authorized(MemorySourceCategory::SpecificationStructure) {
            materials.push(SourceMaterial {
                descriptor: AuthorizedSourceDescriptor {
                    project: project.clone(),
                    category: MemorySourceCategory::SpecificationStructure,
                    locator: MemorySourceLocator::TrackedFile { path: path.clone() },
                    fingerprint: canonical_digest(&parsed.structure),
                    byte_len: metadata.len(),
                },
                content: MaterialContent::TrackedSpec {
                    absolute_path: absolute_path.clone(),
                    category: MemorySourceCategory::SpecificationStructure,
                },
            });
        }
        if policy.is_authorized(MemorySourceCategory::ApprovedOutcome)
            && let Some(outcome) = parsed.outcome
        {
            materials.push(SourceMaterial {
                descriptor: AuthorizedSourceDescriptor {
                    project: project.clone(),
                    category: MemorySourceCategory::ApprovedOutcome,
                    locator: MemorySourceLocator::TrackedFile { path },
                    fingerprint: canonical_digest(&outcome),
                    byte_len: outcome.text.len() as u64,
                },
                content: MaterialContent::TrackedSpec {
                    absolute_path,
                    category: MemorySourceCategory::ApprovedOutcome,
                },
            });
        }
    }
    Ok(())
}

fn spec_fingerprint(category: MemorySourceCategory, bytes: &[u8]) -> Result<Digest> {
    let content = std::str::from_utf8(bytes).context("tracked specification is not UTF-8")?;
    let parsed = parse_spec_memory(content);
    match category {
        MemorySourceCategory::SpecificationStructure => Ok(canonical_digest(&parsed.structure)),
        MemorySourceCategory::ApprovedOutcome => parsed
            .outcome
            .map(|outcome| canonical_digest(&outcome))
            .context("approved Outcome section is no longer present"),
        _ => anyhow::bail!("unsupported tracked specification source category"),
    }
}

#[derive(Debug, Deserialize)]
struct RawArchiveManifest {
    spec_path: String,
    archived_at: String,
    #[serde(default)]
    tasks: Vec<RawArchiveTask>,
}

#[derive(Debug, Deserialize)]
struct RawArchiveTask {
    id: String,
    milestone_id: Option<String>,
}

fn discover_archives(
    data_dir: &Path,
    project: &ProjectRef,
    materials: &mut Vec<SourceMaterial>,
) -> Result<()> {
    let root = data_dir.join("archive").join("specs");
    let Ok(entries) = fs::read_dir(&root) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let archive_id = entry.file_name().to_string_lossy().to_string();
        MemoryRecordId::new(&archive_id)?;
        let manifest_path = entry.path().join("manifest.toml");
        let metadata = match fs::symlink_metadata(&manifest_path) {
            Ok(metadata)
                if metadata.file_type().is_file()
                    && metadata.len() <= MAX_ARCHIVE_MANIFEST_BYTES =>
            {
                metadata
            }
            _ => continue,
        };
        let raw: RawArchiveManifest = toml::from_str(&fs::read_to_string(&manifest_path)?)?;
        let mut task_ids = raw
            .tasks
            .iter()
            .map(|task| task.id.clone())
            .collect::<Vec<_>>();
        task_ids.sort();
        task_ids.dedup();
        let mut milestone_ids = raw
            .tasks
            .iter()
            .filter_map(|task| task.milestone_id.clone())
            .collect::<Vec<_>>();
        milestone_ids.sort();
        milestone_ids.dedup();
        let document = ArchiveSourceDocument {
            archive_id: archive_id.clone(),
            spec_path: RepoPath::new(raw.spec_path)?,
            archived_at: raw.archived_at,
            task_count: count_regular_files(&entry.path().join("tasks"))?,
            run_count: count_directories(&entry.path().join("runs"))?,
            task_ids,
            milestone_ids,
        };
        let bytes = serde_json::to_vec(&document)?;
        materials.push(SourceMaterial {
            descriptor: AuthorizedSourceDescriptor {
                project: project.clone(),
                category: MemorySourceCategory::ArchiveManifest,
                locator: MemorySourceLocator::ArchiveManifest {
                    archive_id: MemoryRecordId::new(archive_id)?,
                },
                fingerprint: canonical_digest(&bytes),
                byte_len: metadata.len(),
            },
            content: MaterialContent::Sanitized(bytes),
        });
        if materials.len() > MAX_SOURCES {
            anyhow::bail!("project memory source limit exceeded");
        }
    }
    Ok(())
}

fn discover_runtime(
    data_dir: &Path,
    project: &ProjectRef,
    materials: &mut Vec<SourceMaterial>,
) -> Result<()> {
    let database_path = data_dir.join("ferrus.db");
    if !database_path.is_file() {
        return Ok(());
    }
    let connection = Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut tasks = Vec::new();
    let mut task_ids = BTreeSet::new();
    {
        let mut statement = connection.prepare(
            "SELECT id, milestone_id, status, baseline_snapshot_id, \
                    repository_view_snapshot_id FROM tasks \
             WHERE spec_path IS NOT NULL AND status IN ('complete', 'failed') ORDER BY id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        for row in rows {
            let (id, milestone_id, status, baseline_snapshot_id, repository_snapshot_id) = row?;
            let task = RuntimeTaskDocument {
                id,
                milestone_id,
                status,
                baseline_snapshot_id: baseline_snapshot_id
                    .map(crate::repository_graph::domain::SnapshotId::new)
                    .transpose()?,
                repository_snapshot_id: repository_snapshot_id
                    .map(crate::repository_graph::domain::SnapshotId::new)
                    .transpose()?,
            };
            task_ids.insert(task.id.clone());
            tasks.push(task);
            if tasks.len() > MAX_RUNTIME_RECORDS {
                anyhow::bail!("runtime task provenance exceeds the record budget");
            }
        }
    }
    if tasks.is_empty() {
        return Ok(());
    }
    let mut runs = Vec::new();
    {
        let mut statement = connection.prepare(
            "SELECT runs.id, runs.task_id, runs.status, runs.baseline_snapshot_id, \
                    runs.repository_view_snapshot_id FROM runs \
             JOIN tasks ON tasks.id = runs.task_id \
             WHERE tasks.spec_path IS NOT NULL AND tasks.status IN ('complete', 'failed') \
             ORDER BY runs.id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        for row in rows {
            let (id, task_id, status, baseline_snapshot_id, repository_snapshot_id) = row?;
            runs.push(RuntimeRunDocument {
                id,
                task_id,
                status,
                check_ids: Vec::new(),
                baseline_snapshot_id: baseline_snapshot_id
                    .map(crate::repository_graph::domain::SnapshotId::new)
                    .transpose()?,
                repository_snapshot_id: repository_snapshot_id
                    .map(crate::repository_graph::domain::SnapshotId::new)
                    .transpose()?,
            });
            if runs.len() > MAX_RUNTIME_RECORDS {
                anyhow::bail!("runtime run provenance exceeds the record budget");
            }
        }
    }
    let run_tasks = runs
        .iter()
        .map(|run| (run.id.clone(), run.task_id.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut checks = Vec::new();
    {
        #[derive(Deserialize)]
        struct SafeEventPayload {
            task_id: Option<String>,
        }
        let mut statement = connection.prepare(
            "SELECT id, run_id, type, payload_json FROM events \
             WHERE type IN ('task_check_passed', 'task_check_failed', \
                 'task_check_limit_exceeded', 'check_passed', 'check_failed', \
                 'submit_check_failed') ORDER BY id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (id, run_id, event_type, payload) = row?;
            if payload.len() > 64 * 1024 {
                continue;
            }
            let payload = serde_json::from_str::<SafeEventPayload>(&payload).ok();
            let task_id = run_id
                .as_ref()
                .and_then(|run_id| run_tasks.get(run_id))
                .cloned()
                .or_else(|| payload.and_then(|payload| payload.task_id));
            let Some(task_id) = task_id.filter(|task_id| task_ids.contains(task_id)) else {
                continue;
            };
            checks.push(RuntimeCheckDocument {
                id: format!("event:{id}"),
                task_id,
                run_id,
                status: if event_type.contains("passed") {
                    "passed".to_string()
                } else {
                    "failed".to_string()
                },
            });
            if checks.len() > MAX_RUNTIME_RECORDS {
                anyhow::bail!("runtime check provenance exceeds the record budget");
            }
        }
    }
    let checks_by_run = checks
        .iter()
        .filter_map(|check| {
            check
                .run_id
                .as_ref()
                .map(|run_id| (run_id.clone(), check.id.clone()))
        })
        .fold(
            BTreeMap::<String, Vec<String>>::new(),
            |mut map, (run, check)| {
                map.entry(run).or_default().push(check);
                map
            },
        );
    for run in &mut runs {
        run.check_ids = checks_by_run.get(&run.id).cloned().unwrap_or_default();
    }
    let document = RuntimeSourceDocument {
        tasks,
        runs,
        checks,
    };
    let bytes = serde_json::to_vec(&document)?;
    if bytes.len() > MAX_RUNTIME_SOURCE_BYTES {
        anyhow::bail!("runtime provenance exceeds the source byte budget");
    }
    materials.push(SourceMaterial {
        descriptor: AuthorizedSourceDescriptor {
            project: project.clone(),
            category: MemorySourceCategory::RuntimeProvenance,
            locator: MemorySourceLocator::RuntimeRecords {
                record_type: MemoryStatusToken::new("task-run-check-metadata")?,
            },
            fingerprint: canonical_digest(&bytes),
            byte_len: bytes.len() as u64,
        },
        content: MaterialContent::Sanitized(bytes),
    });
    Ok(())
}

fn count_regular_files(path: &Path) -> Result<u64> {
    if !fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir()) {
        return Ok(0);
    }
    let Ok(entries) = fs::read_dir(path) else {
        return Ok(0);
    };
    let mut count = 0u64;
    for entry in entries {
        if entry?.file_type()?.is_file() {
            count += 1;
            if count > MAX_RUNTIME_RECORDS as u64 {
                anyhow::bail!("archive task count exceeds the record budget");
            }
        }
    }
    Ok(count)
}

fn count_directories(path: &Path) -> Result<u64> {
    if !fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir()) {
        return Ok(0);
    }
    let Ok(entries) = fs::read_dir(path) else {
        return Ok(0);
    };
    let mut count = 0u64;
    for entry in entries {
        if entry?.file_type()?.is_dir() {
            count += 1;
            if count > MAX_RUNTIME_RECORDS as u64 {
                anyhow::bail!("archive run count exceeds the record budget");
            }
        }
    }
    Ok(count)
}

fn material_key(material: &SourceMaterial) -> Vec<u8> {
    serde_json::to_vec(&material.descriptor).expect("memory source descriptor is serializable")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;

    use crate::project_memory::domain::MemoryEvidenceLocator;
    use rusqlite::params;
    use tempfile::TempDir;

    fn project_ref() -> ProjectRef {
        ProjectRef {
            namespace: ProjectNamespace::new("local:test").unwrap(),
            project_id: ProjectId::new("project-1").unwrap(),
        }
    }

    fn init_git(root: &Path) {
        assert!(
            Command::new("git")
                .arg("init")
                .arg(root)
                .status()
                .unwrap()
                .success()
        );
        fs::create_dir_all(root.join("docs/specs")).unwrap();
    }

    fn add_tracked(root: &Path, path: &str, content: &str) {
        fs::write(root.join(path), content).unwrap();
        assert!(
            Command::new("git")
                .current_dir(root)
                .args(["add", "--", path])
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn outcome_changes_do_not_invalidate_structure_sources() {
        let root = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        init_git(root.path());
        let path = "docs/specs/example.md";
        add_tracked(
            root.path(),
            path,
            "# Example\n\n- [x] #1.0 Done\n\nID: one\nDepends on: none\n\n## Outcome\n\nFirst.\n",
        );
        let first = LocalMemorySource::discover_at(
            root.path().to_path_buf(),
            data.path().to_path_buf(),
            project_ref(),
            RepoPath::new("docs/specs").unwrap(),
            MemoryPolicy::default(),
        )
        .unwrap();
        fs::write(
            root.path().join(path),
            "# Example\n\n- [x] #1.0 Done\n\nID: one\nDepends on: none\n\n## Outcome\n\nSecond.\n",
        )
        .unwrap();
        let second = LocalMemorySource::discover_at(
            root.path().to_path_buf(),
            data.path().to_path_buf(),
            project_ref(),
            RepoPath::new("docs/specs").unwrap(),
            MemoryPolicy::default(),
        )
        .unwrap();
        let fingerprint = |source: &LocalMemorySource, category| {
            source
                .manifest
                .sources
                .iter()
                .find(|source| source.category == category)
                .unwrap()
                .fingerprint
                .clone()
        };
        assert_eq!(
            fingerprint(&first, MemorySourceCategory::SpecificationStructure),
            fingerprint(&second, MemorySourceCategory::SpecificationStructure)
        );
        assert_ne!(
            fingerprint(&first, MemorySourceCategory::ApprovedOutcome),
            fingerprint(&second, MemorySourceCategory::ApprovedOutcome)
        );
    }

    #[test]
    fn runtime_source_serializes_only_bounded_provenance_fields() {
        let root = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        init_git(root.path());
        let connection = Connection::open(data.path().join("ferrus.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE tasks(id TEXT, milestone_id TEXT, status TEXT, spec_path TEXT, \
                    failure_reason TEXT, baseline_snapshot_id TEXT, \
                    repository_view_snapshot_id TEXT); \
                 CREATE TABLE runs(id TEXT, task_id TEXT, status TEXT, agent TEXT, pid INTEGER, \
                    workspace_path TEXT, baseline_snapshot_id TEXT, \
                    repository_view_snapshot_id TEXT); \
                 CREATE TABLE events(id INTEGER, run_id TEXT, type TEXT, payload_json TEXT);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO tasks VALUES (?1, ?2, 'complete', ?3, ?4, NULL, NULL)",
                params!["t-1", "one", "docs/specs/example.md", "do not persist"],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO runs VALUES (?1, ?2, 'completed', ?3, 999, ?4, NULL, NULL)",
                params!["run-1", "t-1", "private-agent", "/private/worktree"],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO events VALUES (1, ?1, 'check_passed', ?2)",
                params!["run-1", r#"{"commands":3,"secret":"hidden"}"#],
            )
            .unwrap();
        let source = LocalMemorySource::discover_at(
            root.path().to_path_buf(),
            data.path().to_path_buf(),
            project_ref(),
            RepoPath::new("docs/specs").unwrap(),
            MemoryPolicy::default(),
        )
        .unwrap();
        let runtime = source
            .materials
            .iter()
            .find(|material| {
                material.descriptor.category == MemorySourceCategory::RuntimeProvenance
            })
            .unwrap();
        let MaterialContent::Sanitized(bytes) = &runtime.content else {
            panic!("runtime content must be sanitized");
        };
        let value = String::from_utf8(bytes.clone()).unwrap();
        for forbidden in [
            "do not persist",
            "private-agent",
            "/private/worktree",
            "hidden",
            "pid",
            "failure_reason",
        ] {
            assert!(!value.contains(forbidden));
        }
        assert!(value.contains("event:1"));
    }

    #[test]
    fn verified_memory_content_clamps_utf8_and_rejects_changed_fingerprints() {
        let root = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        init_git(root.path());
        add_tracked(
            root.path(),
            "docs/specs/example.md",
            "# Example\n\n## Outcome\n\ncaf\u{00e9}\n",
        );
        let source = LocalMemorySource::discover_at(
            root.path().to_path_buf(),
            data.path().to_path_buf(),
            project_ref(),
            RepoPath::new("docs/specs").unwrap(),
            MemoryPolicy::default(),
        )
        .unwrap();
        let descriptor = source
            .manifest
            .sources
            .iter()
            .find(|source| source.category == MemorySourceCategory::ApprovedOutcome)
            .unwrap();
        let content = fs::read(root.path().join("docs/specs/example.md")).unwrap();
        let start = content
            .windows("caf\u{00e9}".len())
            .position(|window| window == "caf\u{00e9}".as_bytes())
            .unwrap() as u64;
        let response = source
            .content(MemoryContentRequest {
                project: project_ref(),
                revision_id: source.manifest.revision_id().unwrap(),
                source_category: descriptor.category,
                locator: descriptor.locator.clone(),
                expected_fingerprint: descriptor.fingerprint.clone(),
                evidence: Some(MemoryEvidenceLocator::Span(
                    crate::repository_graph::domain::SourceSpan {
                        start: crate::repository_graph::domain::SourcePosition {
                            byte_offset: start,
                            line: None,
                            column: None,
                        },
                        end: crate::repository_graph::domain::SourcePosition {
                            byte_offset: start + 5,
                            line: None,
                            column: None,
                        },
                    },
                )),
                max_bytes: NonZeroU64::new(4).unwrap(),
            })
            .unwrap();
        assert!(std::str::from_utf8(&response.bytes).is_ok());
        assert_eq!(response.bytes, b"caf");
        assert!(response.truncated);

        let error = source
            .content(MemoryContentRequest {
                project: project_ref(),
                revision_id: source.manifest.revision_id().unwrap(),
                source_category: descriptor.category,
                locator: descriptor.locator.clone(),
                expected_fingerprint: Digest::new("sha256", "00").unwrap(),
                evidence: None,
                max_bytes: NonZeroU64::new(4).unwrap(),
            })
            .unwrap_err();
        assert_eq!(error, MemoryQueryError::ContentChanged);
    }
}
