//! Pinned Git tree capture, inventory, and verified reads.

use super::*;

/// Captures the effective worktree as a Git tree without changing its real
/// index. This is shared by HQ baseline creation and graph baseline checks.
pub fn capture_worktree_tree(root: impl AsRef<Path>) -> Result<Digest, SourceError> {
    let root = canonical_root(root.as_ref())?;
    ensure_worktree_root(&root)?;
    let temporary_index = TemporaryGitIndex::new();
    let head = run_git_allow_failure(&root, &["rev-parse", "--verify", "HEAD^{tree}"])?;
    if head.status.success() {
        let value = std::str::from_utf8(trim_ascii(&head.stdout))
            .map_err(|_| SourceError::InvalidGitTree)?;
        run_git_with_index(
            &root,
            temporary_index.path(),
            &["read-tree", value],
            "seed temporary index",
        )?;
    } else {
        run_git_with_index(
            &root,
            temporary_index.path(),
            &["read-tree", "--empty"],
            "seed empty temporary index",
        )?;
    }

    run_git_with_index(
        &root,
        temporary_index.path(),
        &["add", "-A", "--", "."],
        "capture worktree paths",
    )?;
    run_git_with_index(
        &root,
        temporary_index.path(),
        &["rm", "--cached", "-r", "--ignore-unmatch", "--", ".ferrus"],
        "exclude Ferrus runtime paths",
    )?;
    let tree = run_git_with_index(
        &root,
        temporary_index.path(),
        &["write-tree"],
        "write captured worktree tree",
    )?;
    parse_git_tree_digest(
        std::str::from_utf8(trim_ascii(&tree.stdout)).map_err(|_| SourceError::InvalidGitTree)?,
    )
}

/// Keeps a submitted tree and all of its blobs reachable while the task is in
/// review. The ref is task-scoped because there can only be one active frozen
/// submission per task, and the reviewing run has a different run identity
/// from the Executor that created the tree.
pub fn pin_submitted_tree(
    root: impl AsRef<Path>,
    task_id: &str,
    tree: &Digest,
) -> Result<(), SourceError> {
    validate_git_tree_digest(tree)?;
    let root = canonical_root(root.as_ref())?;
    ensure_worktree_root(&root)?;
    let reference = submitted_tree_ref(task_id);
    run_git(
        &root,
        &["update-ref", &reference, tree.value()],
        "pin submitted tree",
    )?;
    Ok(())
}

pub fn release_submitted_tree_pin(
    root: impl AsRef<Path>,
    task_id: &str,
) -> Result<(), SourceError> {
    let root = canonical_root(root.as_ref())?;
    ensure_worktree_root(&root)?;
    let reference = submitted_tree_ref(task_id);
    run_git(
        &root,
        &["update-ref", "-d", &reference],
        "release submitted tree pin",
    )?;
    Ok(())
}

pub(super) fn submitted_tree_ref(task_id: &str) -> String {
    let digest = sha256_digest(task_id.as_bytes());
    format!("refs/ferrus/reviews/{}", digest.value())
}

pub(super) fn verify_tree(root: &Path, baseline: &Digest) -> Result<(), SourceError> {
    let tree = format!("{}^{{tree}}", baseline.value());
    run_git(root, &["cat-file", "-e", &tree], "verify baseline tree").map(|_| ())
}

pub(in super::super) fn verify_tree_available(
    root: &Path,
    tree: &Digest,
) -> Result<(), SourceError> {
    validate_git_tree_digest(tree)?;
    verify_tree(root, tree)
}

