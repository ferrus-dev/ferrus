//! Composition of native coding tools; Ferrus lifecycle tools are a separate adapter.

use super::{
    commands::{Commands, ExecutionBackend},
    tools::*,
    workspace::Workspace,
};

pub(crate) struct CodingTools<B: ExecutionBackend> {
    pub workspace: Workspace,
    pub commands: Commands<B>,
}

impl<B: ExecutionBackend> Tools for CodingTools<B> {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        let mut tools = self.workspace.descriptors();
        tools.extend(self.commands.descriptors());
        tools
    }

    fn validate(&self, name: &str, arguments: &serde_json::Value) -> Result<(), ToolError> {
        if super::commands::is_tool(name) {
            self.commands.validate(name, arguments)
        } else {
            self.workspace.validate(name, arguments)
        }
    }

    async fn execute(&mut self, call: &ValidatedCall, cancellation: &Cancellation) -> ToolOutcome {
        if super::commands::is_tool(&call.name) {
            if call.name == "exec" {
                self.workspace.invalidate_for_command();
            }

            self.commands.execute(call, cancellation).await
        } else {
            self.workspace.execute(call, cancellation).await
        }
    }

    async fn shutdown(&mut self) -> bool {
        self.commands.shutdown().await
    }
}
