//! HTTP client for the session server; no UI state or rendering.
//!
//! Every method returns a readable `String` error that carries the HTTP status
//! and the server's error body. Submitting a message is the one call whose
//! failure has two meanings, so it returns a [`SendError`]: the request was
//! refused, or it may have reached the server. Nothing here retries.
use serde::de::DeserializeOwned;
use std::error::Error as _;
use std::time::Duration;
use withnav_client_api::{
    attachment::ImageRef,
    event::{McpStatus, PauseToken, SkillsBody},
    protocol::{
        AgentsBody, BypassResponse, CommandRequest, CommandResponse, CommandsBody, ContextSnapshot,
        CreateSessionRequest, CreateSessionResponse, LayersBody, ModelsBody, PermissionAnswer,
        SessionListBody, SessionMetadata, StopResponse, SubmitRequest, SubmitResponse,
    },
    settings::{GatewayTest, SettingsBody, SettingsError, WriteRequest},
    tools::AskAnswer,
};

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

/// Why a permission answer was not taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerError {
    /// The request is no longer waiting (answered, cancelled, expired).
    Stale,
    Other(String),
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
    pub async fn set_bypass(&self, id: &str, enabled: bool) -> Result<BypassResponse, String> {
        self.json(
            self.post(&format!("/v1/sessions/{}/bypass", encode_segment(id)))
                .json(&serde_json::json!({"enabled": enabled})),
        )
        .await
    }
    pub async fn settings(&self, workspace: &str) -> Result<SettingsBody, String> {
        self.json(self.get("/v1/settings").query(&[("workspace", workspace)]))
            .await
    }

    pub async fn write_setting(
        &self,
        workspace: &str,
        request: &WriteRequest,
    ) -> Result<SettingsBody, SettingsError> {
        use SettingsError;
        let response = self
            .authorized(self.http.put(format!("{}/v1/settings", self.base_url)))
            .query(&[("workspace", workspace)])
            .json(request)
            .send()
            .await
            .map_err(|e| SettingsError::Other(describe(&e)))?;
        let status = response.status();
        if status.is_success() {
            return response
                .json()
                .await
                .map_err(|e| SettingsError::Other(describe(&e)));
        }
        let text = response.text().await.unwrap_or_default();
        let body: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        if status == reqwest::StatusCode::CONFLICT && body["reason"] == "stale" {
            return Err(SettingsError::Stale);
        }
        if status == reqwest::StatusCode::BAD_REQUEST && body["reason"] == "invalid" {
            return Err(SettingsError::Invalid {
                key: body["key"].as_str().unwrap_or(&request.key).into(),
                message: body["error"].as_str().unwrap_or("invalid value").into(),
            });
        }
        Err(SettingsError::Other(status_message(status, &text)))
    }

    pub async fn test_gateway(&self, workspace: &str, name: &str) -> Result<GatewayTest, String> {
        self.json(
            self.post(&format!("/v1/gateways/{}/test", encode_segment(name)))
                .query(&[("workspace", workspace)]),
        )
        .await
    }
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
    pub async fn agents(
        &self,
        workspace: &str,
        session: Option<&str>,
    ) -> Result<AgentsBody, String> {
        let mut query = vec![("workspace", workspace)];
        if let Some(id) = session {
            query.push(("session_id", id));
        }
        self.json(self.get("/v1/agents").query(&query)).await
    }
    pub async fn layers(&self, workspace: &str) -> Result<LayersBody, String> {
        self.json(self.get("/v1/layers").query(&[("workspace", workspace)]))
            .await
    }

    /// The service's MCP servers; the `mcp.status` event carries the same body.
    pub async fn mcp(&self) -> Result<McpStatus, String> {
        self.json(self.get("/v1/mcp")).await
    }

    pub async fn skills(&self, session_id: &str) -> Result<SkillsBody, String> {
        self.json(self.get(&format!(
            "/v1/sessions/{}/skills",
            encode_segment(session_id)
        )))
        .await
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
            agent: None,
            variant: None,
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

    /// Uploads one image as raw bytes to the session's attachment store and returns its
    /// reference. Raw bytes (not JSON or base64) keep the body at the file's size; the server
    /// detects the type from the bytes and ignores the `Content-Type`. Never retried.
    pub async fn upload_attachment(
        &self,
        session_id: &str,
        name: &str,
        bytes: Vec<u8>,
    ) -> Result<ImageRef, String> {
        let path = format!("/v1/sessions/{}/attachments", encode_segment(session_id));
        let request = self
            .post(&path)
            .query(&[("name", name)])
            .header("content-type", "application/octet-stream")
            .body(bytes);
        self.json(request).await
    }

    /// The bytes of one stored image of the session.
    pub async fn attachment(
        &self,
        session_id: &str,
        attachment_id: &str,
    ) -> Result<Vec<u8>, String> {
        let path = format!(
            "/v1/sessions/{}/attachments/{}",
            encode_segment(session_id),
            encode_segment(attachment_id)
        );
        let response = self.get(&path).send().await.map_err(|e| describe(&e))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(status_message(status, &body));
        }
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| describe(&e))
    }

    /// Sends a structured question answer through the ordinary Captain-message ingress.
    pub async fn answer_question(
        &self,
        id: &str,
        answer: AskAnswer,
    ) -> Result<SubmitResponse, SendError> {
        self.send(
            id,
            SubmitRequest {
                target_actor_id: "agent:main".into(),
                mode: "next-turn".into(),
                content: vec![],
                answer: Some(answer),
            },
        )
        .await
    }

    /// Answers a permission request. A 409 `permission_stale` is [`AnswerError::Stale`].
    pub async fn answer_permission(
        &self,
        id: &str,
        request_id: &str,
        answer: PermissionAnswer,
    ) -> Result<(), AnswerError> {
        let path = format!(
            "/v1/sessions/{}/permissions/{}",
            encode_segment(id),
            encode_segment(request_id)
        );
        let response = self
            .post(&path)
            .json(&answer)
            .send()
            .await
            .map_err(|e| AnswerError::Other(describe(&e)))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        let stale = status == reqwest::StatusCode::CONFLICT
            && serde_json::from_str::<serde_json::Value>(&body)
                .is_ok_and(|v| v["reason"] == "permission_stale");
        if stale {
            Err(AnswerError::Stale)
        } else {
            Err(AnswerError::Other(status_message(status, &body)))
        }
    }

    /// Answers the secret request `request_id`, or gives a secret after it was settled
    /// (`submission.name` set). The value goes in this one request body and nowhere else.
    pub async fn submit_secret(
        &self,
        id: &str,
        request_id: &str,
        submission: &withnav_client_api::secrets::SecretSubmission,
    ) -> Result<withnav_client_api::secrets::SecretAccepted, String> {
        let path = format!(
            "/v1/sessions/{}/secrets/{}",
            encode_segment(id),
            encode_segment(request_id)
        );
        self.json(self.post(&path).json(submission)).await
    }

    /// The secrets a session sees: metadata only.
    pub async fn list_secrets(
        &self,
        id: &str,
    ) -> Result<withnav_client_api::secrets::SecretsBody, String> {
        self.json(self.get(&format!("/v1/sessions/{}/secrets", encode_segment(id))))
            .await
    }

    /// Revokes the secret `name`; `Ok(false)` when there was none.
    pub async fn revoke_secret(&self, id: &str, name: &str) -> Result<bool, String> {
        let response = self
            .authorized(self.http.delete(format!(
                "{}/v1/sessions/{}/secrets/{}",
                self.base_url,
                encode_segment(id),
                encode_segment(name)
            )))
            .send()
            .await
            .map_err(|e| describe(&e))?;
        let status = response.status();
        if status == reqwest::StatusCode::NO_CONTENT || status == reqwest::StatusCode::NOT_FOUND {
            return Ok(status == reqwest::StatusCode::NO_CONTENT);
        }
        let body = response.text().await.unwrap_or_default();
        Err(status_message(status, &body))
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

    pub async fn delete_session(&self, id: &str) -> Result<bool, String> {
        let response = self
            .authorized(self.http.delete(format!(
                "{}/v1/sessions/{}",
                self.base_url,
                encode_segment(id)
            )))
            .send()
            .await
            .map_err(|e| describe(&e))?;
        let status = response.status();
        if status == reqwest::StatusCode::NO_CONTENT || status == reqwest::StatusCode::NOT_FOUND {
            return Ok(status == reqwest::StatusCode::NO_CONTENT);
        }
        let body = response.text().await.unwrap_or_default();
        Err(status_message(status, &body))
    }

    pub async fn continue_child(&self, parent: &str, child: &str) -> Result<(), String> {
        self.continue_child_with_token(parent, child, None).await
    }
    pub async fn continue_child_with_token(
        &self,
        parent: &str,
        child: &str,
        token: Option<PauseToken>,
    ) -> Result<(), String> {
        let path = format!(
            "/v1/sessions/{}/children/{}/continue",
            encode_segment(parent),
            encode_segment(child)
        );
        let response: serde_json::Value = self
            .json(
                self.post(&path)
                    .json(&serde_json::json!({"pause_token":token})),
            )
            .await?;
        if response["accepted"].as_bool() == Some(true) {
            Ok(())
        } else {
            Err("the server did not accept continuing this subagent".into())
        }
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

    /// Serves `app` on an ephemeral loopback port and returns its base URL.
    async fn serve(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    /// One request as the test server saw it: method, path and query as sent (still
    /// percent-encoded), the `authorization` header and the body.
    /// No `Debug`: a captured secret submission must never be printed.
    struct Seen {
        method: String,
        uri: String,
        authorization: String,
        content_type: Option<String>,
        body: Vec<u8>,
    }

    /// A server that records every request and answers it with `respond(path)`.
    async fn recording_server(
        respond: fn(&str) -> axum::response::Response,
    ) -> (String, tokio::sync::mpsc::UnboundedReceiver<Seen>) {
        use axum::{
            body::Bytes,
            extract::OriginalUri,
            http::{HeaderMap, Method},
            routing::any,
        };
        let (seen, requests) = tokio::sync::mpsc::unbounded_channel();
        let app = axum::Router::new().fallback(any(
            move |method: Method, uri: OriginalUri, headers: HeaderMap, body: Bytes| {
                let seen = seen.clone();
                async move {
                    let header =
                        |name: &str| headers.get(name).map(|v| v.to_str().unwrap().to_owned());
                    let path = uri.0.path().to_owned();
                    seen.send(Seen {
                        method: method.to_string(),
                        uri: uri.0.to_string(),
                        authorization: header("authorization").unwrap_or_default(),
                        content_type: header("content-type"),
                        body: body.to_vec(),
                    })
                    .unwrap();
                    respond(&path)
                }
            },
        ));
        (serve(app).await, requests)
    }

    /// A client of the test server, with a short timeout so a broken route fails instead of hanging.
    fn local_client(url: String) -> Client {
        Client::new(url, Some("tok".into())).with_request_timeout(Duration::from_secs(5))
    }

    #[tokio::test]
    async fn session_scoped_calls_encode_the_id_in_the_path() {
        use axum::{Json, Router, extract::OriginalUri, routing::any};
        let (seen, mut paths) = tokio::sync::mpsc::unbounded_channel();
        let app = Router::new().fallback(any(move |uri: OriginalUri| {
            let seen = seen.clone();
            async move {
                seen.send(uri.0.path().to_owned()).unwrap();
                Json(serde_json::json!({"skills": [], "disabled_reason": null,
                    "enabled": true, "changed": false, "bypass_seq": 0}))
            }
        }));
        let client = Client::new(serve(app).await, None);

        client.skills("a/b c").await.unwrap();
        assert_eq!(
            paths.try_recv().expect("the request reached the server"),
            "/v1/sessions/a%2Fb%20c/skills"
        );
        client.set_bypass("a/b c", true).await.unwrap();
        assert_eq!(
            paths.try_recv().expect("the request reached the server"),
            "/v1/sessions/a%2Fb%20c/bypass"
        );
    }

    #[tokio::test]
    async fn upload_posts_raw_bytes_with_the_name_in_the_query() {
        use axum::{Json, http::StatusCode, response::IntoResponse};
        let (url, mut requests) = recording_server(|_| {
            Json(
                serde_json::json!({"attachment_id": "a1", "media_type": "image/png",
                "name": "my shot.png", "size": 3, "width": 1, "height": 1}),
            )
            .into_response()
        })
        .await;
        let client = local_client(url);

        let image = client
            .upload_attachment("s/1", "my shot.png", vec![1, 2, 3])
            .await
            .unwrap();
        assert_eq!(image.attachment_id, "a1");
        assert_eq!(image.size, 3);
        let seen = requests.try_recv().expect("the request reached the server");
        assert_eq!(seen.method, "POST");
        assert_eq!(seen.uri, "/v1/sessions/s%2F1/attachments?name=my+shot.png");
        assert_eq!(seen.authorization, "Bearer tok");
        // Raw bytes, never JSON or base64: the server sniffs the type from the bytes.
        assert_eq!(
            seen.content_type.as_deref(),
            Some("application/octet-stream")
        );
        assert_eq!(seen.body, vec![1, 2, 3]);

        let (url, _requests) = recording_server(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "empty image"})),
            )
                .into_response()
        })
        .await;
        let refused = local_client(url)
            .upload_attachment("s", "x.png", vec![])
            .await;
        assert_eq!(refused.unwrap_err(), "HTTP 400 Bad Request: empty image");
    }

    /// Not UTF-8 and holding a NUL: the download must not go through text decoding.
    const PNG_LIKE: [u8; 6] = [0x89, b'P', b'N', b'G', 0x00, 0xFF];

    #[tokio::test]
    async fn attachment_download_returns_the_raw_bytes() {
        use axum::{Json, http::StatusCode, response::IntoResponse};
        let (url, mut requests) = recording_server(|path| {
            if path.ends_with("/missing") {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "attachment not found"})),
                )
                    .into_response();
            }
            ([("content-type", "image/png")], PNG_LIKE.to_vec()).into_response()
        })
        .await;
        let client = local_client(url);

        let bytes = client.attachment("s/1", "a/b c").await.unwrap();
        assert_eq!(bytes, PNG_LIKE);
        let seen = requests.try_recv().expect("the request reached the server");
        assert_eq!(seen.method, "GET");
        assert_eq!(seen.uri, "/v1/sessions/s%2F1/attachments/a%2Fb%20c");
        assert_eq!(seen.authorization, "Bearer tok");

        let missing = client.attachment("s", "missing").await;
        assert_eq!(
            missing.unwrap_err(),
            "HTTP 404 Not Found: attachment not found"
        );
    }

    fn view(name: &str, expires_at: Option<&str>) -> withnav_client_api::secrets::SecretView {
        use withnav_client_api::secrets::{SecretScope, SecretTtl, SecretView};
        SecretView {
            name: name.into(),
            scope: if expires_at.is_some() {
                SecretScope::Workspace
            } else {
                SecretScope::Session
            },
            ttl: if expires_at.is_some() {
                SecretTtl::Hour
            } else {
                SecretTtl::Session
            },
            expires_at: expires_at.map(str::to_owned),
            created_at: "2026-11-09T10:00:00Z".into(),
            use_count: 2,
            last4: "wxyz".into(),
        }
    }

    #[tokio::test]
    async fn submit_secret_posts_the_submission_and_parses_the_answer() {
        use axum::{Json, response::IntoResponse};
        use withnav_client_api::secrets::{
            SecretAccepted, SecretScope, SecretSubmission, SecretTtl,
        };
        let (url, mut requests) = recording_server(|_| {
            Json(serde_json::json!({"accepted": true, "secret": {
                "name": "API_KEY", "scope": "workspace", "ttl": "1h",
                "expires_at": "2026-11-09T11:00:00Z", "created_at": "2026-11-09T10:00:00Z",
                "use_count": 2, "last4": "wxyz"}}))
            .into_response()
        })
        .await;
        let client = local_client(url);
        let value = "test-secret-abcdwxyz";

        let accepted = client
            .submit_secret(
                "s/1",
                "req/1",
                &SecretSubmission {
                    name: Some("API_KEY".into()),
                    value: Some(value.into()),
                    ttl: SecretTtl::Hour,
                    scope: SecretScope::Workspace,
                    decline: false,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            accepted,
            SecretAccepted {
                accepted: true,
                secret: Some(view("API_KEY", Some("2026-11-09T11:00:00Z"))),
            }
        );
        // The captured body is compared, never printed: a failing assertion must not show
        // the secret value, wherever a regression put it.
        let seen = requests.try_recv().expect("the request reached the server");
        assert_eq!(seen.method, "POST");
        assert_eq!(seen.uri, "/v1/sessions/s%2F1/secrets/req%2F1");
        assert_eq!(seen.authorization, "Bearer tok");
        assert_eq!(seen.content_type.as_deref(), Some("application/json"));
        let body: serde_json::Value =
            serde_json::from_slice(&seen.body).expect("the submission body is JSON");
        let expected = serde_json::json!({
            "name": "API_KEY", "value": value, "ttl": "1h", "scope": "workspace"});
        assert!(
            body == expected,
            "the submission body is not the expected JSON"
        );
    }

    #[tokio::test]
    async fn list_secrets_decodes_the_metadata() {
        use axum::{Json, response::IntoResponse};
        let (url, mut requests) = recording_server(|_| {
            Json(serde_json::json!({"secrets": [
                {"name": "API_KEY", "scope": "workspace", "ttl": "1h",
                 "expires_at": "2026-11-09T11:00:00Z", "created_at": "2026-11-09T10:00:00Z",
                 "use_count": 2, "last4": "wxyz"},
                {"name": "TOKEN", "scope": "session", "ttl": "session",
                 "created_at": "2026-11-09T10:00:00Z", "use_count": 2, "last4": "wxyz"}]}))
            .into_response()
        })
        .await;

        let body = local_client(url).list_secrets("s/1").await.unwrap();
        assert_eq!(
            body.secrets,
            [
                view("API_KEY", Some("2026-11-09T11:00:00Z")),
                view("TOKEN", None)
            ]
        );
        let seen = requests.try_recv().expect("the request reached the server");
        assert_eq!(seen.method, "GET");
        assert_eq!(seen.uri, "/v1/sessions/s%2F1/secrets");
        assert_eq!(seen.authorization, "Bearer tok");
    }

    #[tokio::test]
    async fn revoke_secret_maps_204_to_true_404_to_false_and_errors_otherwise() {
        use axum::{Json, http::StatusCode, response::IntoResponse};
        let (url, mut requests) = recording_server(|path| match path.rsplit('/').next() {
            Some("GONE") => StatusCode::NO_CONTENT.into_response(),
            Some("MISSING") => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "no such secret"})),
            )
                .into_response(),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "store failed"})),
            )
                .into_response(),
        })
        .await;
        let client = local_client(url);

        assert_eq!(client.revoke_secret("s/1", "GONE").await, Ok(true));
        let seen = requests.try_recv().expect("the request reached the server");
        assert_eq!(seen.method, "DELETE");
        assert_eq!(seen.uri, "/v1/sessions/s%2F1/secrets/GONE");
        assert_eq!(seen.authorization, "Bearer tok");

        assert_eq!(client.revoke_secret("s/1", "MISSING").await, Ok(false));
        assert_eq!(
            requests
                .try_recv()
                .expect("the request reached the server")
                .uri,
            "/v1/sessions/s%2F1/secrets/MISSING"
        );

        assert_eq!(
            client.revoke_secret("s/1", "BROKEN").await,
            Err("HTTP 500 Internal Server Error: store failed".into())
        );
        assert_eq!(
            requests
                .try_recv()
                .expect("the request reached the server")
                .uri,
            "/v1/sessions/s%2F1/secrets/BROKEN"
        );
    }
}
