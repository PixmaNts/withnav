//! HTTP client for the session server; no UI state or rendering.
//!
//! Every method returns a readable `String` error that carries the HTTP status
//! and the server's error body. Submitting a message is the one call whose
//! failure has two meanings, so it returns a [`SendError`]: the request was
//! refused, or it may have reached the server. Nothing here retries.
use super::protocol::*;
use crate::bus::event::ContextSnapshot;
use serde::de::DeserializeOwned;
use std::error::Error as _;
use std::time::Duration;

/// How long a short request (everything but the event stream) may take.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a new connection may take to establish.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest piece of a server error body that is kept in an error message.
const ERROR_BODY_CAP: usize = 500;

/// Why a message submission failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    /// The server never saw the request or answered with an error status: the
    /// message was not delivered and the draft can be sent again.
    #[error("{0}")]
    Rejected(String),
    /// The request may have reached the server (timeout, connection lost after
    /// the write, unreadable acknowledgement). The caller must not resend
    /// before checking the conversation.
    #[error("{0}")]
    Uncertain(String),
}

/// Why opening the event stream failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// Retrying cannot help (bad token, unknown session, rejected request).
    Fatal(String),
    /// The server is unreachable or failing; a later attempt may succeed.
    Transient(String),
}

#[derive(Clone)]
pub struct Client {
    pub base_url: String,
    pub token: Option<String>,
    http: reqwest::Client,
    request_timeout: Duration,
}

impl Client {
    pub fn new(base_url: impl Into<String>, token: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token,
            http,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }

    /// Overrides the time a short request may take before it fails.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    pub async fn models(&self, workspace: &str) -> Result<ModelsBody, String> {
        let request = self.get("/v1/models").query(&[("workspace", workspace)]);
        self.json(request).await
    }

    pub async fn sessions(&self, workspace: &str) -> Result<SessionListBody, String> {
        let request = self.get("/v1/sessions").query(&[("workspace", workspace)]);
        self.json(request).await
    }

    pub async fn create(
        &self,
        request: CreateSessionRequest,
    ) -> Result<CreateSessionResponse, String> {
        let request = self.post("/v1/sessions").json(&request);
        self.json(request).await
    }

    pub async fn resume(&self, workspace: &str, id: &str) -> Result<CreateSessionResponse, String> {
        self.create(CreateSessionRequest {
            workspace: workspace.to_string(),
            model: None,
            resume: Some(id.to_string()),
        })
        .await
    }

    pub async fn summary(&self, id: &str) -> Result<SessionMetadata, String> {
        let request = self.get(&format!("/v1/sessions/{}", encode_segment(id)));
        self.json(request).await
    }

    /// The session's context snapshot.
    pub async fn context(&self, id: &str) -> Result<ContextSnapshot, String> {
        let request = self.get(&format!("/v1/sessions/{}/context", encode_segment(id)));
        self.json(request).await
    }

    /// Submits a message. Never retried: the server assigns the command id, so
    /// a second attempt after an ambiguous failure could deliver it twice.
    pub async fn send(
        &self,
        id: &str,
        request: SubmitRequest,
    ) -> Result<SubmitResponse, SendError> {
        let path = format!("/v1/sessions/{}/messages", encode_segment(id));
        let response = self
            .post(&path)
            .json(&request)
            .send()
            .await
            .map_err(|e| classify_send_error(&e))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(SendError::Rejected(status_message(status, &body)));
        }
        response.json().await.map_err(|e| {
            SendError::Uncertain(format!(
                "the server accepted the message but its answer was unreadable: {}",
                describe(&e)
            ))
        })
    }

    /// The commands the server runs for a session.
    pub async fn commands(&self) -> Result<CommandsBody, String> {
        self.json(self.get("/v1/commands")).await
    }

    /// Runs a session command. A command that could not run is still `Ok`, with `ok: false`.
    pub async fn run_command(
        &self,
        id: &str,
        request: CommandRequest,
    ) -> Result<CommandResponse, String> {
        let path = format!("/v1/sessions/{}/commands", encode_segment(id));
        self.json(self.post(&path).json(&request)).await
    }

    pub async fn stop(&self, id: &str) -> Result<StopResponse, String> {
        let request = self.post(&format!("/v1/sessions/{}/stop", encode_segment(id)));
        self.json(request).await
    }

    /// Opens `GET /v1/sessions/{id}/events`, resuming after `cursor` when set.
    /// A 2xx answer is returned with its body unread.
    pub async fn open_events(
        &self,
        id: &str,
        cursor: Option<u64>,
    ) -> Result<reqwest::Response, OpenError> {
        let mut request = self
            .http
            .get(self.url(&format!("/v1/sessions/{}/events", encode_segment(id))))
            .header("accept", "text/event-stream");
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        if let Some(seq) = cursor {
            request = request.header("last-event-id", seq.to_string());
        }
        // No total timeout: the body is a long-lived stream. Only the wait for
        // the response head is bounded.
        let sent = tokio::time::timeout(self.request_timeout, request.send())
            .await
            .map_err(|_| OpenError::Transient("timed out opening the event stream".into()))?;
        let response = sent.map_err(|e| OpenError::Transient(describe(&e)))?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.text().await.unwrap_or_default();
        let message = status_message(status, &body);
        if status.is_server_error() || status == reqwest::StatusCode::REQUEST_TIMEOUT {
            Err(OpenError::Transient(message))
        } else {
            Err(OpenError::Fatal(message))
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.authorized(self.http.get(self.url(path)))
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.authorized(self.http.post(self.url(path)))
    }

    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let request = request.timeout(self.request_timeout);
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    async fn json<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, String> {
        let response = request.send().await.map_err(|e| describe(&e))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(status_message(status, &body));
        }
        response
            .json()
            .await
            .map_err(|e| format!("unreadable answer from the server: {}", describe(&e)))
    }
}

