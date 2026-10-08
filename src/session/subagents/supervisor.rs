use std::sync::Arc;
use std::time::Duration;

use super::watchdog::{Clock, Evidence, InstinctJudge, Judge, RealClock, Watchdog};

#[cfg(debug_assertions)]
#[derive(Clone)]
pub(crate) struct TestWatchdog {
    clock: Arc<dyn Clock>,
    judge: Arc<dyn Judge>,
}
#[cfg(debug_assertions)]
impl std::fmt::Debug for TestWatchdog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TestWatchdog")
    }
}
use crate::provider::config::load_models;
use crate::provider::openai::ClientCache;
use crate::session::service::{Lifecycle, Service, SessionId};

impl Service {
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn install_watchdog_test(&self, clock: Arc<dyn Clock>, judge: Arc<dyn Judge>) {
        *self.watchdog_test.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(TestWatchdog { clock, judge });
    }
    pub(crate) fn start_child_watchdog(&self, child: SessionId, interval: u64) {
        if interval == 0 {
            return;
        }
        let Some(handle) = self.handle(child) else {
            return;
        };
        let judge = load_models(
            self.config.models_path.as_deref(),
            &handle.workspace,
            &self.config.config_dir,
        )
        .ok()
        .and_then(|(models, _)| {
            crate::provider::layers::available_instinct(
                &models,
                &self.config.config_dir,
                Arc::new(ClientCache::new()),
            )
        })
        .map(|client| Arc::new(InstinctJudge(client)) as Arc<dyn Judge>);
        let service = self.clone();
        let clock: Arc<dyn Clock> = Arc::new(RealClock);
        #[cfg(debug_assertions)]
        let (clock, judge) = match self
            .watchdog_test
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            Some(test) => (test.clock, Some(test.judge)),
            None => (clock, judge),
        };
        let activity_clock = clock.clone();
        handle
            .status
            .activity
            .install_clock(Arc::new(move || activity_clock.now()));
        tokio::spawn(async move {
            let interval = Duration::from_secs(interval);
            let mut watchdog = Watchdog::new(clock.clone(), interval);
            loop {
                tokio::select! {
                    biased;
                    _ = handle.session_cancel.cancelled() => break,
                    _ = handle.status.watchdog.supervisor_cancel.cancelled() => break,
                    _ = clock.sleep(interval) => {}
                }
                let Ok(Some(link)) = service.child_metadata(child).await else {
                    break;
                };
                if link.status != "running" {
                    break;
                }
                if !watchdog.due()
                    || handle.lifecycle != Lifecycle::Active
                    || !handle.status.watchdog_eligible()
                {
                    continue;
                }
                let evidence = {
                    // Snapshot the turn identity before awaiting the judge.
                    let activity = handle
                        .status
                        .activity
                        .0
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    Evidence {
                        goal: link.goal,
                        since_last_message_secs: clock
                            .now()
                            .saturating_duration_since(activity.last_message)
                            .as_secs(),
                        since_last_tool_call_secs: clock
                            .now()
                            .saturating_duration_since(activity.last_tool)
                            .as_secs(),
                        last_tool_call: activity.last_tool_call.clone(),
                        last_tool_result: activity.last_tool_result.clone(),
                        step_count: activity.steps,
                    }
                    .bounded()
                };
                let Some(generation) = handle.status.watchdog.generation() else {
                    continue;
                };
                if let Some(checked) = watchdog
                    .check(judge.as_deref(), &evidence, &handle.session_cancel)
                    .await
                {
                    if let Err(error) = service
                        .apply_watchdog(link.parent_id, child, generation, checked, evidence)
                        .await
                    {
                        tracing::error!(%child, %error, "child watchdog outcome failed");
                    }
                }
            }
        });
    }
}
