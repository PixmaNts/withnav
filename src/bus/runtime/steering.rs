use std::collections::VecDeque;
use std::sync::Mutex;
use ulid::Ulid;

use crate::provider::openai::{ChatMessage, UserProvenance};

#[derive(Debug, Default)]
pub(super) struct Steering {
    // Volatile guidance and flow notes: crashes may lose queued input. Child results instead
    // remain in the durable completion outbox until transcript consumption.
    pub queue: Mutex<VecDeque<Operation>>,
    terminal: std::sync::atomic::AtomicBool,
    flow_wakes: Mutex<FlowWakes>,
}

#[derive(Debug, Default)]
struct FlowWakes {
    last: Option<std::time::Instant>,
    since_driver: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_note_preserves_non_driver_provenance_without_claiming_replacement() {
        let steering = Steering::default();
        let provenance = UserProvenance::Flow {
            source: crate::provider::openai::FlowSource::Flow,
            flow: "routing".into(),
            run_id: "capture".into(),
        };
        steering.admit(Operation {
            wake: false,
            id: Ulid::new(),
            content: "untrusted note".into(),
            interrupt: false,
            provenance: provenance.clone(),
        });
        let (messages, claimed) = steering.claim_replacements(vec![]);
        assert!(!claimed);
        assert!(messages.is_empty());
        let messages = steering.apply(messages);
        assert!(!messages[0].is_driver_message());
        let value = serde_json::to_value(&messages[0]).unwrap();
        assert_eq!(
            value["provenance"],
            serde_json::json!({"source":"flow","flow":"routing","run_id":"capture"})
        );
        let restored: ChatMessage = serde_json::from_value(value).unwrap();
        assert!(matches!(restored, ChatMessage::User { provenance: p, .. } if p == provenance));
    }

    #[test]
    fn notes_wait_for_another_turn_and_flow_wakes_are_coalesced_for_two_minutes() {
        let steering = Steering::default();
        assert!(steering.admit_flow("queued".into(), "review".into(), "a".into(), false));
        assert!(!steering.has_wake());
        assert_eq!(steering.apply(vec![]).len(), 1);
        assert!(steering.admit_flow("wake".into(), "review".into(), "b".into(), true));
        assert!(steering.has_wake());
        assert_eq!(steering.apply(vec![]).len(), 1);
        assert_eq!(
            steering.admit_flow_report(
                "loop".into(),
                "another-flow".into(),
                "c".into(),
                true,
                false
            ),
            Err("wake cooldown".into())
        );
        assert!(!steering.has_wake());
        assert!(steering.apply(vec![]).is_empty());
        steering.flow_wakes.lock().unwrap().last =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(120));
        assert!(steering.admit_flow("later".into(), "review".into(), "d".into(), true));
        assert!(steering.has_wake());
    }

    #[test]
    fn turn_end_cannot_wake_again_after_a_long_turn_until_driver_input() {
        let steering = Steering::default();
        assert_eq!(
            steering.admit_flow_report("wake".into(), "review".into(), "a".into(), true, true),
            Ok(())
        );
        assert_eq!(steering.apply(vec![]).len(), 1);
        steering.flow_wakes.lock().unwrap().last =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(121));
        assert_eq!(
            steering.admit_flow_report("loop".into(), "other".into(), "b".into(), true, true),
            Err("wake cooldown".into())
        );
        assert!(!steering.has_wake());
        steering.flow_wakes.lock().unwrap().since_driver = false;
        assert_eq!(
            steering.admit_flow_report("new task".into(), "other".into(), "c".into(), true, true),
            Ok(())
        );
        assert!(steering.has_wake());
    }

    #[test]
    fn interrupt_is_reserved_regardless_of_boundary_control_order() {
        for boundary_first in [true, false] {
            let steering = Steering::default();
            let id = Ulid::new();
            steering.queue.lock().unwrap().push_back(Operation {
                wake: true,
                id,
                content: "replace".into(),
                provenance: UserProvenance::Parent,
                interrupt: true,
            });
            let mut transcript = vec![];
            if boundary_first {
                transcript = steering.apply(transcript);
                assert!(transcript.is_empty());
            }
            transcript = steering.replacement(id, transcript).unwrap();
            transcript = steering.apply(transcript);
            assert_eq!(transcript.len(), 1);
            assert!(steering.replacement(id, transcript).is_none());
        }
    }

    #[test]
    fn replacements_claim_both_interrupts_in_order_and_obsolete_ids_have_no_effect() {
        let steering = Steering::default();
        let a = Ulid::new();
        let b = Ulid::new();
        for (id, text) in [(a, "first"), (b, "second")] {
            assert!(steering.admit(Operation {
                wake: true,
                id,
                content: text.into(),
                provenance: UserProvenance::Parent,
                interrupt: true
            }));
        }
        assert!(steering.apply(vec![]).is_empty());
        let (messages, claimed) = steering.claim_replacements(vec![]);
        assert!(claimed);
        assert!(matches!(&messages[0],ChatMessage::User {content,..} if content=="first"));
        assert!(matches!(&messages[1],ChatMessage::User {content,..} if content=="second"));
        assert!(!steering.has_interrupt(a));
        assert!(!steering.has_interrupt(b));
        let (again, claimed) = steering.claim_replacements(messages);
        assert!(!claimed);
        assert_eq!(again.len(), 2);
    }
}

