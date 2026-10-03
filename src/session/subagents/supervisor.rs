use std::sync::Arc;
use std::time::Duration;

use super::watchdog::{Clock, Evidence, JevJudge, Judge, RealClock, Watchdog};
use crate::provider::config::load_models;
use crate::provider::openai::ClientCache;
use crate::provider::systemone::SystemOneClient;
use crate::session::service::{Lifecycle, Service, SessionId};

impl Service {
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
            SystemOneClient::from_alias(
                &models,
                "instinct",
                &self.config.config_dir,
                Arc::new(ClientCache::new()),
            )
            .ok()
        })
        .map(|client| Arc::new(JevJudge(client)) as Arc<dyn Judge>);
        let service = self.clone();
        tokio::spawn(async move {
            let clock: Arc<dyn Clock> = Arc::new(RealClock);
            let interval = Duration::from_secs(interval);
            let mut watchdog = Watchdog::new(clock.clone(), interval);
            loop {
                tokio::select! {
                    biased;
                    _ = handle.session_cancel.cancelled() => break,
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
                let _ = watchdog
                    .check(judge.as_deref(), &evidence, &handle.session_cancel)
                    .await;
            }
        });
    }
}
