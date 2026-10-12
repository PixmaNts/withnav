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
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Part {
    pub source: String,
    pub ids: Vec<i64>,
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn small_window_drops_whole_optional_entries() {
        let component = Component {
            session: "session".into(),
            turn: "turn".into(),
            startup: Some(Part {
                source: "startup".into(),
                ids: vec![1],
                text: "x".repeat(1000),
            }),
            recalled: Arc::new(Mutex::new(vec![Part {
                source: "recall".into(),
                ids: vec![2],
                text: "short".into(),
            }])),
            record: Arc::new(|_| Box::pin(async { Ok(()) })),
        };
        assert_eq!(component.tokens_with_budget(0), 0);
        assert!(component.admitted_with_budget("model", 0).await.is_empty());
        let messages = component.admitted_with_budget("model", 20).await;
        assert_eq!(messages.len(), 1);
        assert!(serde_json::to_string(&messages).unwrap().contains("short"));
        assert!(!serde_json::to_string(&messages).unwrap().contains("xxxx"));
    }
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
        self.tokens_with_budget(u64::MAX)
    }
    pub fn tokens_with_budget(&self, budget: u64) -> u64 {
        self.fitting_parts(budget)
            .iter()
            .map(|p| {
                crate::agent::context::text_tokens(&p.text)
                    + crate::agent::context::MESSAGE_FRAMING_TOKENS
            })
            .sum()
    }
    fn fitting_parts(&self, mut budget: u64) -> Vec<Part> {
        self.parts()
            .into_iter()
            .flat_map(|part| {
                // Rendered bundles contain whole episodes; never truncate an entry.
                let episodes = part
                    .text
                    .split_once("<workspace_memory encoding=\"json\">")
                    .and_then(|(_, rest)| rest.strip_suffix("</workspace_memory>"))
                    .and_then(|json| serde_json::from_str::<Vec<super::Episode>>(json).ok());
                match episodes {
                    Some(episodes) => episodes
                        .into_iter()
                        .map(|episode| {
                            let (ids, text) = super::render(&[episode], usize::MAX);
                            Part {
                                source: part.source.clone(),
                                ids,
                                text,
                            }
                        })
                        .collect(),
                    None => vec![part],
                }
            })
            .filter(|part| {
                let tokens = crate::agent::context::text_tokens(&part.text)
                    + crate::agent::context::MESSAGE_FRAMING_TOKENS;
                if tokens > budget {
                    false
                } else {
                    budget -= tokens;
                    true
                }
            })
            .collect()
    }
    pub async fn admitted(&self, model: &str) -> Vec<crate::provider::openai::ChatMessage> {
        self.admitted_with_budget(model, u64::MAX).await
    }
    pub async fn admitted_with_budget(
        &self,
        model: &str,
        budget: u64,
    ) -> Vec<crate::provider::openai::ChatMessage> {
        use sha2::{Digest, Sha256};
        let request = ulid::Ulid::generate().to_string();
        let parts = self.fitting_parts(budget);
        if parts.is_empty() {
            return Vec::new();
        }
        let text = parts
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let hash = hex::encode(Sha256::digest(text.as_bytes()));
        let injection = Injection {
            request_id: request.clone(),
            session: self.session.clone(),
            turn: self.turn.clone(),
            step: request.clone(),
            model: model.into(),
            source: parts
                .iter()
                .map(|p| p.source.as_str())
                .collect::<Vec<_>>()
                .join("+"),
            episode_ids: parts.iter().flat_map(|p| p.ids.iter().copied()).collect(),
            text: text.clone(),
            hash,
        };
        if (self.record)(injection).await.is_ok() {
            vec![crate::provider::openai::ChatMessage::User {
                provenance: crate::provider::openai::UserProvenance::Subagent,
                content: text,
            }]
        } else {
            Vec::new()
        }
    }
}
