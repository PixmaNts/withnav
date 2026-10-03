use std::collections::VecDeque;
use std::sync::Mutex;

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
}
