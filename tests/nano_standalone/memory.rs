//! Tests for memory.

use super::*;

#[test]
fn repository_domain_context_is_independent_of_memory_availability() {
    use ferrus::project_memory::sqlite::{MEMORY_SIDECAR_SCHEMA_VERSION, MemorySidecar};
    for (version, availability, action) in [
        (None, "not_built", "build"),
        (Some(MEMORY_SIDECAR_SCHEMA_VERSION), "not_built", "build"),
        (Some(1), "incompatible", "rebuild"),
        (
            Some(MEMORY_SIDECAR_SCHEMA_VERSION + 1),
            "incompatible",
            "rebuild",
        ),
    ] {
        let fixture = Fixture::new();
        fs::write(fixture.root.join("lib.rs"), "pub fn current_symbol() {}\n").unwrap();
        let memory_data = fixture.root.parent().unwrap().join("memory");
        fs::create_dir(&memory_data).unwrap();
        let memory_path = memory_data.join("project-memory.db");
        if let Some(version) = version {
            drop(MemorySidecar::open_at(&memory_data).unwrap());
            let database = rusqlite::Connection::open(&memory_path).unwrap();
            database
                .pragma_update(None, "user_version", version)
                .unwrap();
        }

        let before = version.map(|_| fs::read(&memory_path).unwrap());
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
        let graph_before = fs::read(fixture.store.join("repo-graph.db")).unwrap();
        let provider = Provider::new(vec![
            calls(&[
                ("project_memory_status", json!({})),
                (
                    "project_context_search",
                    json!({"domain":"repository","query":"lib.rs"}),
                ),
                (
                    "project_context",
                    json!({"domain":"repository","seeds":[{"type":"path","value":"lib.rs"}]}),
                ),
                (
                    "project_context_search",
                    json!({"domain":"memory","query":"lib.rs"}),
                ),
                (
                    "project_context",
                    json!({"domain":"all","seeds":[{"type":"path","value":"lib.rs"}]}),
                ),
            ]),
            answer("Read repository context without a memory publication."),
        ]);
        fixture.configure(&provider.url, "");
        success(fixture.command("repository-only").args([
            "--graph",
            "--memory-sidecar",
            memory_path.to_str().unwrap(),
            "--memory-namespace",
            "local:standalone-test",
            "--memory-project",
            "empty-memory",
            "--prompt",
            "Inspect repository context.",
        ]));
        let requests = provider.finish();
        let results: Vec<_> = fixture
            .records("repository-only")
            .into_iter()
            .filter(|record| record["event"]["event"] == "tool_result")
            .collect();
        assert_eq!(results.len(), 5);
        let status = &results[0]["event"]["outcome"]["content"]["result"];
        assert_eq!(results[0]["event"]["outcome"]["status"], "success");
        assert!(status["revision_id"].is_null());
        assert_eq!(status["data"]["availability"], availability);
        assert_eq!(status["data"]["recommended_action"], action);
        let selected = requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool")
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(selected["content"].as_str().unwrap()).unwrap(),
            results[0]["event"]["outcome"]
        );
        for (result, field) in results[1..3].iter().zip(["results", "items"]) {
            assert_eq!(result["event"]["outcome"]["status"], "success");
            let response = &result["event"]["outcome"]["content"]["result"]["Ok"];
            assert!(!response[field].as_array().unwrap().is_empty());
            assert!(response["repository"]["snapshot_id"].is_string());
            assert!(response["memory"].is_null());
        }

        for result in &results[3..] {
            assert_eq!(result["event"]["outcome"]["status"], "failed");
        }

        if let Some(before) = before {
            assert_eq!(before, fs::read(&memory_path).unwrap());
        } else {
            assert!(!memory_path.exists());
        }

        assert_eq!(
            graph_before,
            fs::read(fixture.store.join("repo-graph.db")).unwrap()
        );
        fixture.assert_unregistered();
    }
}
