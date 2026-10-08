//! System One protocol, shared without changing its wire defaults.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One typed ask put to the model, answered as a yes-probability (`noul`),
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

/// The questions of one call, keyed by question name.
pub type Questions = BTreeMap<String, Question>;

/// The answers of one call, keyed by the same question names.
pub type Answers = BTreeMap<String, Answer>;

/// What a caller supplies for one System One call; the client adds the
/// model identifier it was built for.
#[derive(Debug, Clone, Copy)]
pub struct DecisionRequest<'a> {
    pub state: &'a str,
    pub questions: &'a Questions,
}

/// The parts of the System One response this crate reads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DecisionResponse {
    #[serde(default)]
    pub answers: Answers,
    #[serde(default)]
    pub usage: Option<DecisionUsage>,
}

/// Token usage as the System One fixture reports it.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct DecisionUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}