pub(in super::super) fn discover_tree_manifest(
    root: &Path,
    context: &SourceDiscoveryContext,
    tree: Digest,
) -> Result<SourceManifest, SourceError> {
    validate_git_tree_digest(&tree)?;
    verify_tree(root, &tree)?;
    let entries = baseline_entries(root, &tree)?;
    if entries.len() as u64 > context.limits.max_files {
        return Err(SourceError::FileLimitExceeded {
            limit: context.limits.max_files,
        });
    }

    let mut diagnostics = DiagnosticCollector::new(context.limits.max_diagnostics);
    let mut metrics = SourceDiscoveryMetrics {
        candidates: entries.len() as u64,
        ..SourceDiscoveryMetrics::default()
    };
    let mut directories = BTreeSet::new();
    let mut files = Vec::new();

    for (path, entry) in entries {
        record_tree_parent_directories(&path, &mut directories, context.limits.max_directories)?;
        if let Some(code) = context.policy.exclusion_for_file(&path) {
            diagnostics.push(code, Some(path));
            metrics.skipped = metrics.skipped.saturating_add(1);
            continue;
        }

        let mode = match entry.mode.as_str() {
            "100644" => SourceFileMode::Regular,
            "100755" => SourceFileMode::Executable,
            "120000" => {
                diagnostics.push(
                    const {
                        crate::repository_graph::domain::DiagnosticCode::from_static(
                            "symlink_skipped",
                        )
                    },
                    Some(path),
                );
                metrics.skipped = metrics.skipped.saturating_add(1);
                continue;
            }
            "160000" => {
                diagnostics.push(
                    const {
                        crate::repository_graph::domain::DiagnosticCode::from_static(
                            "gitlink_skipped",
                        )
                    },
                    Some(path),
                );
                metrics.skipped = metrics.skipped.saturating_add(1);
                continue;
            }
            _ => {
                diagnostics.push(
                    const {
                        crate::repository_graph::domain::DiagnosticCode::from_static(
                            "special_file_skipped",
                        )
                    },
                    Some(path),
                );
                metrics.skipped = metrics.skipped.saturating_add(1);
                continue;
            }
        };
        let size = tree_blob_size(root, &entry.object_id)?;
        if size > context.limits.max_file_bytes {
            diagnostics.push(const { crate::repository_graph::domain::DiagnosticCode::from_static("file_too_large") }, Some(path));
            metrics.skipped = metrics.skipped.saturating_add(1);
            continue;
        }

        if metrics.total_bytes.saturating_add(size) > context.limits.max_total_bytes {
            return Err(SourceError::TotalBytesLimitExceeded {
                limit: context.limits.max_total_bytes,
            });
        }

        let output = run_git(
            root,
            &["cat-file", "blob", &entry.object_id],
            "read pinned baseline blob",
        )?;
        if output.stdout.len() as u64 != size {
            return Err(SourceError::ContentChanged);
        }

        metrics.total_bytes = metrics.total_bytes.saturating_add(size);
        if is_binary(&output.stdout) {
            diagnostics.push(
                const {
                    crate::repository_graph::domain::DiagnosticCode::from_static(
                        "binary_file_skipped",
                    )
                },
                Some(path),
            );
            metrics.skipped = metrics.skipped.saturating_add(1);
            continue;
        }

        files.push(SourceFileDescriptor {
            path,
            content_identity: sha256_digest(&output.stdout),
            byte_len: size,
            file_mode: mode,
        });
        metrics.included = metrics.included.saturating_add(1);
    }

    metrics.directories = directories.len() as u64 + 1;
    metrics.suppressed_diagnostics = diagnostics.suppressed;
    let source_manifest_digest =
        canonical_source_manifest_digest(&files, &context.source_policy_digest)?;
    let revision = SourceRevision {
        id: revision_id(
            &context.repository,
            SourceKind::TaskBaseline,
            Some(&tree),
            &source_manifest_digest,
            &context.analysis_config_digest,
            false,
            false,
        )?,
        repository: context.repository.clone(),
        source_kind: SourceKind::TaskBaseline,
        base_revision: Some(tree),
        manifest_digest: source_manifest_digest,
        analysis_config_digest: context.analysis_config_digest.clone(),
        dirty: false,
        includes_untracked: false,
    };
    Ok(SourceManifest {
        revision,
        extractor_set_digest: context.extractor_set_digest.clone(),
        files,
        diagnostics: diagnostics.diagnostics,
        metrics,
    })
}

pub(super) fn tree_blob_size(root: &Path, object_id: &str) -> Result<u64, SourceError> {
    let output = run_git(
        root,
        &["cat-file", "-s", object_id],
        "inspect pinned baseline blob",
    )?;
    std::str::from_utf8(trim_ascii(&output.stdout))
        .ok()
        .and_then(|size| size.parse().ok())
        .ok_or(SourceError::GitCommand {
            operation: "parse pinned baseline blob size",
        })
}

