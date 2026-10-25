//! Project-scoped local sources for deterministic project-memory ingestion.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
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
    policy::{MemoryContentAccess, MemoryPolicy},
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
            let approved_outcomes = approved_outcome_fingerprints(&data_dir)?;
            discover_tracked_specs(
                &root,
                &project,
                &spec_directory,
                &policy,
                &approved_outcomes,
                &mut materials,
            )?;
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

        materials = crate::json_size::sort_by_json_key(materials, material_key)?;
        let mut manifest = AuthorizedSourceManifest {
            project: project.clone(),
            policy_digest: policy.digest()?,
            source_set_digest: Digest::new("sha256", "00")?,
            extractor_set_digest: built_in_extractor_set_digest()?,
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

    /// Discover authorized memory sources under an explicit local project binding.
    pub fn discover_at(
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
                if canonical_digest(bytes)? != source.fingerprint {
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
        self.content_with_deadline(request, Duration::MAX)
    }

    fn content_with_deadline(
        &self,
        request: MemoryContentRequest,
        max_duration: Duration,
    ) -> Result<MemoryContentResponse, MemoryQueryError> {
        let started = Instant::now();
        ensure_content_read_deadline(started, max_duration)?;
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

        let source_policy = self
            .policy
            .category(request.source_category)
            .filter(|policy| policy.enabled)
            .ok_or(MemoryQueryError::SourceNotAuthorized)?;
        let evidence_is_span = matches!(
            request.evidence.as_ref(),
            Some(super::domain::MemoryEvidenceLocator::Span(_))
        );
        match source_policy.content_access {
            MemoryContentAccess::StructureOnly | MemoryContentAccess::CuratedSections
                if !evidence_is_span =>
            {
                return Err(MemoryQueryError::SourceNotAuthorized);
            }
            MemoryContentAccess::IdentityAndCountsOnly => {
                return Err(MemoryQueryError::SourceNotAuthorized);
            }
            MemoryContentAccess::StructureOnly
            | MemoryContentAccess::CuratedSections
            | MemoryContentAccess::RawBody => {}
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
        let content = read_verified_content(material, started, max_duration)?;
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
                const { super::diagnostics::MemoryDiagnosticCode::from_static("content.nonutf8") },
            )
        })?;
        let mut length = text.len().min(request.max_bytes.get() as usize);
        while length > 0 && !text.is_char_boundary(length) {
            length -= 1;
        }

        ensure_content_read_deadline(started, max_duration)?;
        Ok(MemoryContentResponse {
            verified_fingerprint: material.descriptor.fingerprint.clone(),
            bytes: text.as_bytes()[..length].to_vec(),
            truncated: length < text.len(),
        })
    }
}

fn read_verified_content(
    material: &SourceMaterial,
    started: Instant,
    max_duration: Duration,
) -> Result<MemorySourceContent, MemoryQueryError> {
    ensure_content_read_deadline(started, max_duration)?;
    let bytes = match &material.content {
        MaterialContent::TrackedSpec {
            absolute_path,
            category,
        } => {
            let metadata = fs::symlink_metadata(absolute_path)
                .map_err(|_| MemoryQueryError::ContentChanged)?;
            ensure_content_read_deadline(started, max_duration)?;
            if !metadata.file_type().is_file() || metadata.len() > MAX_SPEC_BYTES {
                return Err(MemoryQueryError::ContentChanged);
            }

            let mut file =
                fs::File::open(absolute_path).map_err(|_| MemoryQueryError::ContentChanged)?;
            ensure_content_read_deadline(started, max_duration)?;
            let mut bytes = Vec::with_capacity(
                usize::try_from(metadata.len()).unwrap_or(MAX_SPEC_BYTES as usize),
            );
            let mut chunk = [0_u8; 16 * 1024];
            loop {
                ensure_content_read_deadline(started, max_duration)?;
                let read = file
                    .read(&mut chunk)
                    .map_err(|_| MemoryQueryError::ContentChanged)?;
                ensure_content_read_deadline(started, max_duration)?;
                if read == 0 {
                    break;
                }

                if bytes.len().saturating_add(read) > MAX_SPEC_BYTES as usize {
                    return Err(MemoryQueryError::ContentChanged);
                }

                bytes.extend_from_slice(&chunk[..read]);
            }

            let fingerprint = spec_fingerprint(*category, &bytes)
                .map_err(|_| MemoryQueryError::ContentChanged)?;
            ensure_content_read_deadline(started, max_duration)?;
            if fingerprint != material.descriptor.fingerprint {
                return Err(MemoryQueryError::ContentChanged);
            }

            bytes
        }
        MaterialContent::Sanitized(bytes) => {
            if canonical_digest(bytes).map_err(|_| MemoryQueryError::ContentChanged)?
                != material.descriptor.fingerprint
            {
                return Err(MemoryQueryError::ContentChanged);
            }

            ensure_content_read_deadline(started, max_duration)?;
            bytes.clone()
        }
    };
    ensure_content_read_deadline(started, max_duration)?;
    Ok(MemorySourceContent { bytes })
}

