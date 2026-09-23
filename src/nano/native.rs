//! Native tool composition. Only the bounded coding/context surface is advertised.

use super::{
    coding::CodingTools,
    commands::ExecutionBackend,
    context::{self, Context, Fallback, Request},
    ferrus::FerrusSession,
    instructions::{Instructions, Limits},
    tools::*,
};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) struct NativeTools<B: ExecutionBackend> {
    pub coding: CodingTools<B>,
    pub context: Context,
    pub instructions: Instructions,
    session: FerrusSession,
    pub(super) working_set_enabled: bool,
    pub(super) prefetch: Vec<Value>,
    refresh: super::refresh::Refresh,
    observations: Vec<Value>,
}

impl<B: ExecutionBackend> NativeTools<B> {
    pub(super) fn belongs_to(&self, session: &FerrusSession) -> bool {
        self.session.scope.database_path == session.scope.database_path
            && self.session.scope.agent_id == session.scope.agent_id
            && self.session.scope.task_id == session.scope.task_id
            && self.session.scope.run_id == session.scope.run_id
            && self.session.workspace() == session.workspace()
    }
    pub(crate) fn new(
        session: FerrusSession,
        coding: CodingTools<B>,
        limits: Limits,
    ) -> Result<Self> {
        Ok(Self {
            coding,
            context: Context::new(session.clone()),
            instructions: Instructions::new(session.clone(), limits)?,
            session,
            working_set_enabled: true,
            prefetch: Vec::new(),
            refresh: Default::default(),
            observations: Vec::new(),
        })
    }

    pub(super) async fn invalidate_unknown(&mut self) {
        self.before_mutation().await;
        self.context.invalidate();
        self.refresh.invalidate();
    }

