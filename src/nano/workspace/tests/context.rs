//! Tests for context.

use super::*;

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
