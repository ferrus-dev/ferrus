use std::collections::BTreeSet;

use super::*;
use crate::repository_graph::{
    config::RepositoryGraphConfig,
    domain::{RepoPath, RepositoryId, RepositoryNamespace, RepositoryRef},
    ports::SourceFileMode,
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

#[test]
fn filesystem_manifest_is_sorted_deterministic_and_policy_bounded() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("src")).unwrap();
    fs::create_dir_all(directory.path().join("target/debug")).unwrap();
    fs::create_dir_all(directory.path().join("vendor/pkg")).unwrap();
    fs::create_dir_all(directory.path().join(".ferrus")).unwrap();
    fs::write(directory.path().join("src/z.rs"), b"fn z() {}\n").unwrap();
    fs::write(directory.path().join("src/a.rs"), b"fn a() {}\n").unwrap();
    fs::write(directory.path().join("Cargo.toml"), b"[package]\n").unwrap();
    fs::write(directory.path().join(".env"), b"TOKEN=secret\n").unwrap();
    fs::write(directory.path().join("target/debug/app"), b"generated\n").unwrap();
    fs::write(directory.path().join("vendor/pkg/lib.rs"), b"vendored\n").unwrap();
    fs::write(directory.path().join(".ferrus/state"), b"runtime\n").unwrap();
    fs::write(directory.path().join("image.bin"), b"text\0binary").unwrap();

    let source = FilesystemRepositorySource::discover(
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
    assert_eq!(paths, vec!["Cargo.toml", "src/a.rs", "src/z.rs"]);
    assert!(source.manifest().revision.base_revision.is_none());
    assert_eq!(
        source.manifest().revision.source_kind,
        SourceKind::NonGitManifest
    );
    assert!(source.manifest().diagnostics.iter().any(|diagnostic| {
        diagnostic.code.as_str() == "sensitive_path_excluded"
            && diagnostic.path.as_ref().map(RepoPath::as_str) == Some(".env")
    }));
    assert!(
        source
            .manifest()
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code.as_str() == "binary_file_skipped")
    );

    let repeated = FilesystemRepositorySource::discover(
        directory.path(),
        context(&RepositoryGraphConfig::default()),
    )
    .unwrap();
    assert_eq!(source.manifest(), repeated.manifest());
}

#[test]
fn nested_runtime_metadata_and_case_variants_are_excluded() {
    let directory = tempfile::tempdir().unwrap();
    for relative in ["nested/.git", "nested/.FERRUS"] {
        fs::create_dir_all(directory.path().join(relative)).unwrap();
    }
    fs::write(directory.path().join("nested/.git/config"), b"metadata\n").unwrap();
    fs::write(directory.path().join("nested/.FERRUS/state"), b"runtime\n").unwrap();
    fs::write(directory.path().join(".ENV"), b"TOKEN=secret\n").unwrap();
    fs::write(directory.path().join("SECRET.PEM"), b"secret\n").unwrap();
    fs::write(directory.path().join("safe.rs"), b"fn safe() {}\n").unwrap();

    let source = FilesystemRepositorySource::discover(
        directory.path(),
        context(&RepositoryGraphConfig::default()),
    )
    .unwrap();
    assert_eq!(
        source
            .manifest()
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["safe.rs"]
    );
}

#[test]
fn local_source_uses_filesystem_fallback_without_git_metadata() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("main.rs"), b"fn main() {}\n").unwrap();

    let source = crate::repository_graph::source::LocalRepositorySource::discover(
        directory.path(),
        context(&RepositoryGraphConfig::default()),
    )
    .unwrap();
    assert!(matches!(
        source,
        crate::repository_graph::source::LocalRepositorySource::Filesystem(_)
    ));
}

#[test]
fn generated_and_vendor_toggles_are_effective() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("target")).unwrap();
    fs::create_dir_all(directory.path().join("vendor")).unwrap();
    fs::write(directory.path().join("target/generated.rs"), b"generated\n").unwrap();
    fs::write(directory.path().join("vendor/dependency.rs"), b"vendor\n").unwrap();
    let mut config = RepositoryGraphConfig::default();
    config.source.include_generated = true;
    config.source.include_vendor = true;

    let source = FilesystemRepositorySource::discover(directory.path(), context(&config)).unwrap();
    let paths = source
        .manifest()
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        paths,
        BTreeSet::from(["target/generated.rs", "vendor/dependency.rs"])
    );
}

