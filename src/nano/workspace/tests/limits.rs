//! Tests for limits.

use super::*;

#[test]
fn range_reads_preserve_bytes_identity_and_explicit_limits() {
    let (dir, workspace) = setup();
    let text = "one\r\ncaf\u{e9}\r\nlast";
    disk::write(dir.path().join("source.txt"), text).unwrap();
    let result = workspace
        .read_file(
            serde_json::from_value(json!({"path":"source.txt","start_line":2,"max_lines":1}))
                .unwrap(),
        )
        .unwrap();
    assert_eq!(result.text, "caf\u{e9}\r\n");
    assert!(result.truncated);
    assert_eq!(result.next_line, Some(3));
    assert_eq!(result.source.digest, digest(text.as_bytes()));
    assert_eq!(result.source.kind, "workspace");
    let short = workspace
        .read_file(serde_json::from_value(json!({"path":"source.txt","max_bytes":2})).unwrap())
        .unwrap();
    assert!(short.text.is_empty() && short.truncated);
    assert_eq!(short.next_line, Some(1));
    let past = workspace
        .read_file(serde_json::from_value(json!({"path":"source.txt","start_line":100})).unwrap())
        .unwrap();
    assert!(past.text.is_empty() && !past.truncated);
    let (_other, other) = setup();
    assert_ne!(workspace.id, other.id);
}

#[tokio::test]
async fn overlapping_search_paths_do_not_repeat_results_or_charge_budgets() {
    let (dir, mut workspace) = setup();
    disk::create_dir(dir.path().join("src")).unwrap();
    let content = "needle once\n";
    disk::write(dir.path().join("src/File.rs"), content).unwrap();
    workspace.limits.entries = 3; // root, directory, file

    #[cfg(windows)]
    let paths = [".", "SRC", "src", "SRC/FILE.RS", "src/File.rs"];
    #[cfg(unix)]
    let paths = [".", "src", "src/File.rs"];
    let result = workspace
        .search_text(
            serde_json::from_value(json!({"paths":paths,"query":"needle"})).unwrap(),
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert!(!result.truncated);
    assert!(result.issues.is_empty());
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.scanned_bytes, content.len());
    assert_eq!(result.visited_entries, 3);
    assert_eq!(result.listed_entries, 2);
    assert_eq!(result.matches[0].source.digest, digest(content.as_bytes()));
    #[cfg(windows)]
    assert_eq!(result.matches[0].source.path, "SRC/FILE.RS");
    #[cfg(unix)]
    assert_eq!(result.matches[0].source.path, "src/File.rs");

    #[cfg(windows)]
    let paths = ["src/File.rs", "SRC/FILE.RS"];
    #[cfg(unix)]
    let paths = ["src/File.rs", "src/File.rs"];
    let result = workspace
        .search_text(
            serde_json::from_value(json!({"paths":paths,"query":"needle"})).unwrap(),
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert!(!result.truncated);
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].source.path, "src/File.rs");
    assert_eq!(result.scanned_bytes, content.len());
    assert_eq!(result.visited_entries, 1);
    assert_eq!(result.listed_entries, 0);
}

#[tokio::test]
async fn literal_search_is_sorted_bounded_and_reports_skips() {
    let (dir, mut workspace) = setup();
    disk::create_dir(dir.path().join("src")).unwrap();
    disk::write(dir.path().join("src/b.rs"), "needle second\r\nneedle third").unwrap();
    disk::write(dir.path().join("src/a.rs"), "caf\u{e9} needle first\n").unwrap();
    disk::write(dir.path().join("binary"), [0, 1, 2]).unwrap();
    disk::create_dir(dir.path().join(".git")).unwrap();
    disk::write(dir.path().join(".git/config"), "needle protected").unwrap();
    let request = || {
        serde_json::from_value(json!({"paths":["src"],"query":"needle","max_results":2})).unwrap()
    };
    let result = workspace
        .search_text(request(), &Cancellation::default())
        .await
        .unwrap();
    assert_eq!(result.matches.len(), 2);
    assert_eq!(result.matches[0].source.path, "src/a.rs");
    assert_eq!(result.matches[0].byte_column, 7);
    assert_eq!(
        result.matches[1].source.digest,
        digest(b"needle second\r\nneedle third")
    );
    assert!(result.truncated);
    workspace.limits.entries = 2;
    let result = workspace
        .search_text(request(), &Cancellation::default())
        .await
        .unwrap();
    assert!(result.truncated && result.visited_entries <= 2);
    workspace.limits = Limits::default();
    let result = workspace
        .search_text(
            serde_json::from_value(json!({"query":"needle"})).unwrap(),
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.matches.len(), 3);
    assert_eq!(result.issues[0].code, Code::Binary);
    assert!(result.truncated);
    assert!(
        result
            .matches
            .iter()
            .all(|m| !m.source.path.starts_with(".git"))
    );
}

