//! Shared native instruction and context tool schemas.

use super::{context_request as context, tools::ToolDescriptor};
use serde_json::json;

pub(super) const MANAGED_NAMES: &[&str] = &["check", "submit", "consult", "ask_human"];

pub(super) fn descriptor(name: &str) -> ToolDescriptor {
    let schema = if name == "load_instructions" {
        json!({"type":"object","properties":{
            "paths":{"type":"array","maxItems":16,"items":{"type":"string","maxLength":256}},
            "skills":{"type":"array","maxItems":8,"items":{"type":"string","maxLength":64}}
        },"additionalProperties":false})
    } else if name == "repository_fallback" {
        let read = json!({"type":"object","properties":{"path":{"type":"string"},"start_line":{"type":"integer","minimum":1},"max_lines":{"type":"integer","minimum":1},"max_bytes":{"type":"integer","minimum":1}},"required":["path"],"additionalProperties":false});
        let search = json!({"type":"object","properties":{"query":{"type":"string"},"paths":{"type":"array","items":{"type":"string"}},"max_results":{"type":"integer","minimum":1}},"required":["query"],"additionalProperties":false});
        let reason = json!({"type":"string","enum":["missing","disabled","stale","ambiguous","unsupported"]});
        // Chat Completions servers may require properties at the schema root.
        // Keep the tagged alternatives so operation and input still agree.
        json!({"type":"object","properties":{
            "operation":{"type":"string","enum":["read","search"]},
            "reason":reason,"input":{"type":"object","oneOf":[read,search]}
        },"required":["operation","reason","input"],"additionalProperties":false,"oneOf":[
            {"type":"object","properties":{"operation":{"const":"read"},"reason":reason,"input":read},"required":["operation","reason","input"],"additionalProperties":false},
            {"type":"object","properties":{"operation":{"const":"search"},"reason":reason,"input":search},"required":["operation","reason","input"],"additionalProperties":false}
        ]})
    } else {
        let mut schema = json!({"type":"object","properties":{
            "domain":{"type":"string","enum":["repository","memory","all"]},
            "query":{"type":"string","minLength":1,"maxLength":512},
            "paths":{"type":"array","maxItems":32,"items":{"type":"string","maxLength":512}},
            "kinds":{"type":"array","maxItems":32,"items":{"type":"string","maxLength":512}},
            "seeds":{"type":"array","maxItems":32,"items":{"type":"object","properties":{"type":{"type":"string","enum":["node","symbol","path","memory_entity","milestone","task","run"]},"value":{"type":"string","minLength":1,"maxLength":512}},"required":["type","value"],"additionalProperties":false}},
            "cursor":{"type":"string","minLength":1,"maxLength":16384},
            "max_results":{"type":"integer","minimum":1},"max_bytes":{"type":"integer","minimum":1},
            "max_depth":{"type":"integer","minimum":1},"max_duration_ms":{"type":"integer","minimum":1},
            "max_diagnostics":{"type":"integer","minimum":1},"max_snippet_bytes":{"type":"integer","minimum":1},
            "include_snippets":{"type":"boolean"},"include_unresolved":{"type":"boolean"},"include_stale":{"type":"boolean"},
            "direction":{"type":"string","enum":["incoming","outgoing","both"]}
        },"required":[],"additionalProperties":false});

        let mut required = vec![];
        if name.starts_with("project_context") {
            required.push("domain");
        } else {
            schema["properties"]
                .as_object_mut()
                .unwrap()
                .remove("domain");
        }

        if name.ends_with("search") {
            required.push("query");
        }

        if name.ends_with("context") {
            required.push("seeds");
        }

        schema["properties"]
            .as_object_mut()
            .unwrap()
            .retain(|key, _| context::fields(name).contains(&key.as_str()));
        schema["required"] = json!(required);
        schema
    };

    ToolDescriptor { name: name.into(), input_schema: schema, description: match name {
        "load_instructions" => "Reload scoped AGENTS.md guidance and explicit skills. Root AGENTS.md is automatic; task and rejection instructions are also loaded for managed sessions. Do not put .ferrus paths in paths. Paths select workspace file scopes, e.g. {\"paths\":[\"src/main.rs\"],\"skills\":[\"ferrus-executor\"]}. Skills are explicit names under .agents/skills. Supporting documents cannot override runtime policy.",
        "repository_fallback" => "Read or search the bound workspace when graph coverage is missing, disabled, stale, ambiguous, or unsupported. Label the requested reason; returned bytes are current workspace evidence, not graph facts. Routing failures remain errors.",
        "repository_graph_status" => "Read repository graph availability, snapshot, freshness and coverage diagnostics. Direct sessions use canonical context; managed sessions include the bound task baseline/overlay. Never builds an index.",
        "repository_search" => "Search the bound repository snapshot under result/time/byte caps. Missing relationships are unknown. Use repository_fallback for incomplete coverage.",
        "repository_context" => "Retrieve bounded structural context. Optional source snippets are hash-verified against the snapshot. Never changes task/review prompts.",
        "project_memory_status" => "Read independent project memory revision, freshness, source policy and availability. Never authors or indexes memory.",
        "project_context_search" => "Search an explicit repository, memory, or all domain through native local adapters. Each domain retains independent revision and freshness.",
        _ => "Retrieve bounded context in an explicit domain. Cross-domain links must belong to the exact memory revision/repository snapshot pair; optional snippets require verified content.",
    }.into() }
}
