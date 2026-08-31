use super::*;
use crate::repository_graph::domain::{
    RepositoryId, RepositoryNamespace, SourcePosition, SourceSpan,
};

fn test_repository() -> RepositoryRef {
    RepositoryRef {
        namespace: RepositoryNamespace::new("local:test").unwrap(),
        repository_id: RepositoryId::new("root").unwrap(),
    }
}

fn descriptor(path: &str, bytes: &[u8]) -> SourceFileDescriptor {
    SourceFileDescriptor {
        path: RepoPath::new(path).unwrap(),
        content_identity: sha256_digest(bytes),
        byte_len: bytes.len() as u64,
        file_mode: SourceFileMode::Regular,
    }
}

fn content_request(file: &SourceFileDescriptor) -> ContentRequest {
    ContentRequest {
        wire_version: super::super::QUERY_WIRE_VERSION,
        repository: test_repository(),
        snapshot_id: SnapshotId::new("snapshot-1").unwrap(),
        path: file.path.clone(),
        expected_content_identity: file.content_identity.clone(),
        span: None,
        max_bytes: NonZeroU64::new(1024).unwrap(),
    }
}

#[test]
fn globstar_matches_root_and_nested_paths() {
    assert!(glob_matches("**/*", "Cargo.toml"));
    assert!(glob_matches("**/*", "src/main.rs"));
    assert!(glob_matches("**/.env", ".env"));
    assert!(glob_matches("**/.env", "nested/.env"));
    assert!(!glob_matches("src/**", "Cargo.toml"));
}

#[test]
fn include_globs_identify_only_directories_with_possible_descendants() {
    assert!(glob_may_match_descendant("src/**", "src"));
    assert!(glob_may_match_descendant("src/**", "src/nested"));
    assert!(!glob_may_match_descendant("src/**", "docs"));
    assert!(glob_may_match_descendant(
        "crates/*/Cargo.toml",
        "crates/example"
    ));
    assert!(!glob_may_match_descendant(
        "crates/*/Cargo.toml",
        "crates/example/nested"
    ));
    assert!(glob_may_match_descendant("Cargo.toml", "any/nesting"));
    assert!(glob_may_match_descendant("**/src/**", "docs"));
}

#[test]
fn ordered_rules_use_the_last_matching_rule() {
    let config = SourceConfig {
        rules: vec!["src/**".to_string(), "!src/keep.rs".to_string()],
        ..SourceConfig::default()
    };
    let policy = SourcePolicy::new(&config).unwrap();
    assert_eq!(
        policy.exclusion_for_file(&RepoPath::new("src/drop.rs").unwrap()),
        Some("source_rule_excluded")
    );
    assert_eq!(
        policy.exclusion_for_file(&RepoPath::new("src/keep.rs").unwrap()),
        None
    );
}

#[test]
fn extractor_set_identity_is_order_independent() {
    let rust = ExtractorIdentity {
        id: crate::repository_graph::domain::ExtractorId::new("rust").unwrap(),
        version: "1.0.0".to_string(),
        contract_version: 1,
    };
    let cargo = ExtractorIdentity {
        id: crate::repository_graph::domain::ExtractorId::new("cargo").unwrap(),
        version: "2.0.0".to_string(),
        contract_version: 1,
    };
    assert_eq!(
        extractor_set_digest(&[rust.clone(), cargo.clone()]),
        extractor_set_digest(&[cargo, rust])
    );
}

#[test]
fn windows_path_keys_reject_aliases_and_reserved_names() {
    let upper = RepoPath::new("src/Foo.rs").unwrap();
    let lower = RepoPath::new("src/foo.rs").unwrap();
    assert_eq!(windows_path_key(&upper), windows_path_key(&lower));
    for path in [
        "CON",
        "aux.txt",
        "src/name.",
        "src/name ",
        "src/file:stream",
    ] {
        assert!(windows_path_key(&RepoPath::new(path).unwrap()).is_none());
    }
}

#[test]
fn windows_final_path_prefixes_compare_with_canonical_paths() {
    assert!(windows_paths_equal(Path::new(r"C:\repo"), r"\\?\C:\repo"));
    assert!(windows_paths_equal(
        Path::new(r"\\server\share\repo"),
        r"\\?\UNC\server\share\repo"
    ));
    assert!(windows_paths_equal(Path::new("C:/Repo/"), r"\\?\c:\repo"));
    assert!(!windows_paths_equal(Path::new(r"C:\repo"), r"\\?\C:\other"));
}

#[test]
fn bounded_reads_report_bytes_consumed_before_an_io_error() {
    struct PartialThenError(bool);

    impl Read for PartialThenError {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.0 {
                return Err(io::Error::other("injected read failure"));
            }
            self.0 = true;
            buffer[..3].copy_from_slice(b"abc");
            Ok(3)
        }
    }

    let error = read_bounded(PartialThenError(false), 10).unwrap_err();
    assert_eq!(error.inspected, 3);
    let mut metrics = SourceDiscoveryMetrics {
        total_bytes: 4,
        ..SourceDiscoveryMetrics::default()
    };
    assert!(matches!(
        account_inspected_bytes(&mut metrics, error.inspected, 6),
        Err(SourceError::TotalBytesLimitExceeded { limit: 6 })
    ));
}

