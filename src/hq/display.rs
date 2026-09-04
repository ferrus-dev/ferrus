//! Translate HQ output into terminal messages and formatted tables.

use tokio::sync::{mpsc, oneshot};

use crate::state::agents::{AgentStatus, AgentsRegistry};

use super::{
    state_watcher::WatchedState,
    tui::{StatusSnapshot, UiMessage},
};

#[derive(Clone)]
pub struct Display(pub mpsc::UnboundedSender<UiMessage>);

impl Display {
    pub fn info(&self, msg: impl Into<String>) {
        let _ = self.0.send(UiMessage::Info(msg.into()));
    }

    pub fn success(&self, msg: impl Into<String>) {
        let _ = self.0.send(UiMessage::Success(msg.into()));
    }

    pub fn info_block(&self, lines: impl IntoIterator<Item = String>) {
        let text = lines.into_iter().collect::<Vec<_>>().join("\n");
        if !text.is_empty() {
            self.info(text);
        }
    }

    pub fn table(&self, lines: impl IntoIterator<Item = String>) {
        let lines = lines.into_iter().collect::<Vec<_>>();
        match lines.len() {
            0 => {}
            1 => self.info(lines.into_iter().next().unwrap_or_default()),
            _ => {
                let _ = self.0.send(UiMessage::Table(lines));
            }
        }
    }

    pub fn tip(&self, msg: impl Into<String>) {
        let _ = self.0.send(UiMessage::Tip(msg.into()));
    }

    pub fn muted(&self, msg: impl Into<String>) {
        let _ = self.0.send(UiMessage::Muted(msg.into()));
    }

    pub fn error(&self, msg: impl Into<String>) {
        let _ = self.0.send(UiMessage::Error(msg.into()));
    }

    pub fn status(&self, watched: &WatchedState, agents: &AgentsRegistry) {
        let mut snapshot = StatusSnapshot::from_watched_state(watched);
        snapshot.supervisor_status =
            agent_status_label(agents, crate::agent_id::ROLE_SUPERVISOR).to_string();
        snapshot.executor_status =
            agent_status_label(agents, crate::agent_id::ROLE_EXECUTOR).to_string();
        let _ = self.0.send(UiMessage::StatusUpdate(snapshot));

        let mut lines = Vec::new();
        if let Some(spec) = &watched.selected_spec_display {
            lines.push(format!("spec       : {spec}"));
        }
        if agents.agents.is_empty() {
            lines.push("agents     : none".to_string());
        } else {
            for agent in &agents.agents {
                let status = match agent.status {
                    AgentStatus::Idle => "idle",
                    AgentStatus::Running => "running",
                    AgentStatus::Suspended => "suspended",
                };
                let pid = agent
                    .pid
                    .map(|pid| format!(" pid={pid}"))
                    .unwrap_or_default();
                lines.push(format!("  [{:<10}] {:<10}{}", agent.role, status, pid));
            }
        }
        self.info_block(lines);
    }

    pub fn suspend(&self) -> oneshot::Receiver<()> {
        let (ack_tx, ack_rx) = oneshot::channel();
        let _ = self.0.send(UiMessage::Suspend { ack: ack_tx });
        ack_rx
    }

    pub fn resume(&self) {
        let _ = self.0.send(UiMessage::Resume);
    }

    pub fn confirm(&self, prompt: impl Into<String>) -> oneshot::Receiver<bool> {
        self.confirm_custom(prompt, "[y/N]", false, &['y'], &['n'])
    }

    pub fn confirm_yes(&self, prompt: impl Into<String>) -> oneshot::Receiver<bool> {
        self.confirm_custom(prompt, "[Y/n]", true, &['y'], &['n'])
    }

    pub fn confirm_continue(&self, prompt: impl Into<String>) -> oneshot::Receiver<bool> {
        self.confirm_custom(prompt, "[c/N]", false, &['c'], &['n'])
    }

