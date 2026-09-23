//! Debounced host maintenance. Read-only retrieval never calls this scheduler.

use super::ferrus::FerrusSession;
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

#[derive(Default)]
pub(super) struct Refresh {
    dirty: Option<Instant>,
    attempts: usize,
    task: Option<JoinHandle<Value>>,
}

impl Refresh {
    pub fn invalidate(&mut self) {
        self.dirty = Some(Instant::now());
    }

    pub async fn settle(&mut self) -> Option<Value> {
        // Retain ownership if the caller is cancelled while awaiting maintenance.
        let result = self.task.as_mut()?.await;
        self.task.take();
        Some(match result {
            Ok(value) => value,
            Err(_) => json!({"kind":"overlay_refresh", "status":"failed"}),
        })
    }

    pub async fn prepare(&mut self, session: &FerrusSession, writers: bool) -> Vec<Value> {
        let mut observations = Vec::new();
        if self.task.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(value) = self.settle().await
        {
            observations.push(value);
        }
        if !self.take_due(Instant::now(), writers) {
            return observations;
        }
        let session = session.clone();
        self.task = Some(tokio::spawn(async move {
            let result = async {
                let runtime = session.authorize().await?;
                let Some(baseline) = session.baseline_tree() else {
                    return Ok(None);
                };
                let graph = crate::repository_graph_runtime::LocalGraphContext::load_for_runtime(
                    session.project_root(),
                    session.project_id(),
                    session.data_dir(),
                    &runtime,
                )
                .await?;
                if !graph.config.enabled {
                    return Ok(None);
                }
                crate::repository_graph_runtime::refresh_task_overlay_explicit(
                    graph,
                    session.data_dir(),
                    &runtime,
                    baseline,
                )
                .await
                .map(Some)
            }
            .await;
            match result {
                Ok(Some(view)) => {
                    json!({"kind":"overlay_refresh", "status":"published", "view":super::working_set::view_identity(&view)})
                }
                Ok(None) => json!({"kind":"overlay_refresh", "status":"disabled"}),
                Err(error) => {
                    json!({"kind":"overlay_refresh", "status":"failed", "message":error.to_string().chars().take(256).collect::<String>()})
                }
            }
        }));
        observations
            .push(json!({"kind":"overlay_refresh", "status":"scheduled", "attempt":self.attempts}));
        observations
    }

    fn take_due(&mut self, now: Instant, writers: bool) -> bool {
        if writers
            || self.task.is_some()
            || self.attempts >= 4
            || self.dirty.is_none_or(|dirty| {
                now.saturating_duration_since(dirty) < Duration::from_millis(250)
            })
        {
            return false;
        }
        self.dirty = None;
        self.attempts += 1;
        true
    }

    #[cfg(test)]
    pub(super) fn elapse_debounce(&mut self) {
        self.dirty = Some(Instant::now() - Duration::from_millis(250));
    }
}

impl Drop for Refresh {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounce_coalesces_changes_and_defers_owned_writers_under_a_hard_cap() {
        let now = Instant::now();
        let mut refresh = Refresh::default();
        assert!(!refresh.take_due(now, false));
        refresh.dirty = Some(now);
        assert!(!refresh.take_due(now + Duration::from_millis(249), false));
        refresh.dirty = Some(now + Duration::from_millis(200));
        assert!(!refresh.take_due(now + Duration::from_millis(300), false));
        assert!(!refresh.take_due(now + Duration::from_secs(1), true));
        assert!(refresh.take_due(now + Duration::from_secs(1), false));
        assert!(!refresh.take_due(now + Duration::from_secs(1), false));
        for _ in 0..3 {
            refresh.dirty = Some(now);
            assert!(refresh.take_due(now + Duration::from_secs(1), false));
        }
        refresh.dirty = Some(now);
        assert!(!refresh.take_due(now + Duration::from_secs(1), false));
    }

    #[tokio::test]
    async fn cancellation_does_not_detach_an_unfinished_refresh() {
        let (send, receive) = tokio::sync::oneshot::channel();
        let mut refresh = Refresh {
            task: Some(tokio::spawn(async move { receive.await.unwrap() })),
            ..Default::default()
        };
        let mut settle = Box::pin(refresh.settle());
        tokio::select! {
            biased;
            _ = &mut settle => panic!("refresh must be pending"),
            _ = std::future::ready(()) => (),
        }
        drop(settle);
        assert!(refresh.task.is_some());
        send.send(json!({"status":"published"})).unwrap();
        assert_eq!(refresh.settle().await.unwrap()["status"], "published");
    }
}
