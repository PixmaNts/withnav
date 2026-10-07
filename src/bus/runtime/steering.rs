use std::collections::VecDeque;
use std::sync::Mutex;
use ulid::Ulid;

use crate::provider::openai::{ChatMessage, UserProvenance};

#[derive(Debug, Default)]
pub(super) struct Steering {
    // Volatile parent guidance: crashes may lose it. Child results instead
    // remain in the durable completion outbox until transcript consumption.
    pub queue: Mutex<VecDeque<Operation>>,
    terminal: std::sync::atomic::AtomicBool,
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
    fn interrupt_is_reserved_regardless_of_boundary_control_order() {
        for boundary_first in [true, false] {
            let steering = Steering::default();
            let id = Ulid::new();
            steering.queue.lock().unwrap().push_back(Operation {
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
    pub provenance: UserProvenance,
    pub id: Ulid,
    pub content: String,
    pub interrupt: bool,
}

impl Steering {
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
