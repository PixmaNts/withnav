use std::collections::VecDeque;
use std::sync::Mutex;
use ulid::Ulid;

use crate::provider::openai::{ChatMessage, UserProvenance};

#[derive(Debug, Default)]
pub(super) struct Steering {
    // Volatile parent guidance: crashes may lose it. Child results instead
    // remain in the durable completion outbox until transcript consumption.
    pub queue: Mutex<VecDeque<Operation>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_is_reserved_regardless_of_boundary_control_order() {
        for boundary_first in [true, false] {
            let steering = Steering::default();
            let id = Ulid::new();
            steering.queue.lock().unwrap().push_back(Operation {
                id,
                content: "replace".into(),
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
}

#[derive(Debug)]
pub(super) struct Operation {
    pub id: Ulid,
    pub content: String,
    pub interrupt: bool,
}

impl Steering {
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
                provenance: UserProvenance::Parent,
            });
        }
        *queue = kept;
        transcript
    }

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
            provenance: UserProvenance::Parent,
        });
        Some(transcript)
    }
}
