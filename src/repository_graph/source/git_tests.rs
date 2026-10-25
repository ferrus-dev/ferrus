//! Git discovery policy, path normalization, and worktree revalidation.

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
fn git_commands_do_not_inherit_host_stdin() {
    use std::{
        io::Write,
        thread,
        time::{Duration, Instant},
    };

    const CHILD: &str = "FERRUS_TEST_GRAPH_GIT_STDIN";
    if std::env::var_os(CHILD).is_some() {
        let directory = initialized_repository();
        let (ready, received) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut input = BufReader::new(std::io::stdin());
            let mut line = String::new();
            input.read_line(&mut line).unwrap();
            ready.send(()).unwrap();
            // Match Nano's blocked control reader while Git is spawned.
            let _ = std::io::copy(&mut input, &mut std::io::sink());
        });
        received.recv_timeout(Duration::from_secs(5)).unwrap();
        // Batch mode waits for EOF. It must see a closed input even when
        // the Ferrus host is waiting on an open control pipe.
        let mut git = git_command(directory.path(), &["cat-file", "--batch"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = git.try_wait().unwrap() {
                assert!(status.success());
                return;
            }

            if Instant::now() >= deadline {
                let _ = git.kill();
                let _ = git.wait();
                panic!("graph Git command inherited the open host stdin");
            }

            thread::sleep(Duration::from_millis(10));
        }
    }

    let output = tempfile::NamedTempFile::new().unwrap();
    let log = output.reopen().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "repository_graph::source::git::tests::git_commands_do_not_inherit_host_stdin",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .stdin(Stdio::piped())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(input, "host control frame").unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }

        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("graph stdin regression child did not finish");
        }

        thread::sleep(Duration::from_millis(10));
    };
    drop(input);
    assert!(
        status.success(),
        "{}",
        fs::read_to_string(output.path()).unwrap()
    );
}

#[test]
fn git_discovery_includes_tracked_and_nonignored_untracked_files() {
    let directory = initialized_repository();
    fs::write(directory.path().join(".gitignore"), b"ignored.log\n").unwrap();
    fs::write(directory.path().join("tracked.rs"), b"fn tracked() {}\n").unwrap();
    git(directory.path(), &["add", ".gitignore", "tracked.rs"]);
    git(directory.path(), &["commit", "-m", "initial"]);

    let clean =
        GitRepositorySource::discover(directory.path(), context(&RepositoryGraphConfig::default()))
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
    let source =
        GitRepositorySource::discover(directory.path(), context(&RepositoryGraphConfig::default()))
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

    let source =
        GitRepositorySource::discover(directory.path(), context(&RepositoryGraphConfig::default()))
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
    let clean =
        GitRepositorySource::discover(directory.path(), context(&RepositoryGraphConfig::default()))
            .unwrap();

    fs::write(directory.path().join(".env"), b"TOKEN=secret\n").unwrap();
    assert!(!clean.revalidate().unwrap());
    let excluded =
        GitRepositorySource::discover(directory.path(), context(&RepositoryGraphConfig::default()))
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
    let initial =
        GitRepositorySource::discover(directory.path(), context(&RepositoryGraphConfig::default()))
            .unwrap();

    fs::write(directory.path().join(".env"), b"TOKEN=secret\n").unwrap();
    git(directory.path(), &["add", "-f", ".env"]);
    git(directory.path(), &["commit", "-m", "excluded metadata"]);
    let changed =
        GitRepositorySource::discover(directory.path(), context(&RepositoryGraphConfig::default()))
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

    let source =
        GitRepositorySource::discover(directory.path(), context(&RepositoryGraphConfig::default()))
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

    let source =
        GitRepositorySource::discover(directory.path(), context(&RepositoryGraphConfig::default()))
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

    let source =
        GitRepositorySource::discover(directory.path(), context(&RepositoryGraphConfig::default()))
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
    let regular =
        GitRepositorySource::discover(directory.path(), context(&RepositoryGraphConfig::default()))
            .unwrap();
    assert_eq!(
        regular.manifest().files[0].file_mode,
        SourceFileMode::Regular
    );

    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    let executable =
        GitRepositorySource::discover(directory.path(), context(&RepositoryGraphConfig::default()))
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