pub(super) fn record_tree_parent_directories(
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

pub(in super::super) fn read_tree_descriptor_verified(
    root: &Path,
    tree: &Digest,
    file: &SourceFileDescriptor,
) -> Result<SourceContent, SourceError> {
    validate_git_tree_digest(tree)?;
    let root = canonical_root(root)?;
    ensure_worktree_root(&root)?;
    verify_tree(&root, tree)?;
    let entries = baseline_entries(&root, tree)?;
    let object_id = entries
        .get(&file.path)
        .map(|entry| entry.object_id.as_str())
        .ok_or(SourceError::FileNotInManifest)?;
    let output = run_git(
        &root,
        &["cat-file", "blob", object_id],
        "read frozen submitted blob",
    )?;
    if output.stdout.len() as u64 != file.byte_len
        || sha256_digest(&output.stdout) != file.content_identity
    {
        return Err(SourceError::ContentChanged);
    }

    Ok(SourceContent {
        bytes: output.stdout,
    })
}

pub(super) fn baseline_entries(
    root: &Path,
    baseline: &Digest,
) -> Result<BTreeMap<RepoPath, BaselineEntry>, SourceError> {
    let output = run_git(
        root,
        &["ls-tree", "-r", "-z", "--full-tree", baseline.value()],
        "list baseline tree",
    )?;
    let mut entries = BTreeMap::new();
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or(SourceError::GitCommand {
                operation: "parse baseline tree",
            })?;
        let header = std::str::from_utf8(&record[..tab]).map_err(|_| SourceError::GitCommand {
            operation: "parse baseline tree",
        })?;
        let mut fields = header.split_ascii_whitespace();
        let mode = fields.next().ok_or(SourceError::GitCommand {
            operation: "parse baseline tree",
        })?;
        let _kind = fields.next().ok_or(SourceError::GitCommand {
            operation: "parse baseline tree",
        })?;
        let object_id = fields.next().ok_or(SourceError::GitCommand {
            operation: "parse baseline tree",
        })?;
        if fields.next().is_some() {
            return Err(SourceError::GitCommand {
                operation: "parse baseline tree",
            });
        }

        let path = parse_path(&record[tab + 1..], "parse baseline tree path")?;
        if !is_runtime_path(&path) {
            entries.insert(
                path,
                BaselineEntry {
                    object_id: object_id.to_string(),
                    mode: mode.to_string(),
                },
            );
        }
    }

    Ok(entries)
}

pub(super) fn listed_paths(
    root: &Path,
    arguments: &[&str],
    operation: &'static str,
) -> Result<Vec<RepoPath>, SourceError> {
    let output = run_git(root, arguments, operation)?;
    let mut paths = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| parse_path(path, operation))
        .collect::<Result<Vec<_>, _>>()?;
    paths.retain(|path| !is_runtime_path(path));
    paths.sort();
    paths.dedup();
    Ok(paths)
}

pub(super) fn changed_paths(
    root: &Path,
    baseline: &Digest,
    baseline_entries: &BTreeMap<RepoPath, BaselineEntry>,
    tracked_paths: &[RepoPath],
    untracked_paths: &[RepoPath],
) -> Result<Vec<GitWorktreeChange>, SourceError> {
    let output = run_git(
        root,
        &[
            "diff",
            "--name-status",
            "-z",
            "--no-ext-diff",
            "--no-textconv",
            "--find-renames",
            "--ignore-submodules=all",
            baseline.value(),
            "--",
        ],
        "compare worktree to baseline",
    )?;
    let records = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .collect::<Vec<_>>();
    let mut changes = BTreeMap::<RepoPath, GitWorktreeChange>::new();
    let mut index = 0;
    while index < records.len() {
        let status = records[index];
        index += 1;
        let code = *status.first().ok_or(SourceError::GitCommand {
            operation: "parse worktree changes",
        })?;
        match code {
            b'R' => {
                let old = next_change_path(&records, &mut index)?;
                let new = next_change_path(&records, &mut index)?;
                insert_change(&mut changes, old.clone(), OverlayChangeKind::Deleted, None);
                insert_change(&mut changes, new, OverlayChangeKind::Added, Some(old));
            }
            b'C' => {
                let _old = next_change_path(&records, &mut index)?;
                let new = next_change_path(&records, &mut index)?;
                insert_change(&mut changes, new, OverlayChangeKind::Added, None);
            }
            b'A' => {
                let path = next_change_path(&records, &mut index)?;
                insert_change(&mut changes, path, OverlayChangeKind::Added, None);
            }
            b'D' => {
                let path = next_change_path(&records, &mut index)?;
                insert_change(&mut changes, path, OverlayChangeKind::Deleted, None);
            }
            b'M' | b'T' | b'U' => {
                let path = next_change_path(&records, &mut index)?;
                insert_change(&mut changes, path, OverlayChangeKind::Modified, None);
            }
            _ => {
                return Err(SourceError::GitCommand {
                    operation: "parse worktree change status",
                });
            }
        }
    }

    let tracked = tracked_paths.iter().cloned().collect::<BTreeSet<_>>();
    let untracked = untracked_paths.iter().cloned().collect::<BTreeSet<_>>();
    for (path, entry) in baseline_entries {
        if untracked.contains(path) {
            changes.remove(path);
            if !untracked_matches_baseline(root, path, entry)? {
                insert_change(
                    &mut changes,
                    path.clone(),
                    OverlayChangeKind::Modified,
                    None,
                );
            }
        } else if !tracked.contains(path) {
            insert_change(&mut changes, path.clone(), OverlayChangeKind::Deleted, None);
        }
    }

    for path in tracked.iter().chain(untracked.iter()) {
        if !baseline_entries.contains_key(path) {
            changes
                .entry(path.clone())
                .or_insert_with(|| GitWorktreeChange {
                    path: path.clone(),
                    kind: OverlayChangeKind::Added,
                    renamed_from: None,
                });
        }
    }

    Ok(changes.into_values().collect())
}

