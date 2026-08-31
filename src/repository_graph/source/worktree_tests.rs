use super::*;
use crate::repository_graph::{
    config::RepositoryGraphConfig,
    domain::{RepositoryId, RepositoryNamespace, RepositoryRef, TaskViewId},
};
use std::process::Command;

fn git(root: &Path, arguments: &[&str]) -> String {
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
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn initialize(root: &Path) {
    git(root, &["init"]);
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "user.name", "Ferrus Test"]);
    git(root, &["config", "commit.gpgsign", "false"]);
}

fn repository() -> RepositoryRef {
    RepositoryRef {
        namespace: RepositoryNamespace::new("local:test").unwrap(),
        repository_id: RepositoryId::new("root").unwrap(),
    }
}

fn workspace(task: &str, baseline_revision: Digest) -> WorkspaceRef {
    WorkspaceRef {
        repository: repository(),
        task_view_id: TaskViewId::new(task).unwrap(),
        baseline_revision,
    }
}

#[test]
fn inventory_reports_add_change_delete_and_rename_without_touching_the_index() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    initialize(root);
    fs::write(root.join(".gitignore"), "ignored.log\n.ferrus/\n").unwrap();
    fs::write(root.join("modified.rs"), "pub struct Before;\n").unwrap();
    fs::write(root.join("deleted.rs"), "pub struct Deleted;\n").unwrap();
    fs::write(root.join("renamed_old.rs"), "pub struct Renamed;\n").unwrap();
    fs::write(root.join("unchanged.rs"), "pub struct Unchanged;\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "baseline"]);
    let baseline = parse_git_tree_digest(&git(root, &["rev-parse", "HEAD^{tree}"])).unwrap();

    fs::write(root.join("modified.rs"), "pub struct After;\n").unwrap();
    fs::remove_file(root.join("deleted.rs")).unwrap();
    git(root, &["mv", "renamed_old.rs", "renamed_new.rs"]);
    fs::write(root.join("added.rs"), "pub struct Added;\n").unwrap();
    fs::write(root.join("ignored.log"), "ignored\n").unwrap();
    fs::create_dir(root.join(".ferrus")).unwrap();
    fs::write(root.join(".ferrus/runtime"), "runtime\n").unwrap();
    let index_before = git(root, &["ls-files", "--stage"]);

    let inventory = GitWorktreeInventory::discover(root, baseline).unwrap();
    let changes = inventory
        .changes()
        .iter()
        .map(|change| {
            (
                change.path.as_str(),
                change.kind,
                change.renamed_from.as_ref().map(RepoPath::as_str),
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(
        changes,
        vec![
            ("added.rs", OverlayChangeKind::Added, None),
            ("deleted.rs", OverlayChangeKind::Deleted, None),
            ("modified.rs", OverlayChangeKind::Modified, None),
            (
                "renamed_new.rs",
                OverlayChangeKind::Added,
                Some("renamed_old.rs")
            ),
            ("renamed_old.rs", OverlayChangeKind::Deleted, None),
        ]
    );
    assert_eq!(git(root, &["ls-files", "--stage"]), index_before);
    assert!(
        !inventory
            .untracked_paths()
            .iter()
            .any(|path| path.as_str() == "ignored.log")
    );
}

#[test]
fn captured_tree_preserves_the_real_index_and_is_an_empty_overlay() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    initialize(root);
    fs::write(root.join("tracked.rs"), "pub struct Base;\n").unwrap();
    git(root, &["add", "tracked.rs"]);
    git(root, &["commit", "-m", "baseline"]);
    fs::write(root.join("tracked.rs"), "pub struct Seeded;\n").unwrap();
    fs::write(root.join("seeded.rs"), "pub struct SeededFile;\n").unwrap();
    let index_before = git(root, &["ls-files", "--stage"]);

    let captured = capture_worktree_tree(root).unwrap();
    let inventory = GitWorktreeInventory::discover(root, captured).unwrap();

    assert!(inventory.changes().is_empty());
    assert_eq!(git(root, &["ls-files", "--stage"]), index_before);
}

#[test]
fn captured_untracked_files_are_compared_with_git_path_filters() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    initialize(root);
    fs::write(root.join(".gitattributes"), "*.txt text eol=lf\n").unwrap();
    fs::write(root.join("seeded.txt"), b"seeded\r\n").unwrap();

    let captured = capture_worktree_tree(root).unwrap();
    let inventory = GitWorktreeInventory::discover(root, captured.clone()).unwrap();

    assert_eq!(
        git(root, &["show", &format!("{}:seeded.txt", captured.value())]),
        "seeded"
    );
    assert!(inventory.changes().is_empty());
}

#[test]
fn submitted_tree_pin_keeps_captured_objects_reachable() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    initialize(root);
    fs::write(root.join("submitted.rs"), "pub struct Submitted;\n").unwrap();

    let captured = capture_worktree_tree(root).unwrap();
    pin_submitted_tree(root, "task/with unsafe ref chars", &captured).unwrap();
    let reference = submitted_tree_ref("task/with unsafe ref chars");

    assert_eq!(
        git(root, &["rev-parse", &reference]),
        captured.value().to_string()
    );
    git(root, &["reflog", "expire", "--expire=now", "--all"]);
    git(root, &["gc", "--prune=now"]);
    assert_eq!(git(root, &["cat-file", "-t", captured.value()]), "tree");

    release_submitted_tree_pin(root, "task/with unsafe ref chars").unwrap();
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["show-ref", "--verify", "--quiet", &reference])
        .output()
        .unwrap();
    assert!(!output.status.success());
}