    fn confirm_custom(
        &self,
        prompt: impl Into<String>,
        suffix: impl Into<String>,
        default: bool,
        accept_keys: &[char],
        reject_keys: &[char],
    ) -> oneshot::Receiver<bool> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.0.send(UiMessage::ConfirmationRequest {
            prompt: prompt.into(),
            suffix: suffix.into(),
            default,
            accept_keys: accept_keys.to_vec(),
            reject_keys: reject_keys.to_vec(),
            reply: reply_tx,
        });
        reply_rx
    }

    pub fn select(
        &self,
        prompt: impl Into<String>,
        options: Vec<String>,
    ) -> oneshot::Receiver<Option<usize>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.0.send(UiMessage::SelectionRequest {
            prompt: prompt.into(),
            options,
            reply: reply_tx,
        });
        reply_rx
    }
}

fn agent_status_label<'a>(agents: &'a AgentsRegistry, role: &str) -> &'a str {
    match agents.by_role(role).map(|entry| &entry.status) {
        Some(AgentStatus::Idle) => "idle",
        Some(AgentStatus::Running) => "running",
        Some(AgentStatus::Suspended) => "suspended",
        None => "none",
    }
}

#[cfg(test)]
mod tests {
    //! Structured UI messages and compact agent-status rendering.

    use tokio::sync::mpsc;

    use crate::{
        agent_id::ROLE_SUPERVISOR,
        hq::{state_watcher::WatchedState, tui::UiMessage},
        state::agents::{AgentEntry, AgentStatus, AgentsRegistry},
    };

    use super::Display;

    #[test]
    fn info_block_sends_one_multiline_message() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let display = Display(tx);

        display.info_block(vec!["first".to_string(), "second".to_string()]);

        let msg = rx.try_recv().expect("message should be sent");
        match msg {
            UiMessage::Info(text) => assert_eq!(text, "first\nsecond"),
            _ => panic!("expected info message"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn table_sends_structured_rows() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let display = Display(tx);

        display.table(vec!["ID  Status".to_string(), "1   pending".to_string()]);

        let msg = rx.try_recv().expect("message should be sent");
        match msg {
            UiMessage::Table(lines) => {
                assert_eq!(lines, vec!["ID  Status", "1   pending"]);
            }
            _ => panic!("expected table message"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn single_table_message_stays_plain() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let display = Display(tx);

        display.table(vec!["No tasks recorded in ferrus.db.".to_string()]);

        let msg = rx.try_recv().expect("message should be sent");
        match msg {
            UiMessage::Info(text) => assert_eq!(text, "No tasks recorded in ferrus.db."),
            _ => panic!("expected info message"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn success_sends_success_message() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let display = Display(tx);

        display.success("Task t-001 completed.");

        let msg = rx.try_recv().expect("message should be sent");
        match msg {
            UiMessage::Success(text) => assert_eq!(text, "Task t-001 completed."),
            _ => panic!("expected success message"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn status_sends_details_as_one_transcript_block() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let display = Display(tx);
        let watched = WatchedState {
            selected_spec_display: Some("spec".into()),
            selected_milestones: Vec::new(),
        };
        let agents = AgentsRegistry {
            agents: vec![AgentEntry {
                role: ROLE_SUPERVISOR.into(),
                agent_type: "codex".into(),
                name: "supervisor".into(),
                pid: None,
                status: AgentStatus::Suspended,
                started_at: None,
            }],
        };

        display.status(&watched, &agents);

        assert!(matches!(
            rx.try_recv().expect("status update should be sent"),
            UiMessage::StatusUpdate(_)
        ));
        let msg = rx.try_recv().expect("status details should be sent");
        match msg {
            UiMessage::Info(text) => {
                assert!(text.contains("spec       : spec\n"));
                assert!(!text.contains("milestone  :"));
                assert!(text.contains("  [supervisor] suspended"));
            }
            _ => panic!("expected info message"),
        }
        assert!(rx.try_recv().is_err());
    }
}