pub(super) fn next_change_path(
    records: &[&[u8]],
    index: &mut usize,
) -> Result<RepoPath, SourceError> {
    let record = records.get(*index).ok_or(SourceError::GitCommand {
        operation: "parse worktree changes",
    })?;
    *index += 1;
    parse_path(record, "parse worktree change path")
}

pub(super) fn insert_change(
    changes: &mut BTreeMap<RepoPath, GitWorktreeChange>,
    path: RepoPath,
    kind: OverlayChangeKind,
    renamed_from: Option<RepoPath>,
) {
    if !is_runtime_path(&path) {
        changes.insert(
            path.clone(),
            GitWorktreeChange {
                path,
                kind,
                renamed_from,
            },
        );
    }
}

pub(super) fn untracked_matches_baseline(
    root: &Path,
    path: &RepoPath,
    baseline: &BaselineEntry,
) -> Result<bool, SourceError> {
    let filter_path = format!("--path={}", path.as_str());
    let output = run_git(
        root,
        &["hash-object", &filter_path, "--", path.as_str()],
        "hash baseline-seeded worktree path",
    )?;
    let object_id =
        std::str::from_utf8(trim_ascii(&output.stdout)).map_err(|_| SourceError::GitCommand {
            operation: "parse worktree blob identity",
        })?;
    let mode = observed_git_mode(root.join(path.as_str()))?;
    Ok(object_id == baseline.object_id && mode == baseline.mode)
}

pub(super) fn observed_git_mode(path: PathBuf) -> Result<&'static str, SourceError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| SourceError::Io {
        operation: "inspect worktree path mode",
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Ok("120000");
    }

    if !metadata.is_file() {
        return Ok("160000");
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 != 0 {
            return Ok("100755");
        }
    }
    Ok("100644")
}

pub(super) fn parse_path(raw: &[u8], operation: &'static str) -> Result<RepoPath, SourceError> {
    let value = std::str::from_utf8(raw).map_err(|_| SourceError::GitCommand { operation })?;
    RepoPath::new(value).map_err(|_| SourceError::GitCommand { operation })
}

pub(super) fn is_runtime_path(path: &RepoPath) -> bool {
    path.as_str().split('/').any(|component| {
        component.eq_ignore_ascii_case(".git") || component.eq_ignore_ascii_case(".ferrus")
    })
}

pub(super) fn run_git(
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

pub(super) fn run_git_allow_failure(
    root: &Path,
    arguments: &[&str],
) -> Result<Output, SourceError> {
    git_command(root, arguments)
        .output()
        .map_err(|_| SourceError::GitCommand {
            operation: "run Git worktree command",
        })
}

pub(super) fn run_git_with_index(
    root: &Path,
    index: &Path,
    arguments: &[&str],
    operation: &'static str,
) -> Result<Output, SourceError> {
    let output = git_command(root, arguments)
        .env("GIT_INDEX_FILE", index)
        .output()
        .map_err(|_| SourceError::GitCommand { operation })?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(SourceError::GitCommand { operation })
    }
}