#[derive(Debug)]
pub(super) struct Operation {
    pub wake: bool,
    pub provenance: UserProvenance,
    pub id: Ulid,
    pub content: String,
    pub interrupt: bool,
}

impl Steering {
    /// The steering instance belongs to one session, so all flows share its cooldown.
    pub fn admit_flow(&self, text: String, flow: String, run_id: String, wake: bool) -> bool {
        match self.admit_flow_report(text, flow, run_id, wake, false) {
            Ok(()) => true,
            Err(reason) => reason == "wake cooldown",
        }
    }

    fn admit_flow_report(
        &self,
        text: String,
        flow: String,
        run_id: String,
        wake: bool,
        turn_end: bool,
    ) -> Result<(), String> {
        let mut wakes = self.flow_wakes.lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        if wake
            && (wakes
                .last
                .is_some_and(|last| now.duration_since(last) < std::time::Duration::from_secs(120))
                || (turn_end && wakes.since_driver))
        {
            return Err("wake cooldown".into());
        }
        let admitted = self.admit(Operation {
            id: Ulid::new(),
            content: format!("<untrusted-flow-note>\n{text}\n</untrusted-flow-note>"),
            provenance: UserProvenance::Flow {
                source: crate::provider::openai::FlowSource::Flow,
                flow,
                run_id,
            },
            interrupt: false,
            wake,
        });
        if admitted && wake {
            wakes.last = Some(now);
            wakes.since_driver = true;
        }
        if admitted {
            Ok(())
        } else {
            Err("session stopped".into())
        }
    }

    pub fn has_wake(&self) -> bool {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|op| op.wake)
    }
    pub fn has_interrupt(&self, id: Ulid) -> bool {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|op| op.id == id && op.interrupt)
    }
    pub fn claim_replacements(&self, mut transcript: Vec<ChatMessage>) -> (Vec<ChatMessage>, bool) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let mut kept = VecDeque::new();
        let mut claimed = false;
        while let Some(op) = queue.pop_front() {
            if op.interrupt {
                claimed = true;
                transcript.push(ChatMessage::User {
                    content: op.content,
                    provenance: op.provenance,
                });
            } else {
                kept.push_back(op);
            }
        }
        *queue = kept;
        (transcript, claimed)
    }
    pub fn admit(&self, operation: Operation) -> bool {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if self.terminal.load(std::sync::atomic::Ordering::Acquire) {
            return false;
        }
        queue.push_back(operation);
        true
    }
    pub fn finish(&self) -> bool {
        let queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if !queue.is_empty() {
            return false;
        }
        self.terminal
            .store(true, std::sync::atomic::Ordering::Release);
        true
    }
    pub fn apply(&self, mut transcript: Vec<ChatMessage>) -> Vec<ChatMessage> {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let mut kept = VecDeque::new();
        while let Some(operation) = queue.pop_front() {
            if operation.interrupt {
                kept.push_back(operation);
                continue;
            }
            transcript.push(ChatMessage::User {
                content: operation.content,
                provenance: operation.provenance,
            });
        }
        *queue = kept;
        transcript
    }

    #[cfg(test)]
    pub fn replacement(
        &self,
        id: Ulid,
        mut transcript: Vec<ChatMessage>,
    ) -> Option<Vec<ChatMessage>> {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let index = queue.iter().position(|operation| operation.id == id)?;
        let operation = queue.remove(index)?;
        transcript.push(ChatMessage::User {
            content: operation.content,
            provenance: operation.provenance,
        });
        Some(transcript)
    }
}

impl super::RuntimeStatus {
    pub(crate) fn flow_driver_message(&self) {
        self.steering
            .flow_wakes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .since_driver = false;
    }

    pub(crate) fn queue_flow_note_report(
        &self,
        text: String,
        flow: String,
        run_id: String,
        wake: bool,
        turn_end: bool,
    ) -> Result<(), String> {
        self.steering
            .admit_flow_report(text, flow, run_id, wake, turn_end)?;
        if wake {
            self.controls
                .tx
                .send(super::Control::Steer)
                .map_err(|_| "session stopped".to_string())?;
        }
        Ok(())
    }
}
