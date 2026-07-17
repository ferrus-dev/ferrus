use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Read},
    path::Path,
    process::{Command, Output, Stdio},
};

use super::{
    Candidate, CandidateKind, DiagnosticCollector, DiscoveryScan, SourceContent,
    SourceDiscoveryContext, SourceDiscoveryMetrics, SourceError, SourceManifest, SourceRoot,
    build_manifest, read_verified, same_manifest_identity, set_manifest_source_state,
};
use crate::repository_graph::{
    domain::{Digest, RepoPath, SourceKind},
    ports::{RepositorySource, SourceFileMode},
};

#[derive(Debug, Clone)]
pub struct GitRepositorySource {
    root: SourceRoot,
    context: SourceDiscoveryContext,
    manifest: SourceManifest,
}

type RawCandidates = BTreeMap<Vec<u8>, (CandidateKind, bool)>;

struct TrackedDiscovery {
    candidates: RawCandidates,
    hidden_index_state: bool,
}

impl GitRepositorySource {
    pub fn discover(
        root: impl AsRef<Path>,
        context: SourceDiscoveryContext,
    ) -> Result<Self, SourceError> {
        let root = canonical_root(root.as_ref())?;
        ensure_worktree_root(&root)?;
        let source_root = SourceRoot::new(root.clone())?;
        let mut diagnostics = DiagnosticCollector::new(context.limits.max_diagnostics);
        let mut metrics = SourceDiscoveryMetrics::default();
        let TrackedDiscovery {
            candidates: mut raw_candidates,
            hidden_index_state,
        } = tracked_candidates(&root, context.limits.max_files)?;
        let mut has_untracked = false;
        if context.policy.include_untracked {
            run_git_records(
                &root,
                &["ls-files", "--others", "--exclude-standard", "-z"],
                "list untracked files",
                |path| {
                    has_untracked = true;
                    raw_candidates
                        .entry(path.to_vec())
                        .or_insert((CandidateKind::File(None), true));
                    enforce_file_limit(raw_candidates.len(), context.limits.max_files)
                },
            )?;
        } else {
            has_untracked = git_has_output(
                &root,
                &["ls-files", "--others", "--exclude-standard", "-z"],
                "inspect excluded untracked files",
            )?;
            if has_untracked {
                diagnostics.push("untracked_paths_excluded", None);
                metrics.skipped = metrics.skipped.saturating_add(1);
            }
        }
        if git_has_output(
            &root,
            &[
                "ls-files",
                "--others",
                "--ignored",
                "--directory",
                "--exclude-standard",
                "-z",
            ],
            "inspect ignored files",
        )? {
            diagnostics.push("git_ignored_paths_excluded", None);
            metrics.skipped = metrics.skipped.saturating_add(1);
        }

        let mut candidates = Vec::new();
        let mut directories = BTreeSet::new();
        for (raw_path, (kind, untracked)) in raw_candidates {
            metrics.candidates = metrics.candidates.saturating_add(1);
            let Some(path) = normalize_git_path(&raw_path) else {
                diagnostics.push("path_encoding_unsupported", None);
                metrics.skipped = metrics.skipped.saturating_add(1);
                continue;
            };
            record_parent_directories(&path, &mut directories, context.limits.max_directories)?;
            candidates.push(Candidate {
                path,
                kind,
                untracked,
            });
        }
        metrics.directories = u64::try_from(directories.len())
            .expect("directory count always fits into u64")
            .saturating_add(1);

        let mut manifest = build_manifest(
            &source_root,
            &context,
            DiscoveryScan {
                source_kind: SourceKind::WorkspaceOverlay,
                base_revision: None,
                dirty: true,
                candidates,
                diagnostics,
                metrics,
            },
        )?;
        let dirty = has_untracked
            || hidden_index_state
            || git_has_output(
                &root,
                &[
                    "status",
                    "--porcelain=v2",
                    "-z",
                    "--untracked-files=all",
                    "--ignore-submodules=all",
                ],
                "inspect worktree status",
            )?;
        let base_revision = base_tree(&root)?;
        let source_kind = if !dirty && base_revision.is_some() {
            SourceKind::CommittedTree
        } else {
            SourceKind::WorkspaceOverlay
        };
        set_manifest_source_state(&mut manifest, source_kind, base_revision, dirty);
        Ok(Self {
            root: source_root,
            context,
            manifest,
        })
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub fn manifest(&self) -> &SourceManifest {
        &self.manifest
    }

    pub fn read_verified(
        &self,
        file: &crate::repository_graph::ports::SourceFileDescriptor,
    ) -> Result<SourceContent, SourceError> {
        read_verified(&self.root, &self.manifest, file)
    }

    pub fn revalidate(&self) -> Result<bool, SourceError> {
        let current_root = match fs::canonicalize(self.root.path()) {
            Ok(root) => root,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(source) => {
                return Err(SourceError::Io {
                    operation: "revalidate Git source root",
                    source,
                });
            }
        };
        if current_root != self.root.path() {
            return Ok(false);
        }
        let current = Self::discover(current_root, self.context.clone())?;
        Ok(same_manifest_identity(&self.manifest, &current.manifest))
    }
}

impl RepositorySource for GitRepositorySource {
    type Error = SourceError;

