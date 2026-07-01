use std::time::SystemTime;

use tokio::sync::watch;

use crate::project::{self, ProjectSelection};
use crate::specs::{self, MilestoneReadiness};

#[derive(Clone, Debug)]
pub struct WatchedState {
    pub selected_spec_display: Option<String>,
    pub selected_milestones: Vec<WatchedMilestone>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchedMilestone {
    pub marker: String,
    pub title: String,
    pub completed: bool,
    pub readiness: MilestoneReadiness,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SelectedDisplayCacheKey {
    selected_spec: Option<String>,
    spec_fingerprint: Option<(Option<SystemTime>, u64)>,
}

#[derive(Clone, Debug, Default)]
struct SelectedDisplayCache {
    key: Option<SelectedDisplayCacheKey>,
    value: (Option<String>, Vec<WatchedMilestone>),
}

impl SelectedDisplayCache {
    async fn is_stale(&self, selection: &ProjectSelection) -> bool {
        let key = SelectedDisplayCacheKey {
            selected_spec: selection.selected_spec.clone(),
            spec_fingerprint: selected_spec_fingerprint(selection).await,
        };
        self.key.as_ref() != Some(&key)
    }

    async fn get(
        &mut self,
        selection: &ProjectSelection,
    ) -> (Option<String>, Vec<WatchedMilestone>) {
        let key = SelectedDisplayCacheKey {
            selected_spec: selection.selected_spec.clone(),
            spec_fingerprint: selected_spec_fingerprint(selection).await,
        };

        if self.key.as_ref() != Some(&key) {
            self.value = selected_spec_display(selection).await;
            self.key = Some(key);
        }

        self.value.clone()
    }
}

/// Watch project selection and spec milestone changes for the dashboard.
pub async fn watch(tx: watch::Sender<Option<WatchedState>>) {
    let mut selected_display_cache = SelectedDisplayCache::default();

    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;

        let selection = project::read_project_selection()
            .await
            .unwrap_or(ProjectSelection {
                selected_spec: None,
            });

        let selected_display_changed = selected_display_cache.is_stale(&selection).await;

        if selected_display_changed {
            let (selected_spec_display, selected_milestones) =
                selected_display_cache.get(&selection).await;
            let _ = tx.send(Some(WatchedState {
                selected_spec_display,
                selected_milestones,
            }));
        }
    }
}

async fn selected_spec_fingerprint(
    selection: &ProjectSelection,
) -> Option<(Option<SystemTime>, u64)> {
    let path = selection.selected_spec.as_deref()?.trim();
    if path.is_empty() {
        return None;
    }

    let metadata = tokio::fs::metadata(path).await.ok()?;
    Some((metadata.modified().ok(), metadata.len()))
}

async fn selected_spec_display(
    selection: &ProjectSelection,
) -> (Option<String>, Vec<WatchedMilestone>) {
    let selected_spec_display = selection
        .selected_spec
        .as_deref()
        .map(specs::spec_display_name)
        .map(|name| specs::compact_spec_display_name(&name))
        .filter(|name| !name.is_empty());
    let selected_milestones = match selection
        .selected_spec
        .as_deref()
        .filter(|path| !path.is_empty())
    {
        Some(path) => specs::load_spec(path)
            .await
            .map(|spec| {
                spec.milestone_plan()
                    .into_iter()
                    .map(|item| WatchedMilestone {
                        marker: item.milestone.marker,
                        title: item.milestone.title,
                        completed: item.milestone.completed,
                        readiness: item.readiness,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        None => Vec::new(),
    };

    (selected_spec_display, selected_milestones)
}

#[cfg(test)]
pub fn format_elapsed(duration: std::time::Duration) -> String {
    let total = duration.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;

    let mut parts = Vec::with_capacity(3);
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes}m"));
    }
    parts.push(format!("{seconds}s"));
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::format_elapsed;

    #[test]
    fn formats_seconds_only() {
        assert_eq!(format_elapsed(Duration::from_secs(45)), "45s");
    }

    #[test]
    fn formats_minutes_and_seconds() {
        assert_eq!(format_elapsed(Duration::from_secs(70)), "1m 10s");
    }

    #[test]
    fn formats_hours_minutes_and_seconds() {
        assert_eq!(format_elapsed(Duration::from_secs(5403)), "1h 30m 3s");
    }

    #[test]
    fn formats_zero_duration() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "0s");
    }
}