#[test]
fn overlay_manifest_applies_policy_and_invalidates_on_indexable_content_changes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    initialize(root);
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub struct Before;\n").unwrap();
    fs::write(root.join("secret.txt"), "before-secret\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "baseline"]);
    let baseline = parse_git_tree_digest(&git(root, &["rev-parse", "HEAD^{tree}"])).unwrap();
    fs::write(root.join("src/lib.rs"), "pub struct After;\n").unwrap();
    fs::write(root.join("secret.txt"), "after-secret\n").unwrap();

    let mut config = RepositoryGraphConfig::default();
    config.source.sensitive = ["secret.txt".to_string()].into_iter().collect();
    let context = SourceDiscoveryContext::from_config(repository(), &config, &[]).unwrap();
    let overlay =
        TaskWorktreeOverlay::discover(root, workspace("task-a", baseline.clone()), context.clone())
            .unwrap();
    let indexed = overlay
        .manifest()
        .changes
        .iter()
        .find(|change| change.path.as_str() == "src/lib.rs")
        .unwrap();
    let sensitive = overlay
        .manifest()
        .changes
        .iter()
        .find(|change| change.path.as_str() == "secret.txt")
        .unwrap();
    let indexed_file = indexed.current_file.clone().unwrap();
    assert_eq!(
        overlay.read_verified(&indexed_file).unwrap().bytes,
        b"pub struct After;\n"
    );
    assert!(sensitive.current_file.is_none());
    assert!(overlay.revalidate().unwrap());

    let other_task =
        TaskWorktreeOverlay::discover(root, workspace("task-b", baseline), context).unwrap();
    assert_eq!(
        overlay.manifest().manifest_digest,
        other_task.manifest().manifest_digest
    );
    assert_ne!(
        overlay.manifest().revision_id,
        other_task.manifest().revision_id
    );

    fs::write(root.join("src/lib.rs"), "pub struct ChangedAgain;\n").unwrap();
    assert!(matches!(
        overlay.read_verified(&indexed_file),
        Err(SourceError::ContentChanged)
    ));
    assert!(!overlay.revalidate().unwrap());
}

