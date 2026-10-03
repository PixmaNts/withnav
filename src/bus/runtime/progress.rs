//! Observable child activity; deliberately excludes reasoning deltas.
use std::sync::Mutex;
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

#[derive(Debug, Default)]
pub struct Activity(pub Mutex<Progress>);
impl Activity {
    pub(super) fn observe(&self, event: &crate::agent::run_loop::AgentEvent<'_>) {
        use crate::agent::run_loop::AgentEvent;
        let mut progress = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match event {
            AgentEvent::StepStarted => progress.steps = progress.steps.saturating_add(1),
            AgentEvent::Response(message) => {
                progress.last_message = Instant::now();
                progress.partial_output = crate::tools::bound_output(&message.text, 15 * 1024).0;
            }
            AgentEvent::Delta(crate::provider::StreamDelta::Text { text }) => {
                progress.last_message = Instant::now();
                let combined = format!("{}{text}", progress.partial_output);
                progress.partial_output = crate::tools::bound_output(&combined, 15 * 1024).0;
            }
            AgentEvent::PreCall { name, args, .. } => {
                progress.last_tool = Instant::now();
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
