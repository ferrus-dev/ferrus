//! Native file-tool behavior, confinement, bounds, and exact patch regressions.

use super::patch::{Edit, Hunk, State};
use super::*;
use std::fs as disk;
use tempfile::TempDir;

fn setup() -> (TempDir, Workspace) {
    let dir = TempDir::new().unwrap();
    let workspace = Workspace::new(&dir.path().canonicalize().unwrap(), Limits::default()).unwrap();
    (dir, workspace)
}

fn read(workspace: &Workspace, path: &str) -> ReadResult {
    workspace
        .read_file(serde_json::from_value(json!({"path":path})).unwrap())
        .unwrap()
}

fn update(path: &str, before: &str, start_line: usize, old: &str, new: &str) -> Edit {
    Edit::Update {
        path: path.into(),
        expected_digest: digest(before.as_bytes()),
        hunks: vec![Hunk {
            start_line,
            old_text: old.into(),
            new_text: new.into(),
        }],
    }
}

async fn apply(workspace: &mut Workspace, edits: Vec<Edit>) -> PatchResult {
    workspace
        .apply_patch(PatchRequest { edits }, &Cancellation::default())
        .await
}

use super::patch::PatchResult;

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
async fn search_deduplicates_aliases_on_the_actual_filesystem() {
    let (dir, mut workspace) = setup();
    disk::create_dir(dir.path().join("src")).unwrap();
    disk::write(dir.path().join("src/file"), "needle\n").unwrap();
    if !dir.path().join("SRC/file").exists() {
        // A case-sensitive volume has two independent directories.
        disk::create_dir(dir.path().join("SRC")).unwrap();
        disk::write(dir.path().join("SRC/file"), "needle\n").unwrap();
        let result = workspace
            .search_text(
                serde_json::from_value(json!({"paths":["src","SRC"],"query":"needle"})).unwrap(),
                &Cancellation::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.matches.len(), 2);
        assert_eq!(result.scanned_bytes, 14);
        assert_eq!(result.visited_entries, 4);
        assert_eq!(result.listed_entries, 2);
        assert!(!result.truncated);
        return;
    }

    workspace.limits.entries = 3;
    let result = workspace
        .search_text(
            serde_json::from_value(json!({"paths":[".","SRC","src/file"],"query":"needle"}))
                .unwrap(),
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert!(!result.truncated, "{result:?}");
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.visited_entries, 3);
    assert_eq!(result.listed_entries, 2);
    assert_eq!(result.scanned_bytes, 7);
}

#[cfg(unix)]
#[tokio::test]
async fn search_reports_unsupported_names_but_hides_protected_metadata() {
    let (dir, mut workspace) = setup();
    for name in [
        "notes~old",
        "a:backup",
        "other?",
        "extra*",
        "last|",
        "more<",
    ] {
        disk::write(dir.path().join(name), "needle\n").unwrap();
    }
    disk::create_dir(dir.path().join(".git")).unwrap();
    disk::write(dir.path().join(".git/config"), "needle\n").unwrap();
    disk::write(dir.path().join("ferrus.db"), "needle\n").unwrap();
    disk::write(dir.path().join("valid"), "needle\n").unwrap();
    workspace.limits.output_bytes = 2048;
    let result = workspace
        .search_text(
            serde_json::from_value(json!({"query":"needle"})).unwrap(),
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert!(result.truncated);
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].source.path, "valid");
    assert_eq!(result.issues.len(), 4);
    assert_eq!(result.suppressed_issues, 2);
    assert!(
        result
            .issues
            .iter()
            .all(|issue| issue.code == Code::InvalidPath)
    );
    assert!(serde_json::to_vec(&result).unwrap().len() <= workspace.limits.output_bytes);

    for name in [
        "notes~old",
        "a:backup",
        "other?",
        "extra*",
        "last|",
        "more<",
    ] {
        disk::remove_file(dir.path().join(name)).unwrap();
    }
    let result = workspace
        .search_text(
            serde_json::from_value(json!({"query":"needle"})).unwrap(),
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert!(!result.truncated);
    assert!(result.issues.is_empty());
    assert_eq!(result.matches.len(), 1);
}