#[test]
fn composed_overlay_replaces_changed_paths_hides_deletions_and_reads_baseline_blobs() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    initialize(root);
    fs::write(root.join("changed.rs"), "pub struct Before;\n").unwrap();
    fs::write(root.join("deleted.rs"), "pub struct Deleted;\n").unwrap();
    fs::write(root.join("unchanged.rs"), "pub struct Unchanged;\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "baseline"]);
    let baseline = parse_git_tree_digest(&git(root, &["rev-parse", "HEAD^{tree}"])).unwrap();
    let config = RepositoryGraphConfig::default();
    let context = SourceDiscoveryContext::from_config(repository(), &config, &[]).unwrap();
    let baseline_source = LocalRepositorySource::discover(root, context.clone()).unwrap();
    let baseline_manifest_digest = baseline_source.manifest().revision.manifest_digest.clone();
    let baseline_files = baseline_source.manifest().files.clone();
    let baseline_analysis_config_digest = baseline_source
        .manifest()
        .revision
        .analysis_config_digest
        .clone();

    fs::write(root.join("changed.rs"), "pub struct After;\n").unwrap();
    fs::remove_file(root.join("deleted.rs")).unwrap();
    fs::write(root.join("added.rs"), "pub struct Added;\n").unwrap();
    let composed = TaskOverlaySource::discover(
        root,
        workspace("task-composed", baseline.clone()),
        context,
        baseline_analysis_config_digest,
        baseline_files,
    )
    .unwrap();

    let files = composed
        .manifest()
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<Vec<_>>();
    assert!(files.contains(&"added.rs"));
    assert!(files.contains(&"changed.rs"));
    assert!(files.contains(&"unchanged.rs"));
    assert!(!files.contains(&"deleted.rs"));
    assert_eq!(
        composed.manifest().revision.source_kind,
        SourceKind::WorkspaceOverlay
    );
    assert_eq!(
        composed.manifest().revision.base_revision.as_ref(),
        Some(&baseline)
    );
    assert_ne!(
        composed.manifest().revision.manifest_digest,
        baseline_manifest_digest
    );

    let changed = composed
        .manifest()
        .files
        .iter()
        .find(|file| file.path.as_str() == "changed.rs")
        .unwrap();
    let unchanged = composed
        .manifest()
        .files
        .iter()
        .find(|file| file.path.as_str() == "unchanged.rs")
        .unwrap();
    assert_eq!(
        composed.read_verified(changed).unwrap().bytes,
        b"pub struct After;\n"
    );
    assert_eq!(
        composed.read_verified(unchanged).unwrap().bytes,
        b"pub struct Unchanged;\n"
    );
    assert!(composed.revalidate().unwrap());

    fs::write(root.join("unchanged.rs"), "pub struct NowChanged;\n").unwrap();
    assert!(!composed.revalidate().unwrap());
}

#[test]
fn composed_overlay_reapplies_changed_source_policy_to_the_baseline_tree() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    initialize(root);
    fs::create_dir(root.join("old")).unwrap();
    fs::create_dir(root.join("new")).unwrap();
    fs::write(root.join("old/changed.rs"), "pub struct Before;\n").unwrap();
    fs::write(root.join("old/keep.rs"), "pub struct Keep;\n").unwrap();
    fs::write(root.join("old/secret.rs"), "pub struct Secret;\n").unwrap();
    fs::write(root.join("new/included.rs"), "pub struct Included;\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "baseline"]);
    let baseline = parse_git_tree_digest(&git(root, &["rev-parse", "HEAD^{tree}"])).unwrap();

    let mut baseline_config = RepositoryGraphConfig::default();
    baseline_config.source.include = ["old/**".to_string()].into_iter().collect();
    let baseline_context =
        SourceDiscoveryContext::from_config(repository(), &baseline_config, &[]).unwrap();
    let baseline_manifest =
        discover_tree_manifest(root, &baseline_context, baseline.clone()).unwrap();
    let baseline_files = baseline_manifest.files;

    fs::write(root.join("old/changed.rs"), "pub struct After;\n").unwrap();
    let mut current_config = baseline_config;
    current_config.source.include = ["new/**".to_string(), "old/**".to_string()]
        .into_iter()
        .collect();
    current_config.source.sensitive = ["old/secret.rs".to_string()].into_iter().collect();
    let current_context =
        SourceDiscoveryContext::from_config(repository(), &current_config, &[]).unwrap();
    let composed = TaskOverlaySource::discover(
        root,
        workspace("task-policy-change", baseline),
        current_context,
        baseline_context.analysis_config_digest().clone(),
        baseline_files,
    )
    .unwrap();

    let files = composed
        .manifest()
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<Vec<_>>();
    assert!(composed.baseline_manifest_rebuilt);
    assert!(composed.requires_index());
    assert!(files.contains(&"old/changed.rs"));
    assert!(files.contains(&"old/keep.rs"));
    assert!(files.contains(&"new/included.rs"));
    assert!(!files.contains(&"old/secret.rs"));

    let newly_included = composed
        .manifest()
        .files
        .iter()
        .find(|file| file.path.as_str() == "new/included.rs")
        .unwrap();
    assert_eq!(
        composed.read_verified(newly_included).unwrap().bytes,
        b"pub struct Included;\n"
    );
}
