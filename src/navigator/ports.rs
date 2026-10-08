//! The model seams of the navigator's four layers, as traits the layer
//! implementations program against and tests script with fakes.
//!
//! - [`FimModel`]: the reflex's fill-in-the-middle engine (ghost-text
//!   suggestions while the driver types).
//! - [`Metacognition`]: the fast remark engine (one remark, or silence, for a
//!   document state).
//!
//! `Questions`/`Answers` are the System One wire types re-exported from the
//! provider (shape captured in `tests/fixtures/navigator/systemone_multi.json`).

use std::time::Duration;

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::error::ProviderError;
use crate::navigator::lsp::protocol::Remark;
use crate::navigator::metacognition::RemarkContext;

/// What can go wrong at a model seam: a provider failure (transport, API,
/// cancellation) or an answer that does not parse per the wire contract.
/// The two land in the log as different outcomes (`provider_error` vs
/// `malformed`), so they must stay distinguishable.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("provider failure: {0}")]
    Provider(ProviderError),
    #[error("malformed model output: {0}")]
    Malformed(String),
    /// The layer cannot run at all (it was disabled at startup); the message
    /// is shown to the driver as is.
    #[error("{0}")]
    Unavailable(String),
}

/// A provider response that is not the expected shape is `Malformed`; every
/// other provider failure stays a `Provider` error.
impl From<ProviderError> for ModelError {
    fn from(e: ProviderError) -> Self {
        match e {
            ProviderError::MalformedResponse(message) => ModelError::Malformed(message),
            other => ModelError::Provider(other),
        }
    }
}

// The System One wire types live with the provider client.
pub use crate::provider::systemone::{Answer, Answers, Question, Questions};

/// A fill-in-the-middle request from the reflex: the text before the cursor
/// (already prefixed with the rendered reflex context, capped) and the text
/// after it.
#[derive(Debug, Clone)]
pub struct FimRequest {
    pub prompt: String,
    pub suffix: String,
    pub max_tokens: u64,
}

/// A fill-in-the-middle answer: the suggested insertion, the provider's
/// token usage when reported, and the measured call latency.
#[derive(Debug, Clone)]
pub struct FimAnswer {
    pub text: String,
    pub usage: crate::provider::Usage,
    pub latency: Duration,
}

/// The reflex's model seam: completes the middle of a document. Stale
/// requests are dropped by the caller; implementations should honour the
/// cancellation token so a superseded call stops costing money.
pub trait FimModel: Send + Sync {
    fn complete<'a>(
        &'a self,
        request: FimRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<FimAnswer, ModelError>>;
}

/// What the metacognition sees when it rewrites the reflex context: the
/// intent summary, the active conversation's digest (both may be empty),
/// the active file, its language and a numbered excerpt around the cursor.
#[derive(Debug, Clone)]
pub struct ReflexContextRequest {
    pub intent: String,
    pub conversation: String,
    pub file: String,
    pub language_id: String,
    pub excerpt: String,
}

/// The metacognition's reflex-context seam: one plain chat call (no tools)
/// producing the short text placed in front of the code the reflex sees.
pub trait ReflexContextModel: Send + Sync {
    fn refresh<'a>(
        &'a self,
        request: &'a ReflexContextRequest,
    ) -> BoxFuture<'a, Result<String, ModelError>>;
}

/// The metacognition's remark seam: one remark (or silence) for a document
/// state. The flow's rendered remark prompt travels on
/// [`RemarkContext::prompt`]; without it the model would judge the diff
/// alone. `Ok(None)` is silence; an error is a provider failure or an answer
/// that violates the prompt contract ([`ModelError::Malformed`]).
pub trait Metacognition: Send + Sync {
    fn remark<'a>(
        &'a self,
        ctx: &'a RemarkContext,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Option<Remark>, ModelError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/navigator/systemone_multi.json");

    #[test]
    fn answers_round_trip_the_captured_multi_question_fixture() {
        let fixture: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        let original = fixture["response"]["answers"].clone();
        let answers: Answers = serde_json::from_value(original.clone()).unwrap();
        assert_eq!(serde_json::to_value(&answers).unwrap(), original);
    }

    #[test]
    fn questions_round_trip_the_captured_multi_question_fixture() {
        let fixture: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        let original = fixture["request"]["questions"].clone();
        let questions: Questions = serde_json::from_value(original.clone()).unwrap();
        assert_eq!(serde_json::to_value(&questions).unwrap(), original);
    }

    #[test]
    fn fixture_answers_expose_the_three_question_types() {
        let fixture: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        let answers: Answers =
            serde_json::from_value(fixture["response"]["answers"].clone()).unwrap();

        match answers.get("worth_attention").unwrap() {
            Answer::Noul { noul } => assert!((noul - 0.8).abs() < 1e-9),
            other => panic!("worth_attention must be noul, got {other:?}"),
        }
        match answers.get("route").unwrap() {
            Answer::Choice {
                choice,
                probabilities,
                confidence,
            } => {
                assert_eq!(choice, "cognition");
                assert_eq!(probabilities.len(), 3);
                assert_eq!(probabilities.get("cognition").copied(), Some(0.51));
                assert!((confidence - 0.27).abs() < 1e-9);
            }
            other => panic!("route must be choice, got {other:?}"),
        }
        match answers.get("severity").unwrap() {
            Answer::Score {
                score,
                legend,
                probabilities,
                confidence,
            } => {
                assert!((score - 1.56).abs() < 1e-9);
                assert_eq!(legend.get("3").map(String::as_str), Some("major"));
                assert_eq!(probabilities.len(), 4);
                assert_eq!(probabilities.get("1").copied(), Some(0.35));
                assert!((confidence - 0.16).abs() < 1e-9);
            }
            other => panic!("severity must be score, got {other:?}"),
        }
    }

    #[test]
    fn noul_answer_serializes_the_wire_shape() {
        let a = Answer::Noul { noul: 0.5 };
        assert_eq!(
            serde_json::to_value(&a).unwrap(),
            serde_json::json!({"type": "noul", "noul": 0.5})
        );
    }

    #[test]
    fn an_answer_without_a_type_marker_is_rejected() {
        assert!(serde_json::from_value::<Answer>(serde_json::json!({"noul": 0.5})).is_err());
    }
}
