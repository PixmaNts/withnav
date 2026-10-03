use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::provider::openai::{ChatMessage, UserProvenance};

#[derive(Debug, Default)]
pub(super) struct Steering {
    pub queue: Mutex<VecDeque<String>>,
}

impl Steering {
    pub fn apply(&self, mut transcript: Vec<ChatMessage>) -> Vec<ChatMessage> {
        for content in self
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            transcript.push(ChatMessage::User {
                content,
                provenance: UserProvenance::Parent,
            });
        }
        transcript
    }
    pub fn boundary(self: &Arc<Self>) -> crate::agent::boundary::SafeBoundary {
        let steering = self.clone();
        Arc::new(move |transcript| {
            let steering = steering.clone();
            Box::pin(async move { Ok(steering.apply(transcript)) })
        })
    }
}
