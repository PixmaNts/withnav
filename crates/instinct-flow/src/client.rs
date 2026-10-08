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
    #[error("provider rejected the request (HTTP {0})")]
    ProviderRejected(u16),
    #[error("provider error (HTTP {0})")]
    ProviderStatus(u16),
    #[error("invalid provider answer: {0}")]
    InvalidAnswer(String),
    #[error("response exceeds byte limit")]
    Limit,
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
                        DecisionError::ProviderRejected(status.as_u16())
                    } else {
                        DecisionError::ProviderStatus(status.as_u16())
                    });
                }
                if response.content_length().is_some_and(|n| n > 262144) {
                    return Err(DecisionError::Limit);
                }
                let mut bytes = Vec::new();
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .map_err(|_| DecisionError::ProviderError)?
                {
                    if bytes.len() + chunk.len() > 262144 {
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