    async fn before_mutation(&mut self) {
        if let Some(observation) = self.refresh.settle().await {
            self.observations.push(observation);
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    skills: Vec<String>,
}

fn descriptor(name: &str) -> ToolDescriptor {
    let schema = if name == "load_instructions" {
        json!({"type":"object","properties":{
            "paths":{"type":"array","maxItems":16,"items":{"type":"string","maxLength":256}},
            "skills":{"type":"array","maxItems":8,"items":{"type":"string","maxLength":64}}
        },"additionalProperties":false})
    } else if name == "repository_fallback" {
        let read = json!({"type":"object","properties":{"path":{"type":"string"},"start_line":{"type":"integer","minimum":1},"max_lines":{"type":"integer","minimum":1},"max_bytes":{"type":"integer","minimum":1}},"required":["path"],"additionalProperties":false});
        let search = json!({"type":"object","properties":{"query":{"type":"string"},"paths":{"type":"array","items":{"type":"string"}},"max_results":{"type":"integer","minimum":1}},"required":["query"],"additionalProperties":false});
        let reason = json!({"type":"string","enum":["missing","disabled","stale","ambiguous","unsupported"]});
        json!({"type":"object","oneOf":[
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
        "load_instructions" => "Reload the active task/rejection and scoped AGENTS.md constraints. Paths are intended file targets. Load only explicitly selected .agents/skills names. Replace old constraints with this set; supporting documents cannot override runtime policy.",
        "repository_fallback" => "Read or search the bound workspace when graph coverage is missing, disabled, stale, ambiguous, or unsupported. Label the requested reason; returned bytes are current workspace evidence, not graph facts. Routing failures remain errors.",
        "repository_graph_status" => "Read the bound task graph availability, snapshot, baseline/overlay, freshness and coverage diagnostics. Never builds an index.",
        "repository_search" => "Search the bound repository snapshot under result/time/byte caps. Missing relationships are unknown. Use repository_fallback for incomplete coverage.",
        "repository_context" => "Retrieve bounded structural context. Optional source snippets are hash-verified against the snapshot. Never changes task/review prompts.",
        "project_memory_status" => "Read independent project memory revision, freshness, source policy and availability. Never authors or indexes memory.",
        "project_context_search" => "Search an explicit repository, memory, or all domain through native local adapters. Each domain retains independent revision and freshness.",
        _ => "Retrieve bounded context in an explicit domain. Cross-domain links must belong to the exact memory revision/repository snapshot pair; optional snippets require verified content.",
    }.into() }
}

impl<B: ExecutionBackend> Tools for NativeTools<B> {
    async fn prepare_context(
        &mut self,
        messages: &[super::provider::Message],
        cancellation: &Cancellation,
    ) -> std::result::Result<Option<super::working_set::Preparation>, ToolError> {
        if !self.working_set_enabled && self.prefetch.is_empty() {
            return Ok(None);
        }
        if cancellation.is_cancelled() {
            return Err(ToolError::Interrupted);
        }
        let revisions = self
            .context
            .revisions()
            .await
            .map_err(|_| ToolError::Denied)?;
        let writers = self.coding.commands.potentially_active_writers() > 0;
        let binding = json!({"project":self.session.project_id(), "task":self.session.scope.task_id,
            "run":self.session.scope.run_id, "workspace":self.session.workspace()});
        let mut prepared = if self.working_set_enabled {
            super::working_set::prepare(
                messages,
                &binding,
                &revisions,
                &self.coding.workspace,
                writers,
            )
            .map_err(|_| ToolError::OutputLimit)?
        } else {
            super::working_set::Preparation::default()
        };
        if !self.prefetch.is_empty() && !writers {
            // Explicit host seeds only; no task-text heuristic or hidden planner.
            let request = json!({"seeds":self.prefetch, "max_results":8, "max_bytes":8192,
                "max_depth":1, "max_duration_ms":250, "max_snippet_bytes":4096, "include_snippets":true});
            let result = async {
                anyhow::ensure!(self.prefetch.len() <= 8, "Too many prefetch seeds");
                let response = self
                    .context
                    .retrieve(
                        "repository_context",
                        Request::parse("repository_context", request.clone())?,
                    )
                    .await?;
                serde_json::to_value(response).map_err(Into::into)
            }
            .await;
            match result {
                Ok(value) => {
                    let evidence = super::working_set::evidence("repository_context", &value, &binding);
                    prepared.observations.push(json!({"kind":"prefetch", "request":request, "response":value, "evidence":evidence}));
                }
                Err(error) => prepared.observations.push(json!({"kind":"prefetch_unavailable", "message":error.to_string().chars().take(256).collect::<String>()})),
            }
        }
        prepared.observations.append(&mut self.observations);
        if self.working_set_enabled {
            prepared
                .observations
                .extend(self.refresh.prepare(&self.session, writers).await);
        }
        Ok(Some(prepared))
    }
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        let mut tools = self.coding.descriptors();
        tools.extend(context::NAMES.iter().map(|name| descriptor(name)));
        tools.push(descriptor("load_instructions"));
        tools
    }

    fn validate(&self, name: &str, arguments: &Value) -> std::result::Result<(), ToolError> {
        let valid = if name == "load_instructions" {
            serde_json::from_value::<Selection>(arguments.clone())
                .is_ok_and(|s| s.paths.len() <= 16 && s.skills.len() <= 8)
        } else if name == "repository_fallback" {
            serde_json::from_value::<Fallback>(arguments.clone()).is_ok()
        } else if context::NAMES.contains(&name) {
            Request::parse(name, arguments.clone()).is_ok()
        } else {
            return self.coding.validate(name, arguments);
        };

        if valid {
            Ok(())
        } else {
            Err(ToolError::InvalidArguments)
        }
    }

    async fn execute(&mut self, call: &ValidatedCall, cancellation: &Cancellation) -> ToolOutcome {
        if cancellation.is_cancelled() {
            return ToolOutcome::Failed(ToolError::Interrupted);
        }

        if self.session.status().await.is_err() {
            return ToolOutcome::Failed(ToolError::Denied);
        }

        if !context::NAMES.contains(&call.name.as_str()) && call.name != "load_instructions" {
            if matches!(call.name.as_str(), "exec" | "apply_patch") {
                self.invalidate_unknown().await;
            }
            return self.coding.execute(call, cancellation).await;
        }

        let result: Result<Value> = async {
            if call.name == "load_instructions" {
                let selected: Selection = serde_json::from_value(call.arguments.clone())?;
                Ok(serde_json::to_value(
                    self.instructions
                        .load(&selected.paths, &selected.skills)
                        .await?,
                )?)
            } else if call.name == "repository_fallback" {
                self.context
                    .fallback(
                        serde_json::from_value(call.arguments.clone())?,
                        cancellation,
                    )
                    .await
            } else {
                Ok(serde_json::to_value(
                    self.context
                        .retrieve(
                            &call.name,
                            Request::parse(&call.name, call.arguments.clone())?,
                        )
                        .await?,
                )?)
            }
        }
        .await;

        match result {
            Ok(value) if super::journal::encode(&value, 32 * 1024).is_ok() => {
                ToolOutcome::Success(value)
            }
            Ok(_) => ToolOutcome::Failed(ToolError::OutputLimit),
            Err(error) => ToolOutcome::Failed(ToolError::Context(
                json!({"code":"context_unavailable", "message":error.to_string().chars().take(256).collect::<String>(), "canonical_fallback":false}),
            )),
        }
    }

    async fn shutdown(&mut self) -> bool {
        self.before_mutation().await;
        self.coding.shutdown().await
    }
}