    fn repository(&self) -> &crate::repository_graph::domain::RepositoryRef {
        &self.manifest.revision.repository
    }

    fn manifest(&self) -> &SourceManifest {
        &self.manifest
    }

    fn read_verified(
        &self,
        file: &crate::repository_graph::ports::SourceFileDescriptor,
    ) -> Result<SourceContent, Self::Error> {
        self.read_verified(file)
    }

    fn revalidate(&self) -> Result<bool, Self::Error> {
        self.revalidate()
    }
}

fn canonical_root(root: &Path) -> Result<std::path::PathBuf, SourceError> {
    let metadata = fs::metadata(root).map_err(|_| SourceError::InvalidRoot)?;
    if !metadata.is_dir() {
        return Err(SourceError::InvalidRoot);
    }
    fs::canonicalize(root).map_err(|_| SourceError::InvalidRoot)
}

fn ensure_worktree_root(root: &Path) -> Result<(), SourceError> {
    let inside = run_git(
        root,
        &["rev-parse", "--is-inside-work-tree"],
        "locate worktree",
    )?;
    if trim_ascii(&inside.stdout) != b"true" {
        return Err(SourceError::NotGitRoot);
    }
    let prefix = run_git(
        root,
        &["rev-parse", "--show-prefix"],
        "locate worktree root",
    )?;
    if !trim_ascii(&prefix.stdout).is_empty() {
        return Err(SourceError::NotGitRoot);
    }
    Ok(())
}

fn tracked_candidates(root: &Path, max_files: u64) -> Result<TrackedDiscovery, SourceError> {
    let mut candidates = BTreeMap::new();
    let mut conflicted = BTreeSet::new();
    let mut hidden_index_state = false;
    run_git_records(
        root,
        &["ls-files", "--stage", "-v", "-z"],
        "list tracked files",
        |record| {
            let Some((&tag, header_and_path)) = record.split_first() else {
                return Err(SourceError::GitCommand {
                    operation: "parse tracked files",
                });
            };
            let Some(header_and_path) = header_and_path.strip_prefix(b" ") else {
                return Err(SourceError::GitCommand {
                    operation: "parse tracked files",
                });
            };
            hidden_index_state |= tag.is_ascii_lowercase() || tag == b'S';
            let Some(tab) = header_and_path.iter().position(|byte| *byte == b'\t') else {
                return Err(SourceError::GitCommand {
                    operation: "parse tracked files",
                });
            };
            let header = std::str::from_utf8(&header_and_path[..tab]).map_err(|_| {
                SourceError::GitCommand {
                    operation: "parse tracked files",
                }
            })?;
            let mut fields = header.split_ascii_whitespace();
            let mode = fields.next().ok_or(SourceError::GitCommand {
                operation: "parse tracked files",
            })?;
            let _object = fields.next().ok_or(SourceError::GitCommand {
                operation: "parse tracked files",
            })?;
            let stage = fields.next().ok_or(SourceError::GitCommand {
                operation: "parse tracked files",
            })?;
            if fields.next().is_some() {
                return Err(SourceError::GitCommand {
                    operation: "parse tracked files",
                });
            }
            let path = header_and_path[tab + 1..].to_vec();
            if stage != "0" {
                conflicted.insert(path.clone());
            }
            let kind = match mode {
                "100644" => CandidateKind::File(Some(SourceFileMode::Regular)),
                "100755" => CandidateKind::File(Some(SourceFileMode::Executable)),
                "120000" => CandidateKind::Symlink,
                "160000" => CandidateKind::Gitlink,
                _ => CandidateKind::Special,
            };
            candidates.entry(path).or_insert((kind, false));
            enforce_file_limit(candidates.len(), max_files)
        },
    )?;
    for path in conflicted {
        candidates.insert(path, (CandidateKind::File(None), false));
    }
    Ok(TrackedDiscovery {
        candidates,
        hidden_index_state,
    })
}

fn enforce_file_limit(count: usize, max_files: u64) -> Result<(), SourceError> {
    if count as u64 > max_files {
        Err(SourceError::FileLimitExceeded { limit: max_files })
    } else {
        Ok(())
    }
}

fn record_parent_directories(
    path: &RepoPath,
    directories: &mut BTreeSet<String>,
    max_directories: u64,
) -> Result<(), SourceError> {
    let mut components = path.as_str().split('/').peekable();
    let mut parent = String::new();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        if !parent.is_empty() {
            parent.push('/');
        }
        parent.push_str(component);
        directories.insert(parent.clone());
        if directories.len() as u64 >= max_directories {
            return Err(SourceError::DirectoryLimitExceeded {
                limit: max_directories,
            });
        }
    }
    Ok(())
}

