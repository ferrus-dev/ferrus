use std::sync::Arc;

use anyhow::Result;
use neva::App;
use neva::types::ToolSchema;

use crate::agent_id::{ENV_AGENT_ID, ROLE_EXECUTOR, ROLE_SUPERVISOR, agent_id};
use crate::platform;

mod prompts;
mod resources;
pub(crate) mod tools;

#[derive(Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Role {
    Supervisor,
    Executor,
}

pub async fn start(role: Option<Role>, agent_name: String, agent_index: u32) -> Result<()> {
    platform::set_serve_process_name();
    platform::install_serve_parent_lifecycle_hooks();

    let role_str = match &role {
        Some(Role::Supervisor) => ROLE_SUPERVISOR,
        Some(Role::Executor) => ROLE_EXECUTOR,
        None => "agent",
    };
    let agent_id = Arc::new(
        std::env::var(ENV_AGENT_ID)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| agent_id(role_str, &agent_name, agent_index)),
    );

    let mut app = App::new().with_options(|opt| {
        opt.with_stdio()
            .with_name("ferrus")
            .with_version(env!("CARGO_PKG_VERSION"))
            .with_mcp_version("2025-03-26")
    });

    let all_roles = role.is_none();
    let supervisor_role = role.as_ref().is_some_and(|r| *r == Role::Supervisor);
    let executor_role = role.as_ref().is_some_and(|r| *r == Role::Executor);
    let sup = all_roles || supervisor_role;
    let exe = all_roles || executor_role;

    if sup {
        if all_roles {
            app.map_tool("create_task", tools::create_task::handler)
                .with_description(tools::create_task::DESCRIPTION)
                .with_input_schema(|_| ToolSchema::from_json_str(tools::create_task::INPUT_SCHEMA));
        }
        app.map_tool("enqueue_task", tools::enqueue_task::handler)
            .with_description(tools::enqueue_task::DESCRIPTION)
            .with_input_schema(|_| ToolSchema::from_json_str(tools::enqueue_task::INPUT_SCHEMA));
        app.map_tool("create_spec", tools::create_spec::handler)
            .with_description(tools::create_spec::DESCRIPTION)
            .with_input_schema(|_| ToolSchema::from_json_str(tools::create_spec::INPUT_SCHEMA));
        {
            let id = agent_id.clone();
            app.map_tool("wait_for_review", move || {
                let id = id.clone();
                async move { tools::wait_for_review::handler(&id).await }
            })
            .with_description(tools::wait_for_review::DESCRIPTION);
        }
        {
            let id = agent_id.clone();
            app.map_tool("review_pending", move || {
                let id = id.clone();
                async move { tools::review_pending::handler_for_agent(&id).await }
            })
            .with_description(tools::review_pending::DESCRIPTION);
        }
        {
            let id = agent_id.clone();
            app.map_tool("approve", move || {
                let id = id.clone();
                async move { tools::approve::handler_for_agent(&id).await }
            })
            .with_description(tools::approve::DESCRIPTION);
        }
        {
            let id = agent_id.clone();
            app.map_tool("reject", move |notes| {
                let id = id.clone();
                async move { tools::reject::handler_for_agent(&id, notes).await }
            })
            .with_description(tools::reject::DESCRIPTION)
            .with_input_schema(|_| ToolSchema::from_json_str(tools::reject::INPUT_SCHEMA));
        }
        {
            let id = agent_id.clone();
            app.map_tool("wait_for_consultation", move || {
                let id = id.clone();
                async move { tools::wait_for_consultation::handler(&id).await }
            })
            .with_description(tools::wait_for_consultation::DESCRIPTION);
        }
        {
            let id = agent_id.clone();
            app.map_tool("respond_consult", move |response| {
                let id = id.clone();
                async move { tools::respond_consult::handler_for_agent(&id, response).await }
            })
            .with_description(tools::respond_consult::DESCRIPTION)
            .with_input_schema(|_| ToolSchema::from_json_str(tools::respond_consult::INPUT_SCHEMA));
        }
    }

    if exe {
        {
            let id = agent_id.clone();
            app.map_tool("wait_for_task", move || {
                let id = id.clone();
                async move { tools::wait_for_task::handler(&id).await }
            })
            .with_description(tools::wait_for_task::DESCRIPTION);
        }
        {
            let id = agent_id.clone();
            app.map_tool("check", move || {
                let id = id.clone();
                async move { tools::check::handler_for_agent(&id).await }
            })
            .with_description(tools::check::DESCRIPTION);
        }
        {
            let id = agent_id.clone();
            app.map_tool("consult", move |question| {
                let id = id.clone();
                async move { tools::consult::handler_for_agent(&id, question).await }
            })
            .with_description(tools::consult::DESCRIPTION)
            .with_input_schema(|_| ToolSchema::from_json_str(tools::consult::INPUT_SCHEMA));
        }
        {
            let id = agent_id.clone();
            app.map_tool("submit", move |content| {
                let id = id.clone();
                async move { tools::submit::handler_for_agent(&id, content).await }
            })
            .with_description(tools::submit::DESCRIPTION)
            .with_input_schema(|_| ToolSchema::from_json_str(tools::submit::INPUT_SCHEMA));
        }
        {
            let id = agent_id.clone();
            app.map_tool("wait_for_consult", move || {
                let id = id.clone();
                async move { tools::wait_for_consult::handler_for_agent(&id).await }
            })
            .with_description(tools::wait_for_consult::DESCRIPTION);
        }
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
    {
        let id = agent_id.clone();
        app.map_resource("ferrus://{file}", "ferrus-file", move |file| {
            let id = id.clone();
            async move { resources::read_for_agent(Some(id.as_str()), file).await }
        });
    }

    // Prompts
    {
        let id = agent_id.clone();
        app.map_prompt("executor-context", move || {
            let id = id.clone();
            async move { prompts::executor_context_for_agent(Some(id.as_str())).await }
        })
        .with_description("Executor task context: state, task, and review notes");
    }
    {
        let id = agent_id.clone();
        app.map_prompt("supervisor-review", move || {
            let id = id.clone();
            async move { prompts::supervisor_review_for_agent(Some(id.as_str())).await }
        })
        .with_description("Supervisor review context: state, task, and submission notes");
    }

    // Shared tools are role-scoped so tools/list fits Neva's 10-item page size.
    if all_roles || supervisor_role || executor_role {
        let id = agent_id.clone();
        app.map_tool("ask_human", move |question| {
            let id = id.clone();
            async move { tools::ask_human::handler_for_agent(&id, question).await }
        })
        .with_description(tools::ask_human::DESCRIPTION)
        .with_input_schema(|_| ToolSchema::from_json_str(tools::ask_human::INPUT_SCHEMA));
    }
    if all_roles || supervisor_role || executor_role {
        let id = agent_id.clone();
        app.map_tool("wait_for_answer", move || {
            let id = id.clone();
            async move { tools::wait_for_answer::handler_for_agent(&id).await }
        })
        .with_description(tools::wait_for_answer::DESCRIPTION);
    }
    if all_roles {
        app.map_tool("answer", tools::answer::handler)
            .with_description(tools::answer::DESCRIPTION)
            .with_input_schema(|_| ToolSchema::from_json_str(tools::answer::INPUT_SCHEMA));
    }
    if all_roles || executor_role {
        let id = agent_id.clone();
        app.map_tool("status", move || {
            let id = id.clone();
            async move { tools::status::handler_for_agent(&id).await }
        })
        .with_description(tools::status::DESCRIPTION);
    }
    if all_roles || executor_role {
        let id = agent_id.clone();
        app.map_tool("reset", move || {
            let id = id.clone();
            async move { tools::reset::handler_for_agent(&id).await }
        })
        .with_description(tools::reset::DESCRIPTION);
    }
    if all_roles || executor_role {
        let id = agent_id.clone();
        app.map_tool("heartbeat", move || {
            let id = id.clone();
            async move { tools::heartbeat::handler_for_agent(&id).await }
        })
        .with_description(tools::heartbeat::DESCRIPTION);
    }

    app.run().await;
    Ok(())
}
