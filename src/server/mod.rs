use anyhow::Result;
use neva::App;
use neva::types::ToolSchema;

use crate::agent_id::{ENV_AGENT_ID, ENV_TASK_ID, ROLE_EXECUTOR, ROLE_SUPERVISOR, agent_id};
use crate::platform;

mod prompts;
mod resources;
pub(crate) mod tools;

#[derive(Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Role {
    Supervisor,
    Executor,
}

impl Role {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Supervisor => ROLE_SUPERVISOR,
            Self::Executor => ROLE_EXECUTOR,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ServerContext {
    agent_id: String,
    role: Option<Role>,
    agent_name: String,
    agent_index: u32,
}

impl ServerContext {
    fn new(agent_id: String, role: Option<Role>, agent_name: String, agent_index: u32) -> Self {
        Self {
            agent_id,
            role,
            agent_name,
            agent_index,
        }
    }

    pub(crate) fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub(crate) fn role(&self) -> Option<&Role> {
        self.role.as_ref()
    }

    pub(crate) fn agent_name(&self) -> &str {
        &self.agent_name
    }

    pub(crate) fn agent_index(&self) -> u32 {
        self.agent_index
    }
}

pub async fn start(role: Option<Role>, agent_name: String, agent_index: u32) -> Result<()> {
    platform::set_serve_process_name();
    platform::install_serve_parent_lifecycle_hooks();

    let role_str = match &role {
        Some(Role::Supervisor) => ROLE_SUPERVISOR,
        Some(Role::Executor) => ROLE_EXECUTOR,
        None => "agent",
    };
    let agent_id = std::env::var(ENV_AGENT_ID)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| agent_id(role_str, &agent_name, agent_index));
    let server_context = ServerContext::new(agent_id, role.clone(), agent_name, agent_index);

    let mut app = App::new()
        .add_singleton(server_context)
        .with_options(|opt| {
            opt.with_stdio()
                .with_name("ferrus")
                .with_version(env!("CARGO_PKG_VERSION"))
                .with_mcp_version("2025-03-26")
        });

    let all_roles = role.is_none();
    let supervisor_role = role.as_ref().is_some_and(|r| *r == Role::Supervisor);
    let executor_role = role.as_ref().is_some_and(|r| *r == Role::Executor);
    let task_scoped_supervisor =
        supervisor_role && task_scope_is_present(std::env::var(ENV_TASK_ID).ok().as_deref());
    let sup = all_roles || supervisor_role;
    let exe = all_roles || executor_role;

    if sup {
        if all_roles {
            app.map_tool("create_task", tools::create_task::handler)
                .with_description(tools::create_task::DESCRIPTION)
                .with_input_schema(|_| ToolSchema::from_json_str(tools::create_task::INPUT_SCHEMA));
        }
        if all_roles || !task_scoped_supervisor {
            app.map_tool("enqueue_task", tools::enqueue_task::handler)
                .with_description(tools::enqueue_task::DESCRIPTION)
                .with_input_schema(|_| {
                    ToolSchema::from_json_str(tools::enqueue_task::INPUT_SCHEMA)
                });
            app.map_tool("create_spec", tools::create_spec::handler)
                .with_description(tools::create_spec::DESCRIPTION)
                .with_input_schema(|_| ToolSchema::from_json_str(tools::create_spec::INPUT_SCHEMA));
        }
        app.map_tool("wait_for_review", tools::wait_for_review::handler)
            .with_description(tools::wait_for_review::DESCRIPTION);
        app.map_tool("review_pending", tools::review_pending::handler)
            .with_description(tools::review_pending::DESCRIPTION);
        app.map_tool("approve", tools::approve::handler)
            .with_description(tools::approve::DESCRIPTION);
        app.map_tool("reject", tools::reject::handler)
            .with_description(tools::reject::DESCRIPTION)
            .with_input_schema(|_| ToolSchema::from_json_str(tools::reject::INPUT_SCHEMA));
        app.map_tool(
            "wait_for_consultation",
            tools::wait_for_consultation::handler,
        )
        .with_description(tools::wait_for_consultation::DESCRIPTION);
        app.map_tool("respond_consult", tools::respond_consult::handler)
            .with_description(tools::respond_consult::DESCRIPTION)
            .with_input_schema(|_| ToolSchema::from_json_str(tools::respond_consult::INPUT_SCHEMA));
    }

