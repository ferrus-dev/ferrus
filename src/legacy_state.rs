use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::path::Path;

use crate::runtime_status::TaskStatus;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub enum LegacyTaskState {
    Idle,
    Executing,
    Consultation,
    Reviewing,
    Addressing,
    Complete,
    Failed,
    AwaitingHuman,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LegacyStateData {
    #[serde(default)]
    pub state: Option<LegacyTaskState>,
    #[serde(default)]
    pub check_retries: u32,
    #[serde(default)]
    pub review_cycles: u32,
    #[serde(default)]
    pub failure_reason: Option<String>,
    #[serde(default)]
    pub paused_state: Option<LegacyTaskState>,
    #[serde(default)]
    pub awaiting_human_by: Option<String>,
    #[serde(default)]
    pub task_spec: Option<String>,
    #[serde(default)]
    pub task_milestone: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub selected_spec: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub selected_milestone: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

impl LegacyStateData {
    pub fn state(&self) -> LegacyTaskState {
        self.state.clone().unwrap_or(LegacyTaskState::Idle)
    }
}

pub async fn read_legacy_state(path: impl AsRef<Path>) -> Result<LegacyStateData> {
    let path = path.as_ref();
    let contents = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("Failed to read legacy {}", path.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("Failed to parse legacy {}", path.display()))
}

pub fn task_status_for_legacy_state(state: &LegacyTaskState) -> TaskStatus {
    match state {
        LegacyTaskState::Idle => TaskStatus::Reset,
        LegacyTaskState::Executing => TaskStatus::Executing,
        LegacyTaskState::Consultation => TaskStatus::Consultation,
        LegacyTaskState::Reviewing => TaskStatus::Reviewing,
        LegacyTaskState::Addressing => TaskStatus::Addressing,
        LegacyTaskState::Complete => TaskStatus::Complete,
        LegacyTaskState::Failed => TaskStatus::Failed,
        LegacyTaskState::AwaitingHuman => TaskStatus::AwaitingHuman,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_legacy_pause_and_counter_metadata() {
        let state: LegacyStateData = serde_json::from_str(
            r#"{
                "state": "AwaitingHuman",
                "paused_state": "Reviewing",
                "awaiting_human_by": "supervisor:codex:1",
                "check_retries": 2,
                "review_cycles": 1,
                "failure_reason": "last failure"
            }"#,
        )
        .unwrap();

        assert_eq!(state.paused_state, Some(LegacyTaskState::Reviewing));
        assert_eq!(
            state.awaiting_human_by.as_deref(),
            Some("supervisor:codex:1")
        );
        assert_eq!(state.check_retries, 2);
        assert_eq!(state.review_cycles, 1);
        assert_eq!(state.failure_reason.as_deref(), Some("last failure"));
    }
}