fn ensure_content_read_deadline(
    started: Instant,
    max_duration: Duration,
) -> Result<(), MemoryQueryError> {
    if started.elapsed() >= max_duration {
        Err(MemoryQueryError::BudgetExceeded(
            super::query::MemoryTruncationReason::Duration,
        ))
    } else {
        Ok(())
    }
}

fn discover_tracked_specs(
    root: &Path,
    project: &ProjectRef,
    spec_directory: &RepoPath,
    policy: &MemoryPolicy,
    approved_outcomes: &BTreeMap<RepoPath, BTreeSet<Digest>>,
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
                    fingerprint: canonical_digest(&parsed.structure)?,
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
            && let fingerprint = canonical_digest(&outcome)?
            && approved_outcomes
                .get(&path)
                .is_some_and(|approved| approved.contains(&fingerprint))
        {
            materials.push(SourceMaterial {
                descriptor: AuthorizedSourceDescriptor {
                    project: project.clone(),
                    category: MemorySourceCategory::ApprovedOutcome,
                    locator: MemorySourceLocator::TrackedFile { path },
                    fingerprint,
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
        MemorySourceCategory::SpecificationStructure => Ok(canonical_digest(&parsed.structure)?),
        MemorySourceCategory::ApprovedOutcome => parsed
            .outcome
            .context("approved Outcome section is no longer present")
            .and_then(|outcome| Ok(canonical_digest(&outcome)?)),
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

fn approved_outcome_fingerprints(data_dir: &Path) -> Result<BTreeMap<RepoPath, BTreeSet<Digest>>> {
    let database_path = data_dir.join("ferrus.db");
    let archive_root = data_dir.join("archive").join("specs");
    if !database_path.is_file() || !archive_root.is_dir() {
        return Ok(BTreeMap::new());
    }

    let archive_root = fs::canonicalize(archive_root)?;
    let connection = Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let has_archive_table: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'spec_archives')",
        [],
        |row| row.get(0),
    )?;
    if !has_archive_table {
        return Ok(BTreeMap::new());
    }

    let mut statement = connection
        .prepare("SELECT spec_path, archive_dir, outcome FROM spec_archives ORDER BY id")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut fingerprints = BTreeMap::<RepoPath, BTreeSet<Digest>>::new();
    let mut archive_count = 0usize;
    for row in rows {
        archive_count += 1;
        if archive_count > MAX_SOURCES {
            anyhow::bail!("approved Outcome archive proof limit exceeded");
        }

        let (spec_path, archive_dir, recorded_outcome) = row?;
        let Ok(spec_path) = RepoPath::new(spec_path) else {
            continue;
        };
        let Ok(archive_dir) = fs::canonicalize(archive_dir) else {
            continue;
        };
        if !archive_dir.starts_with(&archive_root) {
            continue;
        }

        let manifest_path = archive_dir.join("manifest.toml");
        let spec_copy_path = archive_dir.join("spec.md");
        if !bounded_regular_file(&manifest_path, MAX_ARCHIVE_MANIFEST_BYTES)
            || !bounded_regular_file(&spec_copy_path, MAX_SPEC_BYTES)
        {
            continue;
        }

        let Ok(manifest) = fs::read_to_string(manifest_path) else {
            continue;
        };
        let Ok(manifest) = toml::from_str::<RawArchiveManifest>(&manifest) else {
            continue;
        };
        if RepoPath::new(manifest.spec_path).ok().as_ref() != Some(&spec_path) {
            continue;
        }

        let Ok(spec_copy) = fs::read_to_string(spec_copy_path) else {
            continue;
        };
        let Some(outcome) = parse_spec_memory(&spec_copy).outcome else {
            continue;
        };
        let recorded_outcome = recorded_outcome.trim();
        let recorded_outcome = if recorded_outcome.starts_with("## Outcome") {
            recorded_outcome.to_string()
        } else {
            format!("## Outcome\n\n{recorded_outcome}")
        };
        let Some(recorded_outcome) = parse_spec_memory(&recorded_outcome).outcome else {
            continue;
        };
        if outcome.text != recorded_outcome.text
            || outcome.sections.len() != recorded_outcome.sections.len()
            || !outcome.sections.iter().zip(&recorded_outcome.sections).all(
                |(archived, recorded)| {
                    archived.kind == recorded.kind && archived.text == recorded.text
                },
            )
        {
            continue;
        }

        let fingerprint = canonical_digest(&outcome)?;
        fingerprints
            .entry(spec_path)
            .or_default()
            .insert(fingerprint);
    }

    Ok(fingerprints)
}

fn bounded_regular_file(path: &Path, max_bytes: u64) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.file_type().is_file() && metadata.len() <= max_bytes)
}

fn discover_archives(
    data_dir: &Path,
    project: &ProjectRef,
    materials: &mut Vec<SourceMaterial>,
) -> Result<()> {
    for archive_dir in durable_archive_directories(data_dir)? {
        let Some(archive_id) = archive_dir
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
        else {
            continue;
        };
        MemoryRecordId::new(&archive_id)?;
        let manifest_path = archive_dir.join("manifest.toml");
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
            task_count: count_regular_files(&archive_dir.join("tasks"))?,
            run_count: count_directories(&archive_dir.join("runs"))?,
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
                fingerprint: canonical_digest(&bytes)?,
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

fn durable_archive_directories(data_dir: &Path) -> Result<Vec<PathBuf>> {
    let database_path = data_dir.join("ferrus.db");
    let archive_root = data_dir.join("archive").join("specs");
    if !database_path.is_file() || !archive_root.is_dir() {
        return Ok(Vec::new());
    }

    let archive_root = fs::canonicalize(archive_root)?;
    let connection = Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let has_archive_table: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'spec_archives')",
        [],
        |row| row.get(0),
    )?;
    if !has_archive_table {
        return Ok(Vec::new());
    }

    let mut statement = connection.prepare("SELECT archive_dir FROM spec_archives ORDER BY id")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut directories = BTreeSet::new();
    let mut archive_count = 0usize;
    for row in rows {
        archive_count += 1;
        if archive_count > MAX_SOURCES {
            anyhow::bail!("durable archive record limit exceeded");
        }

        let Ok(directory) = fs::canonicalize(row?) else {
            continue;
        };
        if directory != archive_root && directory.starts_with(&archive_root) && directory.is_dir() {
            directories.insert(directory);
        }
    }

    Ok(directories.into_iter().collect())
}

fn material_key(material: &SourceMaterial) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&material.descriptor)
}

