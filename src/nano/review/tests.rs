use super::*;
#[test]
fn authority_and_schemas_follow_the_host_profile() {
    for mode in [Mode::Review, Mode::Consult] {
        let tools = role_tools(mode);
        for forbidden in [
            "exec",
            "apply_patch",
            "submit",
            "enqueue_task",
            "create_spec",
            "archive_spec",
            "mcp_local_write",
        ] {
            assert!(!readable(forbidden));
            assert!(!tools.iter().any(|tool| tool.name == forbidden));
        }
        assert_eq!(
            tools.iter().any(|tool| tool.name == "approve"),
            mode == Mode::Review
        );
        assert_eq!(
            tools.iter().any(|tool| tool.name == "respond_consult"),
            mode == Mode::Consult
        );
        for (name, field) in [
            ("ask_human", "question"),
            ("reject", "notes"),
            ("respond_consult", "response"),
        ] {
            if let Some(tool) = tools.iter().find(|tool| tool.name == name) {
                assert_eq!(tool.input_schema["properties"][field]["type"], "string");
            }
        }
    }
}