#[test]
fn content_policy_and_mode_participate_in_manifest_identity() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("main.rs");
    fs::write(&path, b"fn main() {}\n").unwrap();
    let baseline = FilesystemRepositorySource::discover(
        directory.path(),
        context(&RepositoryGraphConfig::default()),
    )
    .unwrap();
    let baseline_digest = baseline.manifest().revision.manifest_digest.clone();

    fs::write(&path, b"fn main() { println!(\"changed\"); }\n").unwrap();
    let changed = FilesystemRepositorySource::discover(
        directory.path(),
        context(&RepositoryGraphConfig::default()),
    )
    .unwrap();
    assert_ne!(baseline_digest, changed.manifest().revision.manifest_digest);

    let mut config = RepositoryGraphConfig::default();
    config.source.include = BTreeSet::from(["main.rs".to_string()]);
    let policy_changed =
        FilesystemRepositorySource::discover(directory.path(), context(&config)).unwrap();
    assert_ne!(
        changed.manifest().revision.manifest_digest,
        policy_changed.manifest().revision.manifest_digest
    );
    assert_eq!(
        policy_changed.manifest().files[0].file_mode,
        SourceFileMode::Regular
    );
}

#[test]
fn file_and_total_limits_are_enforced() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("a.txt"), b"a").unwrap();
    fs::write(directory.path().join("b.txt"), b"b").unwrap();

    let mut file_limited = RepositoryGraphConfig::default();
    file_limited.index_limits.max_files = 1;
    assert!(matches!(
        FilesystemRepositorySource::discover(directory.path(), context(&file_limited)),
        Err(SourceError::FileLimitExceeded { limit: 1 })
    ));

    let mut byte_limited = RepositoryGraphConfig::default();
    byte_limited.index_limits.max_total_bytes = 1;
    assert!(matches!(
        FilesystemRepositorySource::discover(directory.path(), context(&byte_limited)),
        Err(SourceError::TotalBytesLimitExceeded { limit: 1 })
    ));

    let mut directory_limited = RepositoryGraphConfig::default();
    directory_limited.index_limits.max_directories = 1;
    fs::create_dir(directory.path().join("nested")).unwrap();
    assert!(matches!(
        FilesystemRepositorySource::discover(directory.path(), context(&directory_limited)),
        Err(SourceError::DirectoryLimitExceeded { limit: 1 })
    ));
}

#[test]
fn include_filters_prune_unmatchable_trees_before_hard_limits() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("src")).unwrap();
    fs::create_dir_all(directory.path().join("docs/one/two/three")).unwrap();
    fs::write(
        directory.path().join("src/lib.rs"),
        b"pub struct Included;\n",
    )
    .unwrap();
    for index in 0..4 {
        fs::write(
            directory.path().join(format!("docs/one/file-{index}.md")),
            b"excluded\n",
        )
        .unwrap();
    }
    let mut config = RepositoryGraphConfig::default();
    config.source.include = BTreeSet::from(["src/**".to_string()]);
    config.index_limits.max_files = 1;
    config.index_limits.max_directories = 2;

    let source = FilesystemRepositorySource::discover(directory.path(), context(&config))
        .expect("unmatchable trees must not consume discovery limits");
    assert_eq!(
        source
            .manifest()
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/lib.rs"]
    );
    assert_eq!(source.manifest().metrics.candidates, 1);
    assert_eq!(source.manifest().metrics.directories, 2);
    assert!(source.manifest().diagnostics.iter().any(|diagnostic| {
        diagnostic.code.as_str() == "path_not_included"
            && diagnostic.path.as_ref().map(RepoPath::as_str) == Some("docs")
    }));
}

