//! Tests for files.

use super::*;

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
