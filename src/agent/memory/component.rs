//! Optional low-trust request component, admitted only after its audit write.
use futures::future::BoxFuture;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Injection {
    pub request_id: String,
    pub session: String,
    pub turn: String,
    pub step: String,
    pub model: String,
    pub source: String,
    pub episode_ids: Vec<i64>,
    pub text: String,
    pub hash: String,
}

pub type Recorder = Arc<dyn Fn(Injection) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;
#[derive(Clone)]
pub struct Part {
    pub source: String,
    pub ids: Vec<i64>,
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn request_memory_is_stable_low_trust_and_requires_audit() {
        let records = Arc::new(Mutex::new(Vec::new()));
        let saved = records.clone();
        let mut component = Component {
            session: "session".into(),
            turn: "turn".into(),
            startup: Some(Part {
                source: "startup".into(),
                ids: vec![1],
                text: "unverified history".into(),
            }),
            recalled: Arc::default(),
            record: Arc::new(move |i| {
                saved.lock().unwrap().push(i);
                Box::pin(async { Ok(()) })
            }),
        };
        let first = component.admitted("model").await;
        let second = component.admitted("model").await;
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            serde_json::to_value(&second).unwrap()
        );
        assert!(matches!(
            &first[0],
            crate::provider::openai::ChatMessage::User {
                provenance: crate::provider::openai::UserProvenance::Subagent,
                ..
            }
        ));
        assert_eq!(records.lock().unwrap().len(), 2);
        component.record = Arc::new(|_| Box::pin(async { Err("disk failed".into()) }));
        assert!(component.admitted("model").await.is_empty());
    }
}
#[derive(Clone)]
pub struct Component {
    pub turn: String,
    pub session: String,
    pub startup: Option<Part>,
    pub recalled: Arc<Mutex<Vec<Part>>>,
    pub record: Recorder,
}
impl Component {
    pub fn parts(&self) -> Vec<Part> {
        self.startup
            .iter()
            .cloned()
            .chain(
                self.recalled
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .cloned(),
            )
            .collect()
    }
    pub fn tokens(&self) -> u64 {
        self.parts()
            .iter()
            .map(|p| {
                crate::agent::context::text_tokens(&p.text)
                    + crate::agent::context::MESSAGE_FRAMING_TOKENS
            })
            .sum()
    }
    pub async fn admitted(&self, model: &str) -> Vec<crate::provider::openai::ChatMessage> {
        use sha2::{Digest, Sha256};
        let request = ulid::Ulid::new().to_string();
        let mut messages = Vec::new();
        for (index, part) in self.parts().into_iter().enumerate() {
            let hash = format!("{:x}", Sha256::digest(part.text.as_bytes()));
            let injection = Injection {
                request_id: format!("{request}:{index}"),
                session: self.session.clone(),
                turn: self.turn.clone(),
                step: request.clone(),
                model: model.into(),
                source: part.source,
                episode_ids: part.ids,
                text: part.text.clone(),
                hash,
            };
            if (self.record)(injection).await.is_ok() {
                messages.push(crate::provider::openai::ChatMessage::User {
                    provenance: crate::provider::openai::UserProvenance::Subagent,
                    content: part.text,
                });
            }
        }
        messages
    }
}