#[cfg(test)]
pub(crate) fn record_approved_outcome_for_test(data: &Path, spec_path: &str, spec_content: &str) {
    if parse_spec_memory(spec_content).outcome.is_none() {
        return;
    }

    let connection = Connection::open(data.join("ferrus.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS spec_archives (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                spec_path TEXT NOT NULL, archive_dir TEXT NOT NULL, \
                closed_at TEXT NOT NULL, task_count INTEGER NOT NULL, \
                run_count INTEGER NOT NULL, outcome TEXT NOT NULL\
            ); \
            CREATE TABLE IF NOT EXISTS tasks (\
                id TEXT, milestone_id TEXT, status TEXT, spec_path TEXT, \
                baseline_snapshot_id TEXT, repository_view_snapshot_id TEXT\
            ); \
            CREATE TABLE IF NOT EXISTS runs (\
                id TEXT, task_id TEXT, status TEXT, baseline_snapshot_id TEXT, \
                repository_view_snapshot_id TEXT\
            ); \
            CREATE TABLE IF NOT EXISTS events (\
                id INTEGER, run_id TEXT, type TEXT, payload_json TEXT\
            );",
        )
        .unwrap();
    let mut statement = connection
        .prepare("SELECT archive_dir FROM spec_archives WHERE spec_path = ?1 ORDER BY id")
        .unwrap();
    let existing = statement
        .query_map([spec_path], |row| row.get::<_, String>(0))
        .unwrap()
        .filter_map(Result::ok)
        .any(|archive_dir| {
            fs::read_to_string(Path::new(&archive_dir).join("spec.md"))
                .is_ok_and(|archived| archived == spec_content)
        });
    drop(statement);
    if existing {
        return;
    }

    let proof_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM spec_archives", [], |row| row.get(0))
        .unwrap();
    let archive_dir = data
        .join("archive/specs")
        .join(format!("proof-{}", proof_count + 1));
    fs::create_dir_all(&archive_dir).unwrap();
    fs::write(archive_dir.join("spec.md"), spec_content).unwrap();
    fs::write(
        archive_dir.join("manifest.toml"),
        format!("spec_path = {spec_path:?}\narchived_at = \"2026-08-11T00:00:00Z\"\ntasks = []\n"),
    )
    .unwrap();
    let outcome_start = spec_content.find("## Outcome").unwrap();
    let recorded_outcome = spec_content[outcome_start..].trim();
    connection
        .execute(
            "INSERT INTO spec_archives \
             (spec_path, archive_dir, closed_at, task_count, run_count, outcome) \
             VALUES (?1, ?2, '2026-08-11T00:00:00Z', 0, 0, ?3)",
            rusqlite::params![spec_path, archive_dir.to_string_lossy(), recorded_outcome],
        )
        .unwrap();
}

mod runtime;
use runtime::*;

#[cfg(test)]
#[path = "source_tests.rs"]
mod tests;
