//! Scripted chat-completion streams (SSE bodies, wiremock responses, TCP
//! providers that record the requests they get) and a reader for the
//! session server's `GET /events` stream.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::ResponseTemplate;

/// A chat-completion SSE body: one `data:` frame per event, then `[DONE]`.
pub fn sse_body(events: &[&str]) -> String {
    let mut b = String::new();
    for e in events {
        b.push_str("data: ");
        b.push_str(e);
        b.push_str("\n\n");
    }
    b.push_str("data: [DONE]\n\n");
    b
}

/// A streamed assistant turn that calls tool `name` with the JSON `args`.
pub fn tool_call_chunk(id: &str, name: &str, args: &Value) -> String {
    serde_json::json!({"choices": [{"index": 0, "finish_reason": "tool_calls", "delta": {
        "tool_calls": [{"index": 0, "id": id, "function": {
            "name": name, "arguments": args.to_string(),
        }}],
    }}]})
    .to_string()
}

/// A streamed assistant turn that answers with `text` and stops.
pub fn text_chunk(text: &str) -> String {
    serde_json::json!({"choices": [{"index": 0, "finish_reason": "stop", "delta": {"content": text}}]})
        .to_string()
}

/// The raw HTTP/1.1 response a scripted TCP provider writes for `body`.
pub fn http_sse(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes()
}

/// The wiremock answer carrying [`sse_body`] for `events`.
pub fn sse_response(events: &[&str]) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(sse_body(events), "text/event-stream")
}

/// A scripted TCP provider; `url` is the `base_url` to put in `models.json`.
pub struct MockProvider {
    pub url: String,
}

/// Serves `responses` one per accepted connection, in order, then stops
/// accepting. Each response is written after reading one request chunk.
pub async fn spawn_mock_provider(responses: Vec<Vec<u8>>) -> MockProvider {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock bind");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        for resp in responses {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let mut buf = vec![0u8; 16384];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(&resp).await;
            let _ = sock.flush().await;
            let _ = sock.shutdown().await;
        }
    });
    MockProvider {
        url: format!("http://{addr}/v1"),
    }
}

/// A scripted TCP provider that keeps every raw request it received.
pub struct CapturingProvider {
    pub url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl CapturingProvider {
    /// The raw HTTP requests (head and body), in arrival order.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests").clone()
    }

    /// The request bodies, in arrival order.
    pub fn bodies(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|raw| {
                let idx = raw.find("\r\n\r\n").expect("HTTP header/body separator");
                raw[idx + 4..].to_string()
            })
            .collect()
    }
}

/// Serves `responses` one per complete request, in order. Incomplete
/// cancelled connections consume neither a response nor a recorded index.
pub async fn spawn_capturing_provider(responses: Vec<Vec<u8>>) -> CapturingProvider {
    let count = responses.len();
    spawn_responding_provider(count, move |i, _| responses[i].clone()).await
}

/// Like [`spawn_capturing_provider`], but `respond(index, request)` picks the
/// answer to the `index`th request, so a mock can reject a bad request.
pub async fn spawn_responding_provider(
    count: usize,
    respond: impl Fn(usize, &str) -> Vec<u8> + Send + 'static,
) -> CapturingProvider {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock bind");
    let addr = listener.local_addr().expect("mock addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    tokio::spawn(async move {
        let mut i = 0;
        while i < count {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let Some(request) = read_http_request(&mut sock).await else {
                continue;
            };
            let response = respond(i, &request);
            i += 1;
            captured.lock().expect("requests").push(request);
            let _ = sock.write_all(&response).await;
            let _ = sock.flush().await;
            let _ = sock.shutdown().await;
        }
    });
    CapturingProvider {
        url: format!("http://{addr}/v1"),
        requests,
    }
}

/// Like [`spawn_capturing_provider`], but the first answer is held back
/// until `release` is notified, so a test can act while a turn is running.
pub async fn spawn_gated_provider(
    responses: Vec<Vec<u8>>,
    release: Arc<tokio::sync::Notify>,
) -> CapturingProvider {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock bind");
    let addr = listener.local_addr().expect("mock addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    tokio::spawn(async move {
        for (i, response) in responses.into_iter().enumerate() {
            let (mut sock, request) = loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                if let Some(request) = read_http_request(&mut sock).await {
                    break (sock, request);
                }
            };
            captured.lock().expect("requests").push(request);
            if i == 0 {
                release.notified().await;
            }
            let _ = sock.write_all(&response).await;
            let _ = sock.flush().await;
            let _ = sock.shutdown().await;
        }
    });
    CapturingProvider {
        url: format!("http://{addr}/v1"),
        requests,
    }
}

/// Like [`spawn_responding_provider`], but the answer to request `hold` is
/// held back until `release` is notified, so a test can act while that
/// request (a summary, say) is in flight. `reached` is notified once the
/// held request has arrived.
pub async fn spawn_holding_provider(
    count: usize,
    hold: usize,
    reached: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    respond: impl Fn(usize, &str) -> Vec<u8> + Send + 'static,
) -> CapturingProvider {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock bind");
    let addr = listener.local_addr().expect("mock addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    tokio::spawn(async move {
        let mut i = 0;
        while i < count {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let Some(request) = read_http_request(&mut sock).await else {
                continue;
            };
            let response = respond(i, &request);
            i += 1;
            captured.lock().expect("requests").push(request);
            if i - 1 == hold {
                reached.notify_one();
                release.notified().await;
            }
            let _ = sock.write_all(&response).await;
            let _ = sock.flush().await;
            let _ = sock.shutdown().await;
        }
    });
    CapturingProvider {
        url: format!("http://{addr}/v1"),
        requests,
    }
}