fn base_tree(root: &Path) -> Result<Option<Digest>, SourceError> {
    let output = run_git_allow_failure(root, &["rev-parse", "--verify", "HEAD^{tree}"])?;
    if !output.status.success() {
        let symbolic_head = run_git_allow_failure(root, &["symbolic-ref", "-q", "HEAD"])?;
        if symbolic_head.status.success() && !trim_ascii(&symbolic_head.stdout).is_empty() {
            return Ok(None);
        }
        return Err(SourceError::GitCommand {
            operation: "read base tree",
        });
    }
    let value =
        std::str::from_utf8(trim_ascii(&output.stdout)).map_err(|_| SourceError::GitCommand {
            operation: "read base tree",
        })?;
    let algorithm = match value.len() {
        40 => "git-tree-sha1",
        64 => "git-tree-sha256",
        _ => {
            return Err(SourceError::GitCommand {
                operation: "read base tree",
            });
        }
    };
    Digest::new(algorithm, value)
        .map(Some)
        .map_err(|_| SourceError::GitCommand {
            operation: "read base tree",
        })
}

fn normalize_git_path(raw: &[u8]) -> Option<RepoPath> {
    let path = std::str::from_utf8(raw).ok()?;
    if path.contains('\\') {
        return None;
    }
    RepoPath::new(path).ok()
}

fn run_git(
    root: &Path,
    arguments: &[&str],
    operation: &'static str,
) -> Result<Output, SourceError> {
    let output = run_git_allow_failure(root, arguments)?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(SourceError::GitCommand { operation })
    }
}

fn run_git_allow_failure(root: &Path, arguments: &[&str]) -> Result<Output, SourceError> {
    git_command(root, arguments)
        .output()
        .map_err(|_| SourceError::GitCommand {
            operation: "run Git",
        })
}