#[cfg(windows)]
#[tokio::test]
async fn patches_follow_nt_case_identity_before_any_publication() {
    for (first, second, required_alias) in [
        ("source", "SOURCE", true),
        ("caf\u{e9}", "CAF\u{c9}", true),
        ("source", "\u{17f}ource", false),
    ] {
        // Unicode lowercase/uppercase rules do not determine NT path equivalence.
        let aliases = fs::search_key(first) == fs::search_key(second);
        if required_alias {
            assert!(aliases, "expected case alias: {first} / {second}");
        }
        for create in [false, true] {
            let (dir, mut workspace) = setup();
            let edits = if create {
                vec![
                    Edit::Create {
                        path: first.into(),
                        content: "new\n".into(),
                    },
                    Edit::Create {
                        path: second.into(),
                        content: "other\n".into(),
                    },
                ]
            } else {
                disk::write(dir.path().join(first), "old\n").unwrap();
                if !aliases {
                    disk::write(dir.path().join(second), "old\n").unwrap();
                }
                vec![
                    update(first, "old\n", 1, "old\n", "new\n"),
                    update(second, "old\n", 1, "old\n", "other\n"),
                ]
            };
            let result = apply(&mut workspace, edits).await;
            if aliases {
                assert!(!result.complete && result.changes.is_empty(), "{result:?}");
                assert_eq!(result.failure.unwrap().code, Code::InvalidPatch);
                if create {
                    assert!(!dir.path().join(first).exists());
                    assert!(!dir.path().join(second).exists());
                } else {
                    assert_eq!(disk::read(dir.path().join(first)).unwrap(), b"old\n");
                    assert_eq!(disk::read(dir.path().join(second)).unwrap(), b"old\n");
                }
            } else {
                // Distinct NT names must remain independently writable, even if
                // Rust's Unicode uppercase would merge them (for example long-s).
                assert!(result.complete, "{result:?}");
                assert_eq!(result.changes.len(), 2);
                assert_eq!(disk::read(dir.path().join(first)).unwrap(), b"new\n");
                assert_eq!(disk::read(dir.path().join(second)).unwrap(), b"other\n");
            }
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn unix_patch_preflight_rejects_canonically_equivalent_new_targets() {
    for (first, second) in [
        ("caf\u{e9}", "cafe\u{301}"),
        ("CAF\u{c9}", "cafe\u{301}"),
        ("a\u{301}\u{327}", "a\u{327}\u{301}"),
    ] {
        let (dir, mut workspace) = setup();
        let result = apply(
            &mut workspace,
            ["unrelated", first, second]
                .into_iter()
                .map(|path| Edit::Create {
                    path: path.into(),
                    content: "new\n".into(),
                })
                .collect(),
        )
        .await;
        assert!(!result.complete && result.changes.is_empty(), "{result:?}");
        assert_eq!(result.failure.unwrap().code, Code::InvalidPatch);
        assert_eq!(disk::read_dir(dir.path()).unwrap().count(), 0);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn unix_patch_preflight_rejects_full_casefold_aliases() {
    for (first, second) in [
        ("source", "\u{17f}ource"),
        ("strasse", "stra\u{df}e"),
        ("\u{3c3}", "\u{3c2}"),
        ("ffi", "\u{fb03}"),
        ("\u{1fc3}", "\u{3b7}\u{3b9}"),
    ] {
        let (dir, mut workspace) = setup();
        let result = apply(
            &mut workspace,
            ["unrelated", first, second]
                .into_iter()
                .map(|path| Edit::Create {
                    path: path.into(),
                    content: "new\n".into(),
                })
                .collect(),
        )
        .await;
        assert!(!result.complete && result.changes.is_empty(), "{result:?}");
        assert_eq!(result.failure.unwrap().code, Code::InvalidPatch);
        assert_eq!(disk::read_dir(dir.path()).unwrap().count(), 0);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn unix_patch_target_keys_use_the_opened_parent_identity() {
    let (dir, mut workspace) = setup();
    let first = "caf\u{e9}";
    let second = "cafe\u{301}";
    disk::create_dir(dir.path().join(first)).unwrap();
    let aliases = dir.path().join(second).exists();
    if !aliases {
        disk::create_dir(dir.path().join(second)).unwrap();
    }
    let result = apply(
        &mut workspace,
        [first, second]
            .into_iter()
            .map(|parent| Edit::Create {
                path: format!("{parent}/new"),
                content: parent.into(),
            })
            .collect(),
    )
    .await;
    if aliases {
        assert!(!result.complete && result.changes.is_empty(), "{result:?}");
        assert_eq!(result.failure.unwrap().code, Code::InvalidPatch);
        assert!(!dir.path().join(first).join("new").exists());
    } else {
        assert!(result.complete, "{result:?}");
        assert_eq!(
            disk::read_to_string(dir.path().join(first).join("new")).unwrap(),
            first
        );
        assert_eq!(
            disk::read_to_string(dir.path().join(second).join("new")).unwrap(),
            second
        );
    }
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
async fn patches_create_update_delete_and_preserve_unrelated_content() {
    let (dir, mut workspace) = setup();
    for text in [
        "alpha\nbeta\nkeep",
        "alpha\r\nbeta\r\nkeep",
        "alpha\nbeta\r\nkeep",
        "",
    ] {
        disk::write(dir.path().join("source"), text).unwrap();
        disk::write(dir.path().join("untouched"), "human edit").unwrap();
        let old = text.split_inclusive('\n').next().unwrap_or("");
        let replacement = "caf\u{e9}\n";
        let result = apply(
            &mut workspace,
            vec![
                update("source", text, 1, old, replacement),
                Edit::Create {
                    path: "new".into(),
                    content: "new file\n".into(),
                },
            ],
        )
        .await;
        assert!(result.complete, "{result:?}");
        assert!(result.changes.iter().all(|c| c.state == State::Applied));
        let expected = format!("{replacement}{}", &text[old.len()..]);
        assert_eq!(
            disk::read_to_string(dir.path().join("source")).unwrap(),
            expected
        );
        assert_eq!(
            read(&workspace, "source").source.digest,
            result.changes[0].after_digest.clone().unwrap()
        );
        assert_eq!(
            disk::read_to_string(dir.path().join("untouched")).unwrap(),
            "human edit"
        );
        assert!(
            apply(
                &mut workspace,
                vec![Edit::Delete {
                    path: "new".into(),
                    expected_digest: digest(b"new file\n")
                }]
            )
            .await
            .complete
        );
        assert!(!dir.path().join("new").exists());
    }
    assert!(disk::read_dir(dir.path()).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".nano-tmp-")
    }));
}

#[tokio::test]
async fn stale_bases_and_bad_late_hunks_leave_all_files_unchanged() {
    let (dir, mut workspace) = setup();
    disk::write(dir.path().join("a"), "human\n").unwrap();
    let result = apply(
        &mut workspace,
        vec![
            Edit::Create {
                path: "new".into(),
                content: "new".into(),
            },
            update("a", "old\n", 1, "old\n", "new\n"),
        ],
    )
    .await;
    assert!(!result.complete && result.changes.is_empty());
    let failure = result.failure.unwrap();
    assert_eq!(failure.code, Code::Conflict);
    assert_eq!(failure.current_digest, Some(digest(b"human\n")));
    assert!(failure.message.contains("read_file"));
    assert!(!dir.path().join("new").exists());
    for hunks in [
        vec![Hunk {
            start_line: 1,
            old_text: "wrong\n".into(),
            new_text: "new\n".into(),
        }],
        vec![Hunk {
            start_line: 0,
            old_text: "human\n".into(),
            new_text: "new\n".into(),
        }],
        vec![
            Hunk {
                start_line: 1,
                old_text: "human\n".into(),
                new_text: "new\n".into(),
            },
            Hunk {
                start_line: 1,
                old_text: "".into(),
                new_text: "dup\n".into(),
            },
        ],
        vec![Hunk {
            start_line: 1,
            old_text: "hu".into(),
            new_text: "cut".into(),
        }],
    ] {
        let result = apply(
            &mut workspace,
            vec![
                Edit::Create {
                    path: "new".into(),
                    content: "new".into(),
                },
                Edit::Update {
                    path: "a".into(),
                    expected_digest: digest(b"human\n"),
                    hunks,
                },
            ],
        )
        .await;
        assert_eq!(result.failure.unwrap().code, Code::InvalidPatch);
        assert!(!dir.path().join("new").exists());
        assert_eq!(disk::read(dir.path().join("a")).unwrap(), b"human\n");
    }
}

#[test]
fn protected_and_nonportable_paths_fail_closed() {
    for value in [
        "../escape",
        "/absolute",
        "C:/drive",
        "a\\b",
        "a/../b",
        "a//b",
        "a:stream",
        "a.",
        "a ",
        "NUL.txt",
        "COM1",
        "a/FERRUS~1.DB",
        "a\0b",
    ] {
        assert_eq!(path(value).unwrap_err().code, Code::InvalidPath, "{value}");
    }
    for value in [
        ".git/config",
        "a/.GIT/index",
        ".ferrus/tasks/x",
        "a/ferrus.db",
        "repo-graph.db-wal",
        "project-memory.db-shm",
        ".nano-tmp-12-1",
    ] {
        assert_eq!(
            path(value).unwrap_err().code,
            Code::ProtectedPath,
            "{value}"
        );
    }
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

#[tokio::test]
async fn cancellation_and_schema_validation_precede_effects() {
    let (dir, mut workspace) = setup();
    let cancel = Cancellation::default();
    cancel.cancel();
    let result = workspace
        .apply_patch(
            PatchRequest {
                edits: vec![Edit::Create {
                    path: "x".into(),
                    content: "hello".into(),
                }],
            },
            &cancel,
        )
        .await;
    assert_eq!(result.failure.unwrap().code, Code::Interrupted);
    assert!(!dir.path().join("x").exists());
    assert_eq!(workspace.descriptors().len(), 3);
    assert!(workspace.validate("apply_patch", &json!({"edits":[{"operation":"create","path":"x","content":"hello","unexpected":true}]})).is_err());
    assert!(
        workspace
            .validate("read_file", &json!({"path":"x","start_line":-1}))
            .is_err()
    );
    assert_eq!(
        workspace.validate("exec", &json!({})),
        Err(ToolError::UnknownTool)
    );
    let call = ValidatedCall {
        call_id: "c1".into(),
        provider_call_id: "p1".into(),
        name: "apply_patch".into(),
        arguments: json!({"edits":[{"operation":"create","path":".git/index","content":"bad"}]}),
    };
    assert!(matches!(
        workspace.execute(&call, &Cancellation::default()).await,
        ToolOutcome::Failed(ToolError::Workspace(_))
    ));
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
async fn ordered_hunks_insert_and_delete_against_original_line_numbers() {
    let (dir, mut workspace) = setup();
    let before = "one\r\ntwo\nthree\r\nlast";
    disk::write(dir.path().join("source"), before).unwrap();
    let result = apply(
        &mut workspace,
        vec![Edit::Update {
            path: "source".into(),
            expected_digest: digest(before.as_bytes()),
            hunks: vec![
                Hunk {
                    start_line: 2,
                    old_text: String::new(),
                    new_text: "insert\n".into(),
                },
                Hunk {
                    start_line: 3,
                    old_text: "three\r\n".into(),
                    new_text: String::new(),
                },
                Hunk {
                    start_line: 4,
                    old_text: "last".into(),
                    new_text: "end".into(),
                },
            ],
        }],
    )
    .await;
    assert!(result.complete, "{result:?}");
    assert_eq!(
        disk::read(dir.path().join("source")).unwrap(),
        b"one\r\ninsert\ntwo\nend"
    );
    assert_eq!(
        path(".g\u{131}t/config").unwrap_err().code,
        Code::ProtectedPath
    );
    assert_eq!(
        path("ferru\u{17f}.db").unwrap_err().code,
        Code::ProtectedPath
    );
    assert!(
        workspace
            .validate("read_file", &json!({"path":"source","start_line":0}))
            .is_err()
    );
}

#[tokio::test]
async fn partial_tool_outcomes_retain_reconciliation_details() {
    use std::{future::Future, task::Poll};
    let (dir, mut workspace) = setup();
    disk::create_dir(dir.path().join("parent")).unwrap();
    let call = ValidatedCall {
        call_id: "c1".into(),
        provider_call_id: "p1".into(),
        name: "apply_patch".into(),
        arguments: json!({"edits":[
            {"operation":"create","path":"first","content":"one"},
            {"operation":"create","path":"parent/second","content":"two"}
        ]}),
    };
    let cancel = Cancellation::default();
    let mut operation = std::pin::pin!(workspace.execute(&call, &cancel));
    // The first publication yields before attempting the second one.
    std::future::poll_fn(|cx| {
        assert!(operation.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    disk::remove_dir(dir.path().join("parent")).unwrap();
    let outcome = operation.await;
    assert!(encode(&outcome, 24 * 1024).is_ok());
    let ToolOutcome::Unknown(ToolError::Workspace(result)) = outcome else {
        panic!("missing partial outcome")
    };
    assert_eq!(result["changes"][0]["state"], "applied");
    assert_eq!(result["changes"][0]["after_digest"], digest(b"one"));
    assert_eq!(result["changes"][1]["state"], "not_applied");
    assert_eq!(result["failure"]["code"], "not_found");
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