/// A provider whose answers the test writes piece by piece: every complete request comes out
/// of `requests` as its body and a sender; each string sent is written as one SSE `data:` frame,
/// and dropping the sender ends the answer with `[DONE]`.
/// A complete request remains counted even if its response is later cancelled.
pub struct StreamingProvider {
    pub url: String,
    pub requests: tokio::sync::mpsc::Receiver<(String, tokio::sync::mpsc::UnboundedSender<String>)>,
}

pub async fn spawn_streaming_provider() -> StreamingProvider {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock bind");
    let addr = listener.local_addr().expect("mock addr");
    let (tx, requests) = tokio::sync::mpsc::channel(16);
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let Some(request) = read_http_request(&mut sock).await else {
                    return;
                };
                let (chunks, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                if tx.send((request, chunks)).await.is_err() {
                    return;
                }
                let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
                if sock.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                while let Some(chunk) = rx.recv().await {
                    let frame = format!("data: {chunk}\n\n");
                    if sock.write_all(frame.as_bytes()).await.is_err() {
                        return;
                    }
                    let _ = sock.flush().await;
                }
                let _ = sock.write_all(b"data: [DONE]\n\n").await;
                let _ = sock.flush().await;
                let _ = sock.shutdown().await;
            });
        }
    });
    StreamingProvider {
        url: format!("http://{addr}/v1"),
        requests,
    }
}

/// Reads a complete Content-Length request. Cancelled clients can close a
/// connection before writing its head or body; those are not requests and
/// must not consume a scripted response index or enter captured history.
async fn read_http_request(sock: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        let Some(idx) = buf.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let head = std::str::from_utf8(&buf[..idx]).ok()?;
        let content_len = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>())
            })
            .transpose()
            .ok()?
            .unwrap_or(0);
        let end = idx.checked_add(4)?.checked_add(content_len)?;
        if buf.len() >= end {
            return String::from_utf8(buf[..end].to_vec()).ok();
        }
    }
}

/// One parsed SSE frame: its `id:` (if any), `event:` name (default
/// "message") and the concatenated `data:` payload.
#[derive(Debug, Clone)]
pub struct SseFrame {
    pub id: Option<u64>,
    pub event: String,
    pub data: String,
}

impl SseFrame {
    /// The envelope `kind`, for a frame that carries a durable envelope.
    pub fn kind(&self) -> Option<String> {
        serde_json::from_str::<Value>(&self.data)
            .ok()
            .and_then(|v| v["kind"].as_str().map(str::to_string))
    }

    /// The envelope `seq`, for a frame that carries a durable envelope.
    pub fn seq(&self) -> Option<u64> {
        serde_json::from_str::<Value>(&self.data).ok()?["seq"].as_u64()
    }
}

/// How an SSE read stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum SseEnd {
    /// The server closed the stream cleanly (body EOF).
    Eof,
    /// The `until` predicate was satisfied while the stream was still open.
    Until,
    /// [`SSE_CEILING`] elapsed first: a failure, never an expected end.
    Timeout,
    /// The transport errored mid-read.
    Error,
}

pub struct SseRead {
    pub frames: Vec<SseFrame>,
    pub end: SseEnd,
}

/// The longest a read waits for its expected head or EOF.
pub const SSE_CEILING: Duration = Duration::from_secs(10);

/// Connects to `GET /v1/sessions/{id}/events` (with `Last-Event-ID` when
/// given) and parses frames until `until` holds for the frames so far, the
/// server ends the stream, or [`SSE_CEILING`] passes. Pass `|_| false` to
/// read to EOF.
pub async fn read_sse(
    base: &str,
    id: &str,
    last_event_id: Option<u64>,
    until: impl Fn(&[SseFrame]) -> bool,
) -> SseRead {
    let mut req = reqwest::Client::new().get(format!("{base}/v1/sessions/{id}/events"));
    if let Some(k) = last_event_id {
        req = req.header("last-event-id", k.to_string());
    }
    let mut resp = req.send().await.expect("sse connect");
    assert_eq!(resp.status(), 200, "sse status");
    let mut buf = String::new();
    let mut frames = Vec::new();
    let deadline = tokio::time::Instant::now() + SSE_CEILING;
    let end = 'read: loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, resp.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                buf.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(idx) = buf.find("\n\n") {
                    let raw: String = buf.drain(..idx + 2).collect();
                    frames.extend(parse_frame(&raw));
                    if until(&frames) {
                        break 'read SseEnd::Until;
                    }
                }
            }
            Ok(Ok(None)) => break SseEnd::Eof,
            Ok(Err(_)) => break SseEnd::Error,
            Err(_) => break SseEnd::Timeout,
        }
    };
    SseRead { frames, end }
}

/// Whether some frame carries an envelope of this `kind`.
pub fn has_kind(frames: &[SseFrame], kind: &str) -> bool {
    frames.iter().any(|f| f.kind().as_deref() == Some(kind))
}

fn parse_frame(raw: &str) -> Option<SseFrame> {
    let mut id = None;
    let mut event = "message".to_string();
    let mut data = String::new();
    let mut saw_field = false;
    for line in raw.lines() {
        if let Some(v) = line.strip_prefix("id:") {
            id = v.trim().parse::<u64>().ok();
            saw_field = true;
        } else if let Some(v) = line.strip_prefix("event:") {
            event = v.trim().to_string();
            saw_field = true;
        } else if let Some(v) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(v.trim());
            saw_field = true;
        }
        // ':' comment lines (keep-alive) are ignored.
    }
    saw_field.then_some(SseFrame { id, event, data })
}