#[test]
fn per_file_and_diagnostic_limits_produce_bounded_skips() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("large.txt"), b"too large").unwrap();
    fs::write(directory.path().join("binary-one"), b"a\0b").unwrap();
    fs::write(directory.path().join("binary-two"), b"c\0d").unwrap();
    let mut config = RepositoryGraphConfig::default();
    config.index_limits.max_file_bytes = 4;
    config.index_limits.max_diagnostics = 1;

    let source = FilesystemRepositorySource::discover(directory.path(), context(&config)).unwrap();
    assert!(source.manifest().files.is_empty());
    assert_eq!(source.manifest().diagnostics.len(), 1);
    assert_eq!(source.manifest().metrics.skipped, 3);
    assert_eq!(source.manifest().metrics.suppressed_diagnostics, 2);
}

#[test]
fn binary_detection_covers_the_complete_bounded_file() {
    let directory = tempfile::tempdir().unwrap();
    let mut bytes = vec![b'a'; 9 * 1024];
    bytes.push(0);
    fs::write(directory.path().join("late-nul.bin"), bytes).unwrap();

    let source = FilesystemRepositorySource::discover(
        directory.path(),
        context(&RepositoryGraphConfig::default()),
    )
    .unwrap();
    assert!(source.manifest().files.is_empty());
    assert!(source.manifest().diagnostics.iter().any(|diagnostic| {
        diagnostic.code.as_str() == "binary_file_skipped"
            && diagnostic.path.as_ref().map(RepoPath::as_str) == Some("late-nul.bin")
    }));
}

#[test]
fn aggregate_limit_counts_binary_bytes_that_were_inspected() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("one.bin"), b"a\0b").unwrap();
    fs::write(directory.path().join("two.bin"), b"c\0d").unwrap();
    let mut config = RepositoryGraphConfig::default();
    config.index_limits.max_total_bytes = 5;

    assert!(matches!(
        FilesystemRepositorySource::discover(directory.path(), context(&config)),
        Err(SourceError::TotalBytesLimitExceeded { limit: 5 })
    ));
}

#[test]
fn verified_reads_reject_content_changed_after_discovery() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("main.rs");
    fs::write(&path, b"fn main() {}\n").unwrap();
    let source = FilesystemRepositorySource::discover(
        directory.path(),
        context(&RepositoryGraphConfig::default()),
    )
    .unwrap();
    let file = source.manifest().files[0].clone();
    assert!(source.revalidate().unwrap());
    assert_eq!(
        source.read_verified(&file).unwrap().bytes,
        b"fn main() {}\n"
    );

    fs::write(&path, b"fn changed() {}\n").unwrap();
    assert!(!source.revalidate().unwrap());
    assert!(matches!(
        source.read_verified(&file),
        Err(SourceError::ContentChanged)
    ));

    fs::remove_file(&path).unwrap();
    assert!(matches!(
        source.read_verified(&file),
        Err(SourceError::ContentChanged)
    ));
}

#[test]
fn excluded_content_does_not_change_manifest_identity() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("target")).unwrap();
    fs::write(directory.path().join("included.rs"), b"included\n").unwrap();
    let excluded = directory.path().join("target/generated.rs");
    fs::write(&excluded, b"first\n").unwrap();
    let before = FilesystemRepositorySource::discover(
        directory.path(),
        context(&RepositoryGraphConfig::default()),
    )
    .unwrap();

    fs::write(&excluded, b"second\n").unwrap();
    let after = FilesystemRepositorySource::discover(
        directory.path(),
        context(&RepositoryGraphConfig::default()),
    )
    .unwrap();
    assert_eq!(
        before.manifest().revision.manifest_digest,
        after.manifest().revision.manifest_digest
    );
}