/// A transport failure of the submit request: `Rejected` only when the request
/// provably never left (could not be built, could not connect).
fn classify_send_error(error: &reqwest::Error) -> SendError {
    let message = describe(error);
    if error.is_builder() || error.is_connect() {
        SendError::Rejected(message)
    } else if error.is_timeout() {
        SendError::Uncertain(format!("no answer from the server in time: {message}"))
    } else {
        SendError::Uncertain(message)
    }
}

/// "HTTP 404 Not Found: session not found: X" from a status and its body.
fn status_message(status: reqwest::StatusCode, body: &str) -> String {
    let detail = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            ["error", "reason"]
                .iter()
                .find_map(|key| v.get(key).and_then(|f| f.as_str()).map(str::to_string))
        })
        .unwrap_or_else(|| body.trim().to_string());
    let detail: String = detail.chars().take(ERROR_BODY_CAP).collect();
    if detail.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {detail}")
    }
}

/// An error and its causes on one line ("error sending request: connection
/// refused"), with the request URL left out.
pub(crate) fn describe(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

/// Percent-encodes everything but unreserved characters so an id can sit in a
/// URL path segment.
fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;

    #[test]
    fn status_message_prefers_the_error_field() {
        let message = status_message(StatusCode::NOT_FOUND, r#"{"error":"session not found: X"}"#);
        assert_eq!(message, "HTTP 404 Not Found: session not found: X");
    }

    #[test]
    fn status_message_falls_back_to_reason_then_raw_body() {
        assert_eq!(
            status_message(
                StatusCode::SERVICE_UNAVAILABLE,
                r#"{"accepted":false,"reason":"inbox_full"}"#
            ),
            "HTTP 503 Service Unavailable: inbox_full"
        );
        assert_eq!(
            status_message(StatusCode::BAD_GATEWAY, " upstream down "),
            "HTTP 502 Bad Gateway: upstream down"
        );
        assert_eq!(
            status_message(StatusCode::UNAUTHORIZED, ""),
            "HTTP 401 Unauthorized"
        );
    }

    #[test]
    fn status_message_caps_a_long_body() {
        let body = "x".repeat(5000);
        let message = status_message(StatusCode::INTERNAL_SERVER_ERROR, &body);
        assert!(message.chars().count() < 600, "{}", message.len());
    }

    #[test]
    fn encode_segment_escapes_reserved_characters() {
        assert_eq!(encode_segment("01J-a_b.c~"), "01J-a_b.c~");
        assert_eq!(encode_segment("a/b c"), "a%2Fb%20c");
    }
}
