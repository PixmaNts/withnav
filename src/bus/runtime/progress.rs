//! Observable child activity; deliberately excludes reasoning deltas.
use std::sync::{Arc, Mutex};
use tokio::time::Instant;

#[derive(Debug)]
pub struct Progress {
    pub last_message: Instant,
    pub last_tool: Instant,
    pub last_tool_call: Option<String>,
    pub last_tool_result: Option<String>,
    pub partial_output: String,
    pub steps: u64,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            last_message: Instant::now(),
            last_tool: Instant::now(),
            last_tool_call: None,
            last_tool_result: None,
            partial_output: String::new(),
            steps: 0,
        }
    }
}

pub struct Activity(
    pub Mutex<Progress>,
    Mutex<Arc<dyn Fn() -> Instant + Send + Sync>>,
);
impl std::fmt::Debug for Activity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Activity")
    }
}
impl Default for Activity {
    fn default() -> Self {
        Self(
            Mutex::new(Progress::default()),
            Mutex::new(Arc::new(Instant::now)),
        )
    }
}
impl Activity {
    pub fn install_clock(&self, clock: Arc<dyn Fn() -> Instant + Send + Sync>) {
        let now = clock();
        *self.1.lock().unwrap_or_else(|e| e.into_inner()) = clock;
        let mut progress = self.0.lock().unwrap_or_else(|e| e.into_inner());
        progress.last_message = now;
        progress.last_tool = now;
    }
    pub(super) fn observe(&self, event: &crate::agent::run_loop::AgentEvent<'_>) {
        use crate::agent::run_loop::AgentEvent;
        let now = (self.1.lock().unwrap_or_else(|e| e.into_inner()))();
        let mut progress = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match event {
            AgentEvent::StepStarted => {
                progress.steps = progress.steps.saturating_add(1);
                progress.partial_output.clear();
            }
            AgentEvent::Response(message) => {
                progress.last_message = now;
                progress.partial_output = crate::tools::bound_output(&message.text, 15 * 1024).0;
            }
            AgentEvent::Delta(crate::provider::StreamDelta::Text { text }) => {
                progress.last_message = now;
                let combined = format!("{}{text}", progress.partial_output);
                progress.partial_output = crate::tools::bound_output(&combined, 15 * 1024).0;
            }
            AgentEvent::PreCall { name, args, .. } => {
                progress.last_tool = now;
                progress.last_tool_call =
                    Some(crate::tools::bound_output(&format!("{name}: {args}"), 2048).0);
                progress.last_tool_result = None;
            }
            AgentEvent::PostCall { result, .. } => {
                progress.last_tool_result =
                    Some(crate::tools::bound_output(&result.output, 2048).0);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reasoning_is_not_progress_evidence() {
        let activity = Activity::default();
        activity.observe(&crate::agent::run_loop::AgentEvent::Delta(
            crate::provider::StreamDelta::Thinking {
                text: "private reasoning".into(),
            },
        ));
        let progress = activity.0.lock().unwrap();
        assert!(progress.partial_output.is_empty());
        assert!(progress.last_tool_call.is_none());
    }
}