#[test]
fn snapshot_content_confines_hash_verifies_and_bounds_spans() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("src")).unwrap();
    let bytes = b"abcdef";
    std::fs::write(directory.path().join("src/lib.rs"), bytes).unwrap();
    let file = descriptor("src/lib.rs", bytes);
    let reader = LocalSnapshotContent::new(
        directory.path(),
        test_repository(),
        SnapshotId::new("snapshot-1").unwrap(),
        &SourceConfig::default(),
        vec![file.clone()],
        NonZeroU64::new(3).unwrap(),
    )
    .unwrap();
    let mut request = content_request(&file);
    request.span = Some(SourceSpan {
        start: SourcePosition {
            byte_offset: 1,
            line: Some(1),
            column: Some(2),
        },
        end: SourcePosition {
            byte_offset: 5,
            line: Some(1),
            column: Some(6),
        },
    });

    let response = reader.read_verified(&request).unwrap();
    assert_eq!(response.bytes, b"bcd");
    assert!(response.truncated);

    std::fs::write(directory.path().join("src/lib.rs"), b"changed").unwrap();
    assert_eq!(
        reader.read_verified(&request).unwrap_err().code,
        QueryErrorCode::ContentChanged
    );
}

#[test]
fn snapshot_content_clamps_byte_limits_to_utf8_boundaries() {
    let directory = tempfile::tempdir().unwrap();
    let bytes = "aéz".as_bytes();
    std::fs::write(directory.path().join("unicode.rs"), bytes).unwrap();
    let file = descriptor("unicode.rs", bytes);

    for (hard_limit, request_limit) in [(2, 1024), (1024, 2)] {
        let reader = LocalSnapshotContent::new(
            directory.path(),
            test_repository(),
            SnapshotId::new("snapshot-1").unwrap(),
            &SourceConfig::default(),
            vec![file.clone()],
            NonZeroU64::new(hard_limit).unwrap(),
        )
        .unwrap();
        let mut request = content_request(&file);
        request.max_bytes = NonZeroU64::new(request_limit).unwrap();

        let response = reader.read_verified(&request).unwrap();

        assert_eq!(std::str::from_utf8(&response.bytes).unwrap(), "a");
        assert!(response.truncated);
    }
}

#[test]
fn snapshot_content_denies_sensitive_paths_before_reading() {
    let directory = tempfile::tempdir().unwrap();
    let file = descriptor(".env", b"SECRET=value");
    std::fs::write(directory.path().join(".env"), b"SECRET=value").unwrap();
    let reader = LocalSnapshotContent::new(
        directory.path(),
        test_repository(),
        SnapshotId::new("snapshot-1").unwrap(),
        &SourceConfig::default(),
        vec![file.clone()],
        NonZeroU64::new(1024).unwrap(),
    )
    .unwrap();

    assert_eq!(
        reader
            .read_verified(&content_request(&file))
            .unwrap_err()
            .code,
        QueryErrorCode::ContentUnavailable
    );
}

#[test]
fn frozen_git_tree_content_survives_worktree_changes() {
    let directory = tempfile::tempdir().unwrap();
    let status = std::process::Command::new("git")
        .arg("init")
        .arg("--quiet")
        .arg(directory.path())
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::create_dir_all(directory.path().join("src")).unwrap();
    let original = b"pub struct Submitted;\n";
    std::fs::write(directory.path().join("src/lib.rs"), original).unwrap();
    let tree = capture_worktree_tree(directory.path()).unwrap();
    let file = descriptor("src/lib.rs", original);
    let reader = GitTreeSnapshotContent::new(
        directory.path(),
        test_repository(),
        SnapshotId::new("snapshot-1").unwrap(),
        tree,
        &SourceConfig::default(),
        vec![file.clone()],
        NonZeroU64::new(1024).unwrap(),
    )
    .unwrap();

    std::fs::write(
        directory.path().join("src/lib.rs"),
        b"pub struct Addressing;\n",
    )
    .unwrap();

    let response = reader.read_verified(&content_request(&file)).unwrap();
    assert_eq!(response.bytes, original);
}

#[test]
fn task_baseline_discovery_and_reads_ignore_worktree_changes() {
    let directory = tempfile::tempdir().unwrap();
    let status = std::process::Command::new("git")
        .arg("init")
        .arg("--quiet")
        .arg(directory.path())
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::create_dir_all(directory.path().join("src")).unwrap();
    let original = b"pub struct Baseline;\n";
    std::fs::write(directory.path().join("src/lib.rs"), original).unwrap();
    let tree = capture_worktree_tree(directory.path()).unwrap();
    let context = SourceDiscoveryContext::from_config(
        test_repository(),
        &RepositoryGraphConfig::default(),
        &[],
    )
    .unwrap();

    std::fs::write(
        directory.path().join("src/lib.rs"),
        b"pub struct ExecutorEdit;\n",
    )
    .unwrap();
    let source = TaskBaselineSource::discover(directory.path(), context, tree).unwrap();
    let file = source
        .manifest()
        .files
        .iter()
        .find(|file| file.path.as_str() == "src/lib.rs")
        .unwrap();

    assert_eq!(source.read_verified(file).unwrap().bytes, original);
    assert!(source.revalidate().unwrap());
    assert_eq!(
        source.manifest().revision.source_kind,
        SourceKind::TaskBaseline
    );
}

#[cfg(unix)]
#[test]
fn snapshot_content_rejects_symlink_replacement() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(outside.path(), b"outside").unwrap();
    let file = descriptor("link.rs", b"outside");
    symlink(outside.path(), directory.path().join("link.rs")).unwrap();
    let reader = LocalSnapshotContent::new(
        directory.path(),
        test_repository(),
        SnapshotId::new("snapshot-1").unwrap(),
        &SourceConfig::default(),
        vec![file.clone()],
        NonZeroU64::new(1024).unwrap(),
    )
    .unwrap();

    assert_eq!(
        reader
            .read_verified(&content_request(&file))
            .unwrap_err()
            .code,
        QueryErrorCode::ContentChanged
    );
}
