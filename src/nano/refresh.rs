//! Debounced host maintenance. Read-only retrieval never calls this scheduler.

use super::ferrus::FerrusSession;
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

const DEBOUNCE: Duration = Duration::from_millis(250);

#[derive(Default)]
pub(super) struct Refresh {
    dirty: Option<Instant>,
    attempts: usize,
    task: Option<JoinHandle<Value>>,
}

impl Refresh {
    pub fn invalidate(&mut self) {
        // Repeated edits must not move the first pending refresh deadline.
        self.dirty.get_or_insert_with(Instant::now);
    }

    pub fn pending_reason(&self) -> Option<&'static str> {
        self.dirty.map(|_| {
            if self.attempts >= 4 {
                "overlay_refresh_limit"
            } else {
                "overlay_refresh_pending"
            }
        })
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
        let Some(dirty) = self.take_pending(writers) else {
            return observations;
        };
        let session = session.clone();
        self.task = Some(tokio::spawn(async move {
            tokio::time::sleep_until(tokio::time::Instant::from_std(dirty + DEBOUNCE)).await;
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

    fn take_pending(&mut self, writers: bool) -> Option<Instant> {
        if writers || self.task.is_some() || self.attempts >= 4 {
            return None;
        }
        let dirty = self.dirty.take()?;
        self.attempts += 1;
        Some(dirty)
    }

    #[cfg(test)]
    pub(super) fn elapse_debounce(&mut self) {
        self.dirty = Some(Instant::now() - DEBOUNCE);
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
    fn debounce_keeps_the_first_deadline_and_defers_owned_writers_under_a_hard_cap() {
        let mut refresh = Refresh::default();
        assert!(refresh.take_pending(false).is_none());
        refresh.invalidate();
        let first = refresh.dirty.unwrap();
        for _ in 0..8 {
            refresh.invalidate();
        }
        assert_eq!(refresh.dirty, Some(first));
        assert!(refresh.take_pending(true).is_none());
        assert_eq!(refresh.take_pending(false), Some(first));
        assert!(refresh.take_pending(false).is_none());
        for _ in 0..3 {
            refresh.invalidate();
            assert!(refresh.take_pending(false).is_some());
        }
        refresh.invalidate();
        assert!(refresh.take_pending(false).is_none());
        assert_eq!(refresh.pending_reason(), Some("overlay_refresh_limit"));
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