#[tokio::test]
async fn search_uses_the_full_remaining_scan_budget() {
    for (file_bytes, scan_bytes, sizes) in [
        (8, 64, vec![8; 8]),
        (8, 13, vec![8, 5]),
        (8, 9, vec![8, 1]),
        (8, 8, vec![8]),
    ] {
        let (dir, mut workspace) = setup();
        workspace.limits.file_bytes = file_bytes;
        workspace.limits.scan_bytes = scan_bytes;
        for (i, size) in sizes.iter().enumerate() {
            disk::write(dir.path().join(format!("file{i}")), "x".repeat(*size)).unwrap();
        }

        let result = workspace
            .search_text(
                serde_json::from_value(json!({"query":"x"})).unwrap(),
                &Cancellation::default(),
            )
            .await
            .unwrap();
        assert!(!result.truncated, "{result:?}");
        assert!(result.issues.is_empty(), "{result:?}");
        assert_eq!(result.scanned_bytes, scan_bytes);
        assert_eq!(result.matches.len(), sizes.len());
        assert_eq!(
            result.matches.last().unwrap().source.path,
            format!("file{}", sizes.len() - 1)
        );
    }
}

#[tokio::test]
async fn search_skips_oversized_files_without_spending_the_remaining_budget() {
    for (scan_bytes, sizes, expected_paths, rejected) in [
        (8, vec![9, 8], vec!["file1"], "file0"),
        (9, vec![8, 2, 1], vec!["file0", "file2"], "file1"),
        (8, vec![8, 1], vec!["file0"], "file1"),
    ] {
        let (dir, mut workspace) = setup();
        workspace.limits.file_bytes = 8;
        workspace.limits.scan_bytes = scan_bytes;
        for (i, size) in sizes.iter().enumerate() {
            disk::write(dir.path().join(format!("file{i}")), "x".repeat(*size)).unwrap();
        }

        let result = workspace
            .search_text(
                serde_json::from_value(json!({"query":"x"})).unwrap(),
                &Cancellation::default(),
            )
            .await
            .unwrap();
        assert!(result.truncated);
        assert_eq!(result.scanned_bytes, scan_bytes);
        assert_eq!(result.issues.len(), 1);
        assert_eq!(result.issues[0].code, Code::FileTooLarge);
        assert_eq!(result.issues[0].path, rejected);
        assert_eq!(
            result
                .matches
                .iter()
                .map(|hit| hit.source.path.as_str())
                .collect::<Vec<_>>(),
            expected_paths
        );
    }
}

#[tokio::test]
async fn search_caps_content_and_serialized_output_even_with_long_escaped_lines() {
    let (dir, mut workspace) = setup();
    for i in 0..20 {
        disk::write(
            dir.path().join(format!("file{i:02}")),
            format!("hit {}\n", "\u{1}".repeat(1000)),
        )
        .unwrap();
    }

    workspace.limits.output_bytes = 4096;
    let request = || serde_json::from_value(json!({"query":"hit"})).unwrap();
    let result = workspace
        .search_text(request(), &Cancellation::default())
        .await
        .unwrap();
    assert!(result.truncated);
    assert!(encode(&result, 4096).is_ok());
    workspace.limits.file_bytes = 512;
    workspace.limits.scan_bytes = 1024;
    let result = workspace
        .search_text(request(), &Cancellation::default())
        .await
        .unwrap();
    assert!(result.truncated && result.scanned_bytes <= 1024);
    assert!(result.matches.is_empty());
}

