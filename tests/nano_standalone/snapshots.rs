//! Tests for snapshots.

use super::*;

#[test]
fn standalone_git_graph_is_explicit_local_and_snapshot_bound_after_workspace_move() {
    let mut fixture = Fixture::new();
    success(
        Command::new("git")
            .arg("init")
            .arg("--object-format=sha1")
            .arg(&fixture.root),
    );
    fs::write(
        fixture.root.join("Cargo.toml"),
        "[package]\nname = 'standalone-fixture'\nversion = '0.1.0'\nedition = '2024'\n[lib]\npath = 'lib.rs'\n",
    ).unwrap();
    fs::write(
        fixture.root.join("lib.rs"),
        "pub fn standalone_symbol() -> bool { true }\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ferrus-nano"))
        .env("HOME", &fixture.home)
        .env("USERPROFILE", &fixture.home)
        .args([
            "--workspace",
            fixture.root.to_str().unwrap(),
            "--storage",
            fixture.store.to_str().unwrap(),
            "--index-graph",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let index: Value = serde_json::from_slice(&output.stdout).unwrap();
    let moved = fixture.root.with_file_name("moved-workspace");
    fs::rename(&fixture.root, &moved).unwrap();
    // The historical pathname may now refer to a different directory.
    fs::create_dir(&fixture.root).unwrap();
    fixture.root = moved.canonicalize().unwrap();
    let provider = Provider::new(vec![
        calls(&[
            ("repository_search", json!({"query":"standalone_symbol"})),
            (
                "repository_context",
                json!({"seeds":[{"type":"path","value":"lib.rs"}],"include_snippets":true}),
            ),
            (
                "repository_context",
                json!({"seeds":[{"type":"path","value":"lib.rs"}],"include_snippets":true,"max_snippet_bytes":1}),
            ),
            (
                "repository_search",
                json!({"query":"standalone-fixture","kinds":["cargo_package"]}),
            ),
            (
                "repository_context",
                json!({"seeds":[{"type":"path","value":"lib.rs"}],"cursor":"obsolete-cursor","include_snippets":true}),
            ),
        ]),
        calls(&[(
            "apply_patch",
            json!({"edits":[{"operation":"create","path":"new.rs","content":"pub fn new_symbol() {}\n"}]}),
        )]),
        answer("Read the explicit standalone index and updated the workspace."),
    ]);
    fixture.configure(&provider.url, "");
    success(fixture.command("graph-query").args([
        "--graph",
        "--prompt",
        "Inspect standalone_symbol.",
    ]));
    let requests = provider.finish();
    let binding: Value =
        serde_json::from_slice(&fs::read(fixture.store.join("workspace.json")).unwrap()).unwrap();
    assert_eq!(
        PathBuf::from(binding["workspace"].as_str().unwrap()),
        fixture.root
    );
    let results: Vec<_> = fixture
        .records("graph-query")
        .into_iter()
        .filter(|record| record["event"]["event"] == "tool_result")
        .collect();
    assert_eq!(results.len(), 6);
    for result in &results {
        assert_eq!(result["event"]["outcome"]["status"], "success");
    }

    let context = &results[1]["event"]["outcome"]["content"]["result"]["Ok"];
    assert_eq!(context["snapshot_id"], index["snapshot"]["id"]);
    assert!(context["task_view"].is_null());
    assert!(!context["data"]["snippets"].as_array().unwrap().is_empty());
    let bounded = &results[2]["event"]["outcome"]["content"]["result"]["Ok"];
    assert!(
        bounded["diagnostics"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "content.snippets_truncated")
    );
    let snippet_bytes: usize = bounded["data"]["snippets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|snippet| snippet["text"].as_str().unwrap().len())
        .sum();
    assert!(snippet_bytes <= 1);
    let package = &results[3]["event"]["outcome"]["content"]["result"]["Ok"];
    assert!(
        package["data"]["hits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "cargo_package")
    );
    let error = &results[4]["event"]["outcome"]["content"]["result"]["Err"];
    assert_eq!(error["code"], "stale_cursor");
    assert_eq!(error["retryable"], false);
    assert!(error["message"].as_str().unwrap().contains("snapshot"));
    let tool_contents = |request: &Value| {
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .map(|message| {
                serde_json::from_str::<Value>(message["content"].as_str().unwrap()).unwrap()
            })
            .collect::<Vec<_>>()
    };
    let selected = tool_contents(&requests[1]);
    assert_eq!(selected.len(), 5);
    for (selected, recorded) in selected.iter().zip(&results) {
        assert_eq!(selected, &recorded["event"]["outcome"]);
    }

    let invalidated = tool_contents(&requests[2]);
    for result in &invalidated[..4] {
        assert_eq!(result["content"]["kind"], "evidence_unavailable");
        assert_eq!(result["content"]["reason"], "publication_changed");
    }

    fixture.assert_unregistered();
}

#[test]
fn graph_status_without_an_index_reports_availability_and_preserves_fallback() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("lib.rs"), "pub fn current_symbol() {}\n").unwrap();
    let provider = Provider::new(vec![
        calls(&[
            ("repository_graph_status", json!({})),
            (
                "repository_fallback",
                json!({"operation":"search","reason":"missing","input":{"query":"current_symbol"}}),
            ),
        ]),
        answer("Used current workspace evidence."),
    ]);
    fixture.configure(&provider.url, "");
    success(fixture.command("missing-graph").args([
        "--graph",
        "--prompt",
        "Check graph availability and inspect current_symbol.",
    ]));
    provider.finish();
    let results: Vec<_> = fixture
        .records("missing-graph")
        .into_iter()
        .filter(|r| r["event"]["event"] == "tool_result")
        .collect();
    assert!(
        results
            .iter()
            .all(|r| r["event"]["outcome"]["status"] == "success")
    );
    let status = &results[0]["event"]["outcome"]["content"]["result"];
    assert_eq!(status["data"]["availability"], "not_built");
    assert_eq!(status["data"]["recommended_action"], "index");
    assert!(status["snapshot_id"].is_null());
    assert!(!fixture.store.join("repo-graph.db").exists());
    fixture.assert_unregistered();
}

#[test]
fn a_mutation_keeps_the_standalone_graph_stale_until_explicit_reindex() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("lib.rs"), "pub fn original_symbol() {}\n").unwrap();
    let index = || {
        success(
            Command::new(env!("CARGO_BIN_EXE_ferrus-nano"))
                .env("HOME", &fixture.home)
                .env("USERPROFILE", &fixture.home)
                .args([
                    "--workspace",
                    fixture.root.to_str().unwrap(),
                    "--storage",
                    fixture.store.to_str().unwrap(),
                    "--index-graph",
                ]),
        );
    };
    index();
    let provider = Provider::new(vec![
        calls(&[(
            "apply_patch",
            json!({"edits":[{"operation":"create","path":"created.rs","content":"pub fn newly_created() {}\n"}]}),
        )]),
        calls(&[
            ("repository_graph_status", json!({})),
            ("repository_search", json!({"query":"newly_created"})),
            (
                "repository_fallback",
                json!({"operation":"search","reason":"stale","input":{"query":"newly_created"}}),
            ),
        ]),
        answer("Used current workspace evidence."),
    ]);
    fixture.configure(&provider.url, "");
    success(fixture.command("graph-stale").args([
        "--graph",
        "--prompt",
        "Create a symbol and inspect it.",
    ]));
    provider.finish();
    let results: Vec<_> = fixture
        .records("graph-stale")
        .into_iter()
        .filter(|record| record["event"]["event"] == "tool_result")
        .collect();
    assert_eq!(
        results[1]["event"]["outcome"]["content"]["result"]["freshness"]["freshness"],
        "stale"
    );
    assert_eq!(results[2]["event"]["outcome"]["status"], "failed");
    assert_eq!(results[3]["event"]["outcome"]["status"], "success");
    assert!(fixture.store.join("graph-invalidated").exists());
    index();
    assert!(!fixture.store.join("graph-invalidated").exists());
    fixture.assert_unregistered();
}