#[cfg(unix)]
#[test]
fn executable_mode_changes_manifest_identity() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("script.sh");
    fs::write(&path, b"#!/bin/sh\n").unwrap();
    let regular = FilesystemRepositorySource::discover(
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
    let executable = FilesystemRepositorySource::discover(
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
    assert!(matches!(
        regular.read_verified(&regular.manifest().files[0]),
        Err(SourceError::ContentChanged)
    ));
}

#[cfg(unix)]
#[test]
fn external_symlink_is_never_followed() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    let outside_directory = tempfile::tempdir().unwrap();
    fs::write(outside.path(), b"outside secret\n").unwrap();
    fs::write(
        outside_directory.path().join("outside-name.rs"),
        b"outside secret\n",
    )
    .unwrap();
    symlink(outside.path(), directory.path().join("outside-link")).unwrap();
    symlink(
        outside_directory.path(),
        directory.path().join("outside-directory-link"),
    )
    .unwrap();

    let source = FilesystemRepositorySource::discover(
        directory.path(),
        context(&RepositoryGraphConfig::default()),
    )
    .unwrap();
    assert!(source.manifest().files.is_empty());
    assert!(
        source
            .manifest()
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code.as_str() == "symlink_skipped")
    );
    assert!(source.manifest().diagnostics.iter().all(|diagnostic| {
        !diagnostic
            .path
            .as_ref()
            .is_some_and(|path| path.as_str().contains("outside-name.rs"))
    }));
}

#[cfg(unix)]
#[test]
fn directory_enumeration_remains_bound_to_the_opened_handle() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("repository");
    let moved = parent.path().join("moved");
    let outside = parent.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("nested")).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(root.join("nested/original.rs"), b"original\n").unwrap();
    fs::write(outside.join("outside.rs"), b"outside\n").unwrap();

    let source_root = SourceRoot::new(fs::canonicalize(&root).unwrap()).unwrap();
    let nested = RepoPath::new("nested").unwrap();
    let directory = source_root.open_directory(&nested).unwrap();
    fs::rename(root.join("nested"), &moved).unwrap();
    symlink(&outside, root.join("nested")).unwrap();

    let mut names = Vec::new();
    visit_directory_entries(&directory, |name, _| {
        names.push(name);
        Ok(())
    })
    .unwrap();
    assert_eq!(names, vec![OsString::from("original.rs")]);
}

#[cfg(unix)]
#[test]
fn special_files_are_skipped_without_opening_them() {
    use std::ffi::CString;

    let directory = tempfile::tempdir().unwrap();
    let fifo = directory.path().join("events.pipe");
    let raw = CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: the path is NUL-terminated and the temporary root is writable.
    assert_eq!(unsafe { libc::mkfifo(raw.as_ptr(), 0o600) }, 0);

    let source = FilesystemRepositorySource::discover(
        directory.path(),
        context(&RepositoryGraphConfig::default()),
    )
    .unwrap();
    assert!(source.manifest().files.is_empty());
    assert!(source.manifest().diagnostics.iter().any(|diagnostic| {
        diagnostic.code.as_str() == "special_file_skipped"
            && diagnostic.path.as_ref().map(RepoPath::as_str) == Some("events.pipe")
    }));
}

#[cfg(target_os = "linux")]
#[test]
fn non_utf8_paths_are_skipped_with_a_bounded_diagnostic() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory
            .path()
            .join(OsString::from_vec(b"invalid-\xff".to_vec())),
        b"content\n",
    )
    .unwrap();

    let source = FilesystemRepositorySource::discover(
        directory.path(),
        context(&RepositoryGraphConfig::default()),
    )
    .unwrap();
    assert!(source.manifest().files.is_empty());
    assert!(source.manifest().diagnostics.iter().any(|diagnostic| {
        diagnostic.code.as_str() == "path_encoding_unsupported" && diagnostic.path.is_none()
    }));
}

#[cfg(unix)]
#[test]
fn verified_reads_remain_bound_to_the_opened_source_root() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("repository");
    let moved = parent.path().join("repository-moved");
    let outside = parent.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(root.join("main.rs"), b"fn original() {}\n").unwrap();
    fs::write(outside.join("main.rs"), b"outside secret\n").unwrap();
    let source =
        FilesystemRepositorySource::discover(&root, context(&RepositoryGraphConfig::default()))
            .unwrap();
    let file = source.manifest().files[0].clone();

    fs::rename(&root, &moved).unwrap();
    symlink(&outside, &root).unwrap();

    assert!(!source.revalidate().unwrap());
    assert_eq!(
        source.read_verified(&file).unwrap().bytes,
        b"fn original() {}\n"
    );
}