#[tokio::test]
async fn binary_oversized_directory_and_duplicate_targets_cannot_be_patched() {
    let (dir, mut workspace) = setup();
    disk::write(dir.path().join("binary"), [0, 1]).unwrap();
    disk::create_dir(dir.path().join("directory")).unwrap();
    let result = apply(
        &mut workspace,
        vec![Edit::Delete {
            path: "binary".into(),
            expected_digest: digest(&[0, 1]),
        }],
    )
    .await;
    assert_eq!(result.failure.unwrap().code, Code::Binary);
    assert_eq!(
        workspace.content("directory").unwrap_err().code,
        Code::UnsafeFile
    );
    workspace.limits.file_bytes = 4;
    disk::write(dir.path().join("large"), "12345").unwrap();
    assert_eq!(
        workspace.content("large").unwrap_err().code,
        Code::FileTooLarge
    );
    let result = apply(
        &mut workspace,
        vec![Edit::Create {
            path: "x".into(),
            content: "12345".into(),
        }],
    )
    .await;
    assert_eq!(result.failure.unwrap().code, Code::FileTooLarge);
    let result = apply(
        &mut workspace,
        vec![
            Edit::Create {
                path: "x".into(),
                content: "1".into(),
            },
            Edit::Create {
                path: "X".into(),
                content: "2".into(),
            },
        ],
    )
    .await;
    assert_eq!(result.failure.unwrap().code, Code::InvalidPatch);
    assert!(!dir.path().join("x").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn symlinks_hardlinks_and_modes_are_handled_without_escapes() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (dir, mut workspace) = setup();
    let outside = TempDir::new().unwrap();
    disk::write(outside.path().join("secret"), "secret\n").unwrap();
    symlink(outside.path(), dir.path().join("link")).unwrap();
    symlink(outside.path().join("secret"), dir.path().join("file-link")).unwrap();
    disk::hard_link(outside.path().join("secret"), dir.path().join("hard")).unwrap();
    for value in ["link/secret", "file-link", "hard"] {
        assert_eq!(workspace.content(value).unwrap_err().code, Code::UnsafeFile);
        let result = apply(
            &mut workspace,
            vec![update(value, "secret\n", 1, "secret\n", "bad\n")],
        )
        .await;
        assert_eq!(result.failure.unwrap().code, Code::UnsafeFile);
    }

    assert_eq!(
        disk::read_to_string(outside.path().join("secret")).unwrap(),
        "secret\n"
    );
    disk::write(dir.path().join("run"), "old\n").unwrap();
    disk::set_permissions(dir.path().join("run"), disk::Permissions::from_mode(0o751)).unwrap();
    assert!(
        apply(
            &mut workspace,
            vec![update("run", "old\n", 1, "old\n", "new\n")]
        )
        .await
        .complete
    );
    assert_eq!(
        disk::metadata(dir.path().join("run"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o751
    );
    disk::create_dir(dir.path().join("parent")).unwrap();
    let held = workspace.root.parent("parent/new").unwrap();
    disk::rename(dir.path().join("parent"), dir.path().join("moved")).unwrap();
    symlink(outside.path(), dir.path().join("parent")).unwrap();
    let staged = held.stage(b"safe", None).unwrap();
    held.publish(staged, true)
        .unwrap_or_else(|_| panic!("confined publication failed"));
    assert!(!outside.path().join("new").exists());
    assert_eq!(disk::read(dir.path().join("moved/new")).unwrap(), b"safe");
}

#[tokio::test]
async fn edit_reports_are_bounded_before_writing_and_search_snippets_include_the_match() {
    let (dir, mut workspace) = setup();
    workspace.limits.output_bytes = 4096;
    let edits = (0..16)
        .map(|i| Edit::Create {
            path: format!("{i}{}", "a".repeat(240)),
            content: "small".into(),
        })
        .collect();
    let result = apply(&mut workspace, edits).await;
    assert_eq!(result.failure.unwrap().code, Code::OutputLimit);
    assert_eq!(disk::read_dir(dir.path()).unwrap().count(), 0);
    disk::write(
        dir.path().join("source"),
        format!("{}needle\n", "x".repeat(1000)),
    )
    .unwrap();
    let result = workspace
        .search_text(
            serde_json::from_value(json!({"query":"needle"})).unwrap(),
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.matches.len(), 1);
    assert!(result.matches[0].text.contains("needle"));
    assert!(result.matches[0].snippet_start_column > 1);
    assert_eq!(result.scanned_bytes, 1007);
    assert!(encode(&result, 4096).is_ok());
}