fn run_git_records(
    root: &Path,
    arguments: &[&str],
    operation: &'static str,
    mut visit: impl FnMut(&[u8]) -> Result<(), SourceError>,
) -> Result<(), SourceError> {
    let mut child = git_command(root, arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| SourceError::GitCommand { operation })?;
    let stdout = child
        .stdout
        .take()
        .ok_or(SourceError::GitCommand { operation })?;
    let mut reader = BufReader::new(stdout);
    let mut record = Vec::new();
    loop {
        record.clear();
        let read = match reader.read_until(0, &mut record) {
            Ok(read) => read,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SourceError::GitCommand { operation });
            }
        };
        if read == 0 {
            break;
        }
        if record.last() == Some(&0) {
            record.pop();
        }
        if !record.is_empty()
            && let Err(error) = visit(&record)
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    }
    let status = child
        .wait()
        .map_err(|_| SourceError::GitCommand { operation })?;
    if status.success() {
        Ok(())
    } else {
        Err(SourceError::GitCommand { operation })
    }
}

fn git_has_output(
    root: &Path,
    arguments: &[&str],
    operation: &'static str,
) -> Result<bool, SourceError> {
    let mut child = git_command(root, arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| SourceError::GitCommand { operation })?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or(SourceError::GitCommand { operation })?;
    let mut first = [0_u8; 1];
    let read = match stdout.read(&mut first) {
        Ok(read) => read,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SourceError::GitCommand { operation });
        }
    };
    if read != 0 {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(true);
    }
    let status = child
        .wait()
        .map_err(|_| SourceError::GitCommand { operation })?;
    if status.success() {
        Ok(false)
    } else {
        Err(SourceError::GitCommand { operation })
    }
}

fn git_command(root: &Path, arguments: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-c")
        .arg(format!("core.hooksPath={NULL_DEVICE}"))
        .arg("-c")
        .arg(format!("core.excludesFile={NULL_DEVICE}"));
    #[cfg(unix)]
    command.arg("-c").arg("core.filemode=true");
    command
        .arg("-C")
        .arg(root)
        .args(arguments)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_NAMESPACE")
        .env_remove("GIT_GRAFT_FILE")
        .env_remove("GIT_REPLACE_REF_BASE")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT");
    command
}

