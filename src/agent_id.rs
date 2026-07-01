pub const ROLE_SUPERVISOR: &str = "supervisor";
pub const ROLE_EXECUTOR: &str = "executor";
pub const DEFAULT_AGENT_INDEX: u32 = 1;
pub const ENV_AGENT_ID: &str = "FERRUS_AGENT_ID";
pub const ENV_TASK_ID: &str = "FERRUS_TASK_ID";
#[allow(dead_code)]
pub const ENV_RUN_ID: &str = "FERRUS_RUN_ID";
pub const ENV_PROJECT_ROOT: &str = "FERRUS_PROJECT_ROOT";
pub const ENV_BASELINE_TREE: &str = "FERRUS_BASELINE_TREE";

pub fn agent_id(role: &str, vendor: &str, index: u32) -> String {
    format!("{role}:{vendor}:{index}")
}

pub fn mcp_server_name(role: &str) -> String {
    format!("ferrus-{role}")
}

pub fn legacy_mcp_server_name(role: &str, index: u32) -> String {
    format!("ferrus-{role}-{index}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_structured_agent_id() {
        assert_eq!(agent_id("executor", "codex", 1), "executor:codex:1");
    }

    #[test]
    fn builds_mcp_server_name() {
        assert_eq!(mcp_server_name("supervisor"), "ferrus-supervisor");
        assert_eq!(
            legacy_mcp_server_name("supervisor", 2),
            "ferrus-supervisor-2"
        );
    }
}