    if exe {
        app.map_tool("wait_for_task", tools::wait_for_task::handler)
            .with_description(tools::wait_for_task::DESCRIPTION);
        app.map_tool("check", tools::check::handler)
            .with_description(tools::check::DESCRIPTION);
        app.map_tool("consult", tools::consult::handler)
            .with_description(tools::consult::DESCRIPTION)
            .with_input_schema(|_| ToolSchema::from_json_str(tools::consult::INPUT_SCHEMA));
        app.map_tool("submit", tools::submit::handler)
            .with_description(tools::submit::DESCRIPTION)
            .with_input_schema(|_| ToolSchema::from_json_str(tools::submit::INPUT_SCHEMA));
        app.map_tool("wait_for_consult", tools::wait_for_consult::handler)
            .with_description(tools::wait_for_consult::DESCRIPTION);
    }

    // Resources
    app.add_resource("ferrus://task", "Task");
    app.add_resource("ferrus://task_template", "Task Template");
    app.add_resource("ferrus://review", "Review Notes");
    app.add_resource("ferrus://submission", "Submission");
    app.add_resource("ferrus://question", "Question");
    app.add_resource("ferrus://answer", "Answer");
    app.add_resource("ferrus://consult_template", "Consultation Template");
    app.add_resource("ferrus://spec_template", "Specification Template");
    app.add_resource("ferrus://consult_request", "Consult Request");
    app.add_resource("ferrus://consult_response", "Consult Response");
    app.add_resource("ferrus://state", "State");
    app.add_resource("ferrus://runtime_context", "Runtime Context");
    app.map_resource(
        "ferrus://task/{task_id}",
        "ferrus-task",
        resources::read_task_by_id,
    );
    app.map_resource("ferrus://{file}", "ferrus-file", resources::read);

    // Prompts
    app.map_prompt("executor-context", prompts::executor_context)
        .with_description("Executor task context: state, task, and review notes");
    app.map_prompt("supervisor-review", prompts::supervisor_review)
        .with_description("Supervisor review context: state, task, and submission notes");

    // Shared tools are role-scoped so tools/list fits Neva's 10-item page size.
    if all_roles || supervisor_role || executor_role {
        app.map_tool("ask_human", tools::ask_human::handler)
            .with_description(tools::ask_human::DESCRIPTION)
            .with_input_schema(|_| ToolSchema::from_json_str(tools::ask_human::INPUT_SCHEMA));
    }
    if all_roles || supervisor_role || executor_role {
        app.map_tool("wait_for_answer", tools::wait_for_answer::handler)
            .with_description(tools::wait_for_answer::DESCRIPTION);
    }
    if all_roles {
        app.map_tool("answer", tools::answer::handler)
            .with_description(tools::answer::DESCRIPTION)
            .with_input_schema(|_| ToolSchema::from_json_str(tools::answer::INPUT_SCHEMA));
    }
    if all_roles || executor_role {
        app.map_tool("status", tools::status::handler)
            .with_description(tools::status::DESCRIPTION);
    }
    if all_roles || executor_role {
        app.map_tool("reset", tools::reset::handler)
            .with_description(tools::reset::DESCRIPTION);
    }
    if all_roles || executor_role || task_scoped_supervisor {
        app.map_tool("heartbeat", tools::heartbeat::handler)
            .with_description(tools::heartbeat::DESCRIPTION);
    }

    app.run().await;
    Ok(())
}

fn task_scope_is_present(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_scoped_supervisor_mapping_uses_heartbeat_instead_of_definition_tools() {
        let all_roles = false;
        let executor_role = false;
        let task_scoped_supervisor = task_scope_is_present(Some("t-001"));

        assert!(!(all_roles || !task_scoped_supervisor));
        assert!(all_roles || executor_role || task_scoped_supervisor);
    }

    #[test]
    fn interactive_supervisor_mapping_keeps_definition_tools_without_heartbeat() {
        let all_roles = false;
        let executor_role = false;
        let task_scoped_supervisor = task_scope_is_present(Some("  "));

        assert!(all_roles || !task_scoped_supervisor);
        assert!(!(all_roles || executor_role || task_scoped_supervisor));
    }

    #[test]
    fn executor_and_all_role_servers_still_expose_heartbeat() {
        let task_scoped_supervisor = task_scope_is_present(None);

        let all_roles = false;
        let executor_role = true;
        assert!(all_roles || executor_role || task_scoped_supervisor);

        let all_roles = true;
        let executor_role = false;
        assert!(all_roles || executor_role || task_scoped_supervisor);
    }
}
