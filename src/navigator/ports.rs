//! The model seams of the navigator's four layers, as traits the layer
//! implementations program against and tests script with fakes.
//!
//! - [`FimModel`]: the reflex's fill-in-the-middle engine (ghost-text
//!   suggestions while the driver types).
//! - [`DecisionModel`]: the instinct's typed-question engine (one call,
//!   several bounded questions, never free text).
//! - [`Commenter`]: the prototype's remark seam, kept as-is.
//!
//! The request/answer types here are the wire contract for the provider
//! implementations: `Questions`/`Answers` follow the System One shape
//! captured in `tests/fixtures/navigator/systemone_multi.json`.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::error::ProviderError;

/// What can go wrong at a model seam: a provider failure (transport, API,
/// cancellation) or an answer that does not parse per the wire contract.
/// The two land in the log as different outcomes (`provider_error` vs
/// `malformed`), so they must stay distinguishable.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("provider failure: {0}")]
    Provider(#[from] ProviderError),
    #[error("malformed model output: {0}")]
    Malformed(String),
}

/// One typed ask put to the instinct, answered as a yes-probability (`noul`),
/// a choice between named options, or a score over an ordered list of level
/// labels. This is the request half of the System One wire shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    /// A yes-probability question; the answer is a float in 0..1.
    Noul { instructions: String },
    /// A choice between named options; `criteria` maps each option to the
    /// instruction describing when it applies.
    Choice {
        instructions: String,
        criteria: BTreeMap<String, String>,
    },
    /// A score over an ordered list of 2 to 10 level labels (weakest first
    /// or strongest first, as the instructions define); the answer is an
    /// expected level index plus per-level probabilities.
    Score {
        instructions: String,
        criteria: Vec<String>,
    },
}

/// The answer to one typed question: the response half of the System One
/// wire shape. `probabilities` keys are the choice options, or the level
/// indices as strings for scores; `confidence` is the model's own
/// self-assessed certainty in 0..1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    Score {
        /// Expected level index (fractional, e.g. `1.56`).
        score: f64,
        /// Level index (as a string) to level label, as returned.
        legend: BTreeMap<String, String>,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
}

/// The questions of one instinct call, keyed by question name.
pub type Questions = BTreeMap<String, Question>;

/// The answers of one instinct call, keyed by the same question names.
pub type Answers = BTreeMap<String, Answer>;

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
    ) -> Pin<Box<dyn Future<Output = Result<FimAnswer, ModelError>> + Send + 'a>>;
}

/// The instinct's model seam: answers several typed questions about a
/// rendered state in one call. Never produces free text.
pub trait DecisionModel: Send + Sync {
    fn decide<'a>(
        &'a self,
        state: &'a str,
        questions: &'a Questions,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Answers, ModelError>> + Send + 'a>>;
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
