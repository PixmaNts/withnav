//! Explicit System One transport with bounded admission and secret-free diagnostics.
use crate::{protocol::*, BoxFuture};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub images: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientIdentity {
    pub model: String,
    pub variant: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecisionError {
    #[error("decision deadline exhausted")]
    Timeout,
    #[error("decision cancelled")]
    Cancelled,
    #[error("provider unreachable")]
    ProviderError,
    /// The provider refused the request (a 4xx). `reason` is the provider's
    /// own explanation, bounded by [`reason_excerpt`]; `None` when the body
    /// gave none or echoed a credential.
    #[error("provider rejected the request (HTTP {status}){}", reason_suffix(.reason))]
    ProviderRejected { status: u16, reason: Option<String> },
    #[error("provider error (HTTP {0})")]
    ProviderStatus(u16),
    #[error("invalid provider answer: {0}")]
    InvalidAnswer(String),
    #[error("response exceeds byte limit")]
    Limit,
}
fn reason_suffix(reason: &Option<String>) -> String {
    reason
        .as_deref()
        .map(|reason| format!(": {reason}"))
        .unwrap_or_default()
}

/// Longest provider reason kept on an error, in characters.
pub const REASON_MAX_CHARS: usize = 300;

/// A provider's error text made safe to show on one diagnostic line: every
/// whitespace run becomes one space, other control characters are dropped
/// (an escape sequence cannot restyle a terminal), and the result is cut at
/// [`REASON_MAX_CHARS`] with a trailing `…`. `None` when nothing is left.
pub fn reason_excerpt(text: &str) -> Option<String> {
    let mut line = String::new();
    for c in text.chars() {
        if c.is_whitespace() {
            if !line.is_empty() && !line.ends_with(' ') {
                line.push(' ');
            }
        } else if !c.is_control() {
            line.push(c);
        }
    }
    let line = line.trim_end();
    if line.is_empty() {
        return None;
    }
    let mut chars = line.chars();
    let mut excerpt: String = chars.by_ref().take(REASON_MAX_CHARS).collect();
    if chars.next().is_some() {
        excerpt.push('…');
    }
    Some(excerpt)
}

/// Error bodies larger than this are read only this far: the reason is
/// near the start, and a hostile endpoint cannot make a rejection costly.
const REJECTION_BODY_BYTES: usize = 4 * 1024;

/// The provider's explanation of a rejection: `error.message`, `error` or
/// `message` of a JSON body, else the body text. Dropped entirely when it
/// contains a credential this client sent, since a provider echoing the
/// request back must never put a key into a run record or a log.
fn rejection_reason(body: &[u8], credentials: &[&str]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let json: Option<serde_json::Value> = serde_json::from_str(&text).ok();
    let message = json.as_ref().and_then(|value| {
        let error = value.get("error");
        error
            .and_then(|error| error.get("message"))
            .or(error)
            .or_else(|| value.get("message"))
            .and_then(serde_json::Value::as_str)
    });
    let reason = reason_excerpt(message.unwrap_or(&text))?;
    let echoed = credentials
        .iter()
        .any(|secret| !secret.is_empty() && (text.contains(secret) || reason.contains(secret)));
    (!echoed).then_some(reason)
}

#[derive(Debug, thiserror::Error)]
#[error("invalid System One client configuration")]
pub struct ClientError;
impl DecisionError {
    pub fn is_outage(&self) -> bool {
        matches!(
            self,
            Self::Timeout | Self::ProviderError | Self::ProviderStatus(_)
        )
    }
}
pub trait DecisionClient: Send + Sync {
    fn capabilities(&self) -> Capabilities;
    fn identity(&self) -> ClientIdentity;
    fn decide<'a>(
        &'a self,
        request: DecisionRequest<'a>,
        cancel: &'a CancellationToken,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<DecisionResponse, DecisionError>>;
}
#[derive(Clone)]
pub struct SystemOneClient {
    endpoint: url::Url,
    model: String,
    client: reqwest::Client,
    key: String,
    deadline: Duration,
    headers: reqwest::header::HeaderMap,
    /// The configured header values, kept only to recognize a rejection body
    /// that echoes one (see [`rejection_reason`]).
    header_values: Vec<String>,
}
impl std::fmt::Debug for SystemOneClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemOneClient")
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}
impl SystemOneClient {
    pub fn new(
        endpoint: url::Url,
        model: String,
        key: String,
        deadline: Duration,
        headers: BTreeMap<String, String>,
    ) -> Result<Self, ClientError> {
        if !matches!(endpoint.scheme(), "http" | "https")
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || deadline.is_zero()
        {
            return Err(ClientError);
        }
        let mut map = reqwest::header::HeaderMap::new();
        let header_values = headers.values().cloned().collect();
        for (name, value) in headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| ClientError)?;
            let mut value =
                reqwest::header::HeaderValue::from_str(&value).map_err(|_| ClientError)?;
            value.set_sensitive(true);
            map.insert(name, value);
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| ClientError)?;
        Ok(Self {
            endpoint,
            model,
            client,
            key,
            deadline: deadline.min(Duration::from_secs(2)),
            headers: map,
            header_values,
        })
    }
}
impl DecisionClient for SystemOneClient {
    fn capabilities(&self) -> Capabilities {
        Capabilities { images: false }
    }
    fn identity(&self) -> ClientIdentity {
        ClientIdentity {
            model: self.model.clone(),
            variant: None,
        }
    }
    fn decide<'a>(
        &'a self,
        request: DecisionRequest<'a>,
        cancel: &'a CancellationToken,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<DecisionResponse, DecisionError>> {
        Box::pin(async move {
            let deadline = deadline.min(Instant::now() + self.deadline);
            let work = async {
                let mut response=self.client.post(self.endpoint.clone()).headers(self.headers.clone()).bearer_auth(&self.key)
                    .json(&serde_json::json!({"model":self.model,"state":request.state,"questions":request.questions}))
                    .send().await.map_err(|_| DecisionError::ProviderError)?;
                if !response.status().is_success() {
                    let status = response.status();
                    return Err(if status.is_client_error() {
                        let mut body = Vec::new();
                        while body.len() < REJECTION_BODY_BYTES {
                            match response.chunk().await {
                                Ok(Some(chunk)) => {
                                    let room = REJECTION_BODY_BYTES - body.len();
                                    body.extend_from_slice(&chunk[..chunk.len().min(room)]);
                                }
                                Ok(None) | Err(_) => break,
                            }
                        }
                        let credentials: Vec<&str> = std::iter::once(self.key.as_str())
                            .chain(self.header_values.iter().map(String::as_str))
                            .collect();
                        DecisionError::ProviderRejected {
                            status: status.as_u16(),
                            reason: rejection_reason(&body, &credentials),
                        }
                    } else {
                        DecisionError::ProviderStatus(status.as_u16())
                    });
                }
                if response
                    .content_length()
                    .is_some_and(|n| n > crate::MAX_RESPONSE_BYTES as u64)
                {
                    return Err(DecisionError::Limit);
                }
                let mut bytes = Vec::new();
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .map_err(|_| DecisionError::ProviderError)?
                {
                    if chunk.len() > crate::MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) {
                        return Err(DecisionError::Limit);
                    }
                    bytes.extend_from_slice(&chunk);
                }
                let raw: crate::admission::UniqueValue =
                    serde_json::from_slice(&bytes).map_err(|_| {
                        DecisionError::InvalidAnswer(
                            "malformed or duplicate response fields".into(),
                        )
                    })?;
                let response: DecisionResponse = serde_json::from_value(raw.0)
                    .map_err(|_| DecisionError::InvalidAnswer("malformed response".into()))?;
                validate_answers(request.questions, &response.answers)?;
                Ok(response)
            };
            let result = tokio::select! { biased;
                _=cancel.cancelled()=>Err(DecisionError::Cancelled),
                _=tokio::time::sleep_until(deadline.into())=>Err(DecisionError::Timeout),
                result=work=>result,
            };
            if cancel.is_cancelled() {
                Err(DecisionError::Cancelled)
            } else if Instant::now() >= deadline {
                Err(DecisionError::Timeout)
            } else {
                result
            }
        })
    }
}
fn unit(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}
pub fn validate_answers(questions: &Questions, answers: &Answers) -> Result<(), DecisionError> {
    let invalid = |message: &str| DecisionError::InvalidAnswer(message.into());
    if !questions.keys().eq(answers.keys()) {
        return Err(invalid("answer keys differ from request"));
    }
    fn probabilities(
        expected: impl Iterator<Item = String>,
        values: &BTreeMap<String, f64>,
    ) -> bool {
        expected.eq(values.keys().cloned())
            && values.values().all(|v| unit(*v))
            && (values.values().sum::<f64>() - 1.0).abs() <= 0.01 + f64::EPSILON
    }
    for (id, question) in questions {
        let valid = match (question, &answers[id]) {
            (Question::Noul { .. }, Answer::Noul { noul }) => unit(*noul),
            (
                Question::Choice { criteria, .. },
                Answer::Choice {
                    choice,
                    probabilities: probs,
                    confidence,
                },
            ) => {
                criteria.contains_key(choice)
                    && unit(*confidence)
                    && probabilities(criteria.keys().cloned(), probs)
            }
            (
                Question::Score { criteria, .. },
                Answer::Score {
                    score,
                    legend,
                    probabilities: probs,
                    confidence,
                },
            ) => {
                let expected: BTreeMap<_, _> = criteria
                    .iter()
                    .enumerate()
                    .map(|(i, s)| (i.to_string(), s.clone()))
                    .collect();
                criteria.len() >= 2
                    && score.is_finite()
                    && (0.0..=(criteria.len() - 1) as f64).contains(score)
                    && *legend == expected
                    && unit(*confidence)
                    && probabilities(expected.keys().cloned(), probs)
            }
            _ => false,
        };
        if !valid {
            return Err(invalid(
                "answer type, range, probabilities or legend differs from request",
            ));
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn answer_validation_is_request_relative() {
        let q = Questions::from([(
            "x".into(),
            Question::Choice {
                instructions: "pick".into(),
                criteria: BTreeMap::from([("a".into(), "A".into()), ("b".into(), "B".into())]),
            },
        )]);
        let answer = |choice: &str, p: f64| {
            Answers::from([(
                "x".into(),
                Answer::Choice {
                    choice: choice.into(),
                    confidence: 0.9,
                    probabilities: BTreeMap::from([("a".into(), p), ("b".into(), 0.5)]),
                },
            )])
        };
        assert!(validate_answers(&q, &answer("a", 0.5)).is_ok());
        assert!(validate_answers(&q, &answer("c", 0.5)).is_err());
        assert!(validate_answers(&q, &answer("a", 0.7)).is_err());
        assert!(validate_answers(&q, &answer("a", f64::NAN)).is_err());
        assert!(validate_answers(&q, &Answers::new()).is_err());
    }
    fn noul_questions() -> Questions {
        Questions::from([(
            "go".into(),
            Question::Noul {
                instructions: "Go?".into(),
            },
        )])
    }

    async fn rejection(body: wiremock::ResponseTemplate) -> DecisionError {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(body)
            .mount(&server)
            .await;
        let client = SystemOneClient::new(
            format!("{}/systemone", server.uri()).parse().unwrap(),
            "model".into(),
            "sk-key-value".into(),
            Duration::from_secs(2),
            BTreeMap::from([("x-token".into(), "header-value".into())]),
        )
        .unwrap();
        let questions = noul_questions();
        client
            .decide(
                DecisionRequest {
                    state: "s",
                    questions: &questions,
                },
                &CancellationToken::new(),
                Instant::now() + Duration::from_secs(1),
            )
            .await
            .unwrap_err()
    }

    // A rejected decision used to say only "HTTP 400": every Instinct call
    // failed that way for days (observed 2026-11-23..25) and neither the run
    // record nor the log said why. The provider's own reason must travel
    // with the error, bounded and on one line.
    #[tokio::test]
    async fn a_rejection_carries_the_providers_reason() {
        let json = wiremock::ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {"message": "model jev-router does not support\nthe systemone API", "code": 400}
        }));
        let error = rejection(json).await;
        assert_eq!(
            error.to_string(),
            "provider rejected the request (HTTP 400): model jev-router does not support the systemone API"
        );
        assert!(!error.is_outage());

        let text = wiremock::ResponseTemplate::new(422)
            .set_body_string(format!("bad\u{1b}[31m state\t{}", "x".repeat(500)));
        let message = rejection(text).await.to_string();
        let reason = message
            .strip_prefix("provider rejected the request (HTTP 422): ")
            .expect(&message);
        assert!(reason.starts_with("bad[31m state x"), "{reason}");
        assert_eq!(reason.chars().count(), 301, "300 chars and the cut marker");
        assert!(reason.ends_with('…'));
        assert!(!reason.chars().any(char::is_control));

        // A body echoing a credential is not shown at all.
        for echoed in ["sk-key-value", "invalid header header-value"] {
            let echo = wiremock::ResponseTemplate::new(401).set_body_string(echoed);
            assert_eq!(
                rejection(echo).await.to_string(),
                "provider rejected the request (HTTP 401)"
            );
        }
        let empty = wiremock::ResponseTemplate::new(400);
        assert_eq!(
            rejection(empty).await.to_string(),
            "provider rejected the request (HTTP 400)"
        );
    }

    #[test]
    fn secret_values_do_not_escape_debug_or_errors() {
        let client = SystemOneClient::new(
            "http://localhost/systemone".parse().unwrap(),
            "model".into(),
            "secret".into(),
            Duration::from_secs(1),
            BTreeMap::from([("x-token".into(), "header-secret".into())]),
        )
        .unwrap();
        assert!(!format!("{client:?}").contains("secret"));
    }
}