#[cfg(windows)]
const NULL_DEVICE: &str = "NUL";
#[cfg(not(windows))]
const NULL_DEVICE: &str = "/dev/null";

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &bytes[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository_graph::{
        config::RepositoryGraphConfig,
        domain::{RepositoryId, RepositoryNamespace, RepositoryRef},
    };

    fn repository() -> RepositoryRef {
        RepositoryRef {
            namespace: RepositoryNamespace::new("local:test").unwrap(),
            repository_id: RepositoryId::new("root").unwrap(),
        }
    }

    fn context(config: &RepositoryGraphConfig) -> SourceDiscoveryContext {
        SourceDiscoveryContext::from_config(repository(), config, &[]).unwrap()
    }

    fn git(root: &Path, arguments: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            arguments,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn initialized_repository() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init"]);
        git(directory.path(), &["config", "commit.gpgsign", "false"]);
        git(directory.path(), &["config", "user.name", "Ferrus Tests"]);
        git(
            directory.path(),
            &["config", "user.email", "ferrus@example.invalid"],
        );
        directory
    }

    #[test]
    fn git_discovery_includes_tracked_and_nonignored_untracked_files() {
        let directory = initialized_repository();
        fs::write(directory.path().join(".gitignore"), b"ignored.log\n").unwrap();
        fs::write(directory.path().join("tracked.rs"), b"fn tracked() {}\n").unwrap();
        git(directory.path(), &["add", ".gitignore", "tracked.rs"]);
        git(directory.path(), &["commit", "-m", "initial"]);

        let clean = GitRepositorySource::discover(
            directory.path(),
            context(&RepositoryGraphConfig::default()),
        )
        .unwrap();
        assert_eq!(
            clean.manifest().revision.source_kind,
            SourceKind::CommittedTree
        );
        assert!(!clean.manifest().revision.dirty);
        assert!(clean.manifest().revision.base_revision.is_some());
        assert!(clean.revalidate().unwrap());

        fs::write(directory.path().join("tracked.rs"), b"fn changed() {}\n").unwrap();
        fs::write(directory.path().join("untracked.rs"), b"fn new() {}\n").unwrap();
        fs::write(directory.path().join("ignored.log"), b"ignored\n").unwrap();
        fs::write(directory.path().join(".env"), b"TOKEN=secret\n").unwrap();
        assert!(!clean.revalidate().unwrap());
        let source = GitRepositorySource::discover(
            directory.path(),
            context(&RepositoryGraphConfig::default()),
        )
        .unwrap();
        let paths = source
            .manifest()
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec![".gitignore", "tracked.rs", "untracked.rs"]);
        assert_eq!(
            source.manifest().revision.source_kind,
            SourceKind::WorkspaceOverlay
        );
        assert!(source.manifest().revision.dirty);
        assert!(source.manifest().revision.includes_untracked);
        assert!(source.manifest().diagnostics.iter().any(|diagnostic| {
            diagnostic.code.as_str() == "sensitive_path_excluded"
                && diagnostic.path.as_ref().map(RepoPath::as_str) == Some(".env")
        }));
        assert!(source.manifest().diagnostics.iter().any(|diagnostic| {
            diagnostic.code.as_str() == "git_ignored_paths_excluded" && diagnostic.path.is_none()
        }));
    }

    #[test]
    fn local_source_selects_git_at_a_worktree_root() {
        let directory = initialized_repository();
        let source = crate::repository_graph::source::LocalRepositorySource::discover(
            directory.path(),
            context(&RepositoryGraphConfig::default()),
        )
        .unwrap();
        assert!(matches!(
            source,
            crate::repository_graph::source::LocalRepositorySource::Git(_)
        ));
    }

    #[test]
    fn machine_global_exclude_files_do_not_change_discovery_policy() {
        let directory = initialized_repository();
        let excludes = tempfile::NamedTempFile::new().unwrap();
        fs::write(excludes.path(), b"hidden-by-machine.rs\n").unwrap();
        git(
            directory.path(),
            &[
                "config",
                "core.excludesFile",
                excludes.path().to_str().unwrap(),
            ],
        );
        fs::write(
            directory.path().join("hidden-by-machine.rs"),
            b"fn still_visible() {}\n",
        )
        .unwrap();

        let source = GitRepositorySource::discover(
            directory.path(),
            context(&RepositoryGraphConfig::default()),
        )
        .unwrap();
        assert!(
            source
                .manifest()
                .files
                .iter()
                .any(|file| file.path.as_str() == "hidden-by-machine.rs")
        );
    }

    #[test]
    fn excluded_untracked_content_does_not_change_the_manifest_digest() {
        let directory = initialized_repository();
        fs::write(directory.path().join("tracked.rs"), b"fn tracked() {}\n").unwrap();
        git(directory.path(), &["add", "tracked.rs"]);
        git(directory.path(), &["commit", "-m", "initial"]);
        let clean = GitRepositorySource::discover(
            directory.path(),
            context(&RepositoryGraphConfig::default()),
        )
        .unwrap();

        fs::write(directory.path().join(".env"), b"TOKEN=secret\n").unwrap();
        assert!(!clean.revalidate().unwrap());
        let excluded = GitRepositorySource::discover(
            directory.path(),
            context(&RepositoryGraphConfig::default()),
        )
        .unwrap();

        assert_eq!(
            clean.manifest().revision.manifest_digest,
            excluded.manifest().revision.manifest_digest
        );
        assert!(excluded.manifest().revision.dirty);
        assert!(!excluded.manifest().revision.includes_untracked);
    }

    #[test]
    fn excluded_committed_changes_invalidate_base_revision_metadata() {
        let directory = initialized_repository();
        fs::write(directory.path().join("tracked.rs"), b"fn tracked() {}\n").unwrap();
        git(directory.path(), &["add", "tracked.rs"]);
        git(directory.path(), &["commit", "-m", "initial"]);
        let initial = GitRepositorySource::discover(
            directory.path(),
            context(&RepositoryGraphConfig::default()),
        )
        .unwrap();

        fs::write(directory.path().join(".env"), b"TOKEN=secret\n").unwrap();
        git(directory.path(), &["add", "-f", ".env"]);
        git(directory.path(), &["commit", "-m", "excluded metadata"]);
        let changed = GitRepositorySource::discover(
            directory.path(),
            context(&RepositoryGraphConfig::default()),
        )
        .unwrap();

        assert_eq!(
            initial.manifest().revision.manifest_digest,
            changed.manifest().revision.manifest_digest
        );
        assert_ne!(
            initial.manifest().revision.base_revision,
            changed.manifest().revision.base_revision
        );
        assert!(!initial.revalidate().unwrap());
    }

    #[test]
    fn git_untracked_policy_and_unborn_head_are_supported() {
        let directory = initialized_repository();
        fs::write(directory.path().join("only.rs"), b"fn only() {}\n").unwrap();
        let mut config = RepositoryGraphConfig::default();
        config.source.include_untracked = false;
        let source = GitRepositorySource::discover(directory.path(), context(&config)).unwrap();
        assert!(source.manifest().files.is_empty());
        assert!(source.manifest().revision.base_revision.is_none());
        assert_eq!(
            source.manifest().revision.source_kind,
            SourceKind::WorkspaceOverlay
        );
        assert!(!source.manifest().revision.includes_untracked);
        assert!(source.manifest().revision.dirty);
        assert!(source.manifest().diagnostics.iter().any(|diagnostic| {
            diagnostic.code.as_str() == "untracked_paths_excluded" && diagnostic.path.is_none()
        }));
    }

    #[test]
    fn git_candidate_count_is_bounded_before_manifest_materialization() {
        let directory = initialized_repository();
        fs::write(directory.path().join("one.rs"), b"fn one() {}\n").unwrap();
        fs::write(directory.path().join("two.rs"), b"fn two() {}\n").unwrap();
        let mut config = RepositoryGraphConfig::default();
        config.index_limits.max_files = 1;

        assert!(matches!(
            GitRepositorySource::discover(directory.path(), context(&config)),
            Err(SourceError::FileLimitExceeded { limit: 1 })
        ));
    }

    #[test]
    fn git_derived_directory_count_is_bounded() {
        let directory = initialized_repository();
        fs::create_dir_all(directory.path().join("one/two")).unwrap();
        fs::write(
            directory.path().join("one/two/file.rs"),
            b"fn nested() {}\n",
        )
        .unwrap();
        let mut config = RepositoryGraphConfig::default();
        config.index_limits.max_directories = 2;

        assert!(matches!(
            GitRepositorySource::discover(directory.path(), context(&config)),
            Err(SourceError::DirectoryLimitExceeded { limit: 2 })
        ));
    }

    #[test]
    fn assume_unchanged_files_never_report_a_clean_committed_tree() {
        let directory = initialized_repository();
        let path = directory.path().join("tracked.rs");
        fs::write(&path, b"fn original() {}\n").unwrap();
        git(directory.path(), &["add", "tracked.rs"]);
        git(directory.path(), &["commit", "-m", "initial"]);
        git(
            directory.path(),
            &["update-index", "--assume-unchanged", "tracked.rs"],
        );
        fs::write(&path, b"fn hidden_change() {}\n").unwrap();

        let source = GitRepositorySource::discover(
            directory.path(),
            context(&RepositoryGraphConfig::default()),
        )
        .unwrap();
        assert!(source.manifest().revision.dirty);
        assert_eq!(
            source.manifest().revision.source_kind,
            SourceKind::WorkspaceOverlay
        );
    }

    #[cfg(unix)]
    #[test]
    fn git_nul_paths_and_symlinks_are_handled_without_dereference() {
        use std::os::unix::fs::symlink;

        let directory = initialized_repository();
        let newline_path = directory.path().join("line\nbreak.rs");
        fs::write(&newline_path, b"fn newline() {}\n").unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), b"outside secret\n").unwrap();
        symlink(outside.path(), directory.path().join("outside-link")).unwrap();
        git(directory.path(), &["add", "line\nbreak.rs", "outside-link"]);

        let source = GitRepositorySource::discover(
            directory.path(),
            context(&RepositoryGraphConfig::default()),
        )
        .unwrap();
        assert!(
            source
                .manifest()
                .files
                .iter()
                .any(|file| file.path.as_str() == "line\nbreak.rs")
        );
        assert!(
            !source
                .manifest()
                .files
                .iter()
                .any(|file| file.path.as_str() == "outside-link")
        );
        assert!(
            source
                .manifest()
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code.as_str() == "symlink_skipped")
        );
    }

    #[cfg(unix)]
    #[test]
    fn tracked_symlink_replaced_by_a_regular_file_is_included() {
        use std::os::unix::fs::symlink;

        let directory = initialized_repository();
        fs::write(directory.path().join("target.rs"), b"fn target() {}\n").unwrap();
        symlink("target.rs", directory.path().join("changed.rs")).unwrap();
        git(directory.path(), &["add", "target.rs", "changed.rs"]);
        git(directory.path(), &["commit", "-m", "symlink"]);
        fs::remove_file(directory.path().join("changed.rs")).unwrap();
        fs::write(
            directory.path().join("changed.rs"),
            b"fn now_regular() {}\n",
        )
        .unwrap();

        let source = GitRepositorySource::discover(
            directory.path(),
            context(&RepositoryGraphConfig::default()),
        )
        .unwrap();
        assert!(
            source
                .manifest()
                .files
                .iter()
                .any(|file| file.path.as_str() == "changed.rs")
        );
    }

    #[cfg(unix)]
    #[test]
    fn repository_configured_fsmonitor_is_never_executed() {
        use std::os::unix::fs::PermissionsExt;

        let directory = initialized_repository();
        fs::write(directory.path().join("tracked.rs"), b"fn tracked() {}\n").unwrap();
        git(directory.path(), &["add", "tracked.rs"]);
        git(directory.path(), &["commit", "-m", "initial"]);
        let hook = directory.path().join("fsmonitor-hook");
        fs::write(&hook, b"#!/bin/sh\ntouch fsmonitor-invoked\n").unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();
        git(
            directory.path(),
            &["config", "core.fsmonitor", hook.to_str().unwrap()],
        );

        GitRepositorySource::discover(directory.path(), context(&RepositoryGraphConfig::default()))
            .unwrap();
        assert!(!directory.path().join("fsmonitor-invoked").exists());
    }

    #[cfg(unix)]
    #[test]
    fn git_worktree_mode_changes_participate_in_manifest_identity() {
        use std::os::unix::fs::PermissionsExt;

        let directory = initialized_repository();
        git(directory.path(), &["config", "core.filemode", "true"]);
        let path = directory.path().join("script.sh");
        fs::write(&path, b"#!/bin/sh\n").unwrap();
        git(directory.path(), &["add", "script.sh"]);
        git(directory.path(), &["commit", "-m", "script"]);
        let regular = GitRepositorySource::discover(
            directory.path(),
            context(&RepositoryGraphConfig::default()),
        )
        .unwrap();
        assert_eq!(
            regular.manifest().files[0].file_mode,
            SourceFileMode::Regular
        );

        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        let executable = GitRepositorySource::discover(
            directory.path(),
            context(&RepositoryGraphConfig::default()),
        )
        .unwrap();
        assert_eq!(
            executable.manifest().files[0].file_mode,
            SourceFileMode::Executable
        );
        assert_ne!(
            regular.manifest().revision.manifest_digest,
            executable.manifest().revision.manifest_digest
        );
    }
}
