//! Fixtures and fakes for the navigator tests: the in-memory LSP client and
//! scripted reflex, instinct and metacognition models.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio_util::sync::CancellationToken;

use rust_agent::error::ProviderError;
use rust_agent::navigator::commenter::{Commenter, RemarkContext};
use rust_agent::navigator::ports::{
    Answer, Answers, DecisionModel, FimAnswer, FimModel, FimRequest, ModelError, Question,
    Questions, ReflexContextModel, ReflexContextRequest,
};
use rust_agent::navigator::{self, NavigatorDeps, Remark};
use rust_agent::provider::Usage;

// ---------- fixtures ----------

const SYSTEMONE_FIXTURE: &str = include_str!("../fixtures/navigator/systemone_multi.json");

/// The answers of the captured System One multi-question response.
pub fn systemone_answers() -> Answers {
    let fixture: Value = serde_json::from_str(SYSTEMONE_FIXTURE).expect("systemone fixture");
    serde_json::from_value(fixture["response"]["answers"].clone()).expect("systemone answers")
}

// ---------- chat SSE bodies (metacognition tool loop) ----------

/// One SSE data line carrying `payload` (already-serialized JSON value).
pub fn sse_data(payload: &Value) -> String {
    format!("data: {payload}\n\n")
}

/// The SSE data lines followed by the `[DONE]` terminator.
pub fn sse_stream(lines: Vec<String>) -> String {
    let mut out = lines.concat();
    out.push_str("data: [DONE]\n\n");
    out
}

/// A text-only assistant delta.
pub fn delta_text(text: &str) -> Value {
    json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]})
}

/// A tool-call assistant delta (single chunk, full arguments).
pub fn delta_tool_call(id: &str, name: &str, args: &Value) -> Value {
    json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":id,
        "function":{"name":name,"arguments":args.to_string()}}]},"finish_reason":"tool_calls"}]})
}

// ---------- metacognition fake ----------

/// One scripted metacognition answer.
/// - `Remark(line, text)` => Ok(Some(...))
/// - `Silent` => Ok(None)
/// - `Fail(msg)` => Err(msg)
#[derive(Clone)]
pub enum RemarkAnswer {
    Remark(u32, &'static str),
    Silent,
    Fail(&'static str),
}

/// Scripted [`Commenter`]: consumes answers from a shared list, in order;
/// when empty, answers `Silent`. Counts calls, and can hold every call on a
/// gate so a test can interleave an edit while the request is in flight.
#[derive(Clone, Default)]
pub struct ScriptedCommenter {
    answers: Arc<Mutex<Vec<RemarkAnswer>>>,
    calls: Arc<Mutex<usize>>,
    gate: Option<Arc<CancellationToken>>,
}

impl ScriptedCommenter {
    pub fn new(answers: Vec<RemarkAnswer>) -> Self {
        Self {
            answers: Arc::new(Mutex::new(answers)),
            ..Self::default()
        }
    }

    /// Every call waits on `gate` (or its own cancellation) before answering.
    pub fn gated(answers: Vec<RemarkAnswer>, gate: Arc<CancellationToken>) -> Self {
        Self {
            gate: Some(gate),
            ..Self::new(answers)
        }
    }

    pub fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

impl Commenter for ScriptedCommenter {
    fn remark<'a>(
        &'a self,
        _ctx: &'a RemarkContext,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Remark>, String>> + Send + 'a>> {
        Box::pin(async move {
            // The answer is assigned when the call starts: a cancelled call
            // still consumes its answer (it is discarded), like a real LLM
            // request whose tokens are thrown away.
            *self.calls.lock().unwrap() += 1;
            let answer = {
                let mut answers = self.answers.lock().unwrap();
                if answers.is_empty() {
                    RemarkAnswer::Silent
                } else {
                    answers.remove(0)
                }
            };
            if let Some(gate) = &self.gate {
                tokio::select! {
                    _ = gate.cancelled() => {}
                    _ = cancel.cancelled() => return Err("cancelled".into()),
                }
            }
            match answer {
                RemarkAnswer::Remark(line, text) => Ok(Some(Remark {
                    line,
                    text: text.to_string(),
                })),
                RemarkAnswer::Silent => Ok(None),
                RemarkAnswer::Fail(msg) => Err(msg.to_string()),
            }
        })
    }
}

// ---------- instinct fake ----------

enum DecisionMode {
    /// Answers per call, in order; the last one repeats once the script
    /// runs out.
    Script(Vec<Answers>),
    /// Answers derived from the questions asked, routing to metacognition.
    Routing,
    /// Every call fails with a malformed-output error.
    Failing(String),
}

/// Scripted [`DecisionModel`]: no network, records every state it was asked
/// about and counts calls.
pub struct ScriptedDecision {
    mode: DecisionMode,
    states: Arc<Mutex<Vec<String>>>,
    calls: Arc<AtomicUsize>,
}

impl ScriptedDecision {
    fn with_mode(mode: DecisionMode) -> Self {
        Self {
            mode,
            states: Arc::default(),
            calls: Arc::default(),
        }
    }

    /// One answer set per call, in order; the last repeats afterwards.
    pub fn new(answers: Vec<Answers>) -> Self {
        Self::with_mode(DecisionMode::Script(answers))
    }

    /// The same answers on every call.
    pub fn fixed(answers: Answers) -> Self {
        Self::new(vec![answers])
    }

    /// Answers every question the way the default pipeline's scenarios need:
    /// noul 0.9 (worth a remark, drifting), choice `metacognition` when
    /// offered (else the first option), score level 0.
    pub fn routing_to_metacognition() -> Self {
        Self::with_mode(DecisionMode::Routing)
    }

    /// Every call fails with `ModelError::Malformed(message)`.
    pub fn failing(message: &str) -> Self {
        Self::with_mode(DecisionMode::Failing(message.to_string()))
    }

    pub fn states(&self) -> Vec<String> {
        self.states.lock().unwrap().clone()
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

fn routed_answer(question: &Question) -> Answer {
    match question {
        Question::Noul { .. } => Answer::Noul { noul: 0.9 },
        Question::Choice { criteria, .. } => {
            let choice = if criteria.contains_key("metacognition") {
                "metacognition"
            } else {
                criteria.keys().next().map(String::as_str).unwrap_or("")
            };
            let probabilities = criteria
                .keys()
                .map(|k| (k.clone(), if k == choice { 1.0 } else { 0.0 }))
                .collect();
            Answer::Choice {
                choice: choice.to_string(),
                probabilities,
                confidence: 1.0,
            }
        }
        Question::Score { criteria, .. } => {
            let probabilities = (0..criteria.len())
                .map(|i| (i.to_string(), if i == 0 { 1.0 } else { 0.0 }))
                .collect();
            Answer::Score {
                score: 0.0,
                legend: criteria
                    .iter()
                    .enumerate()
                    .map(|(i, s)| (i.to_string(), s.clone()))
                    .collect(),
                probabilities,
                confidence: 1.0,
            }
        }
    }
}

impl DecisionModel for ScriptedDecision {
    fn decide<'a>(
        &'a self,
        state: &'a str,
        questions: &'a Questions,
        _cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Answers, ModelError>> + Send + 'a>> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        self.states.lock().unwrap().push(state.to_string());
        let result = match &self.mode {
            DecisionMode::Script(script) => Ok(script
                .get(index)
                .or_else(|| script.last())
                .cloned()
                .unwrap_or_default()),
            DecisionMode::Routing => Ok(questions
                .iter()
                .map(|(name, question)| (name.clone(), routed_answer(question)))
                .collect()),
            DecisionMode::Failing(message) => Err(ModelError::Malformed(message.clone())),
        };
        Box::pin(async move { result })
    }
}

// ---------- reflex fakes ----------

/// Scripted [`FimModel`]: answers from a queue consumed in call order, then
/// from a fallback text; records every request; can delay its answer
/// (virtual time) so a second request arrives while the first is in flight.
#[derive(Clone, Default)]
pub struct ScriptedFim {
    answers: Arc<Mutex<VecDeque<String>>>,
    fallback: String,
    requests: Arc<Mutex<Vec<FimRequest>>>,
    delay_ms: u64,
}

impl ScriptedFim {
    /// Answers in call order; an empty text once they run out.
    pub fn new(answers: Vec<String>) -> Self {
        Self {
            answers: Arc::new(Mutex::new(answers.into())),
            ..Self::default()
        }
    }

    /// The same text on every call.
    pub fn fixed(text: &str) -> Self {
        Self {
            fallback: text.to_string(),
            ..Self::default()
        }
    }

    /// Waits `delay_ms` of virtual time before answering (cancellable).
    pub fn with_delay(mut self, delay_ms: u64) -> Self {
        self.delay_ms = delay_ms;
        self
    }

    pub fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    pub fn last_request(&self) -> FimRequest {
        self.requests.lock().unwrap().last().cloned().unwrap()
    }

    /// The prompt of every request received, in order.
    pub fn prompts(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.prompt.clone())
            .collect()
    }
}

impl FimModel for ScriptedFim {
    fn complete<'a>(
        &'a self,
        request: FimRequest,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<FimAnswer, ModelError>> + Send + 'a>> {
        Box::pin(async move {
            // The answer is assigned when the call starts: a cancelled call
            // still consumes it, like a real request whose tokens are paid
            // and thrown away.
            self.requests.lock().unwrap().push(request);
            let text = self
                .answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| self.fallback.clone());
            if self.delay_ms > 0 {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        return Err(ModelError::Provider(ProviderError::Canceled {
                            partial: None,
                        }));
                    }
                    _ = tokio::time::sleep(Duration::from_millis(self.delay_ms)) => {}
                }
            }
            Ok(FimAnswer {
                text,
                usage: Usage::default(),
                latency: Duration::from_millis(5),
            })
        })
    }
}

/// A reflex-context model answering from a script (one entry per call;
/// `Err` fails the refresh) and recording every request it received.
#[derive(Clone, Default)]
pub struct ScriptedReflexContext {
    answers: Arc<Mutex<Vec<Result<&'static str, &'static str>>>>,
    requests: Arc<Mutex<Vec<ReflexContextRequest>>>,
}

impl ScriptedReflexContext {
    pub fn new(answers: Vec<Result<&'static str, &'static str>>) -> Self {
        Self {
            answers: Arc::new(Mutex::new(answers)),
            requests: Arc::default(),
        }
    }

    pub fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    pub fn requests(&self) -> Vec<ReflexContextRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl ReflexContextModel for ScriptedReflexContext {
    fn refresh<'a>(
        &'a self,
        request: &'a ReflexContextRequest,
    ) -> Pin<Box<dyn Future<Output = Result<String, ModelError>> + Send + 'a>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request.clone());
            let next = {
                let mut answers = self.answers.lock().unwrap();
                if answers.is_empty() {
                    Err("script exhausted")
                } else {
                    answers.remove(0)
                }
            };
            next.map(str::to_string)
                .map_err(|m| ModelError::Malformed(m.to_string()))
        })
    }
}

// ---------- LSP client harness ----------

/// The client side of an in-memory LSP connection to the real navigator
/// server: Content-Length framed JSON-RPC over a duplex pair.
pub struct LspClient {
    writer: DuplexStream,
    reader: DuplexStream,
    /// Leftover bytes from previous reads (messages can coalesce).
    buf: Vec<u8>,
}

impl LspClient {
    fn frame(msg: &Value) -> String {
        let body = serde_json::to_string(msg).unwrap();
        format!("Content-Length: {}\r\n\r\n{}", body.len(), body)
    }

    pub async fn send(&mut self, msg: &Value) {
        let frame = Self::frame(msg);
        self.writer.write_all(frame.as_bytes()).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    /// Reads one framed JSON-RPC message from the server. Keeps leftover
    /// bytes buffered so coalesced messages are not lost between calls.
    pub async fn recv(&mut self) -> Value {
        loop {
            if let Some((msg, consumed)) = try_parse(&self.buf) {
                self.buf.drain(..consumed);
                return msg;
            }
            let mut tmp = [0u8; 4096];
            let n = self.reader.read(&mut tmp).await.unwrap_or(0);
            assert!(n > 0, "server closed the stream");
            self.buf.extend_from_slice(&tmp[..n]);
        }
    }

    /// Reads messages until one matches method; returns it. Other messages
    /// (e.g. navigator/status) are skipped but recorded in `seen`.
    pub async fn expect(&mut self, method: &str, seen: &mut Vec<Value>) -> Value {
        loop {
            let msg = self.recv().await;
            if msg.get("method").and_then(|m| m.as_str()) == Some(method) {
                return msg;
            }
            seen.push(msg);
        }
    }

    /// Reads notifications until a `navigator/status` with the given state
    /// arrives; other notifications are skipped. This is the synchronization
    /// point for tests that expect no remark (silent) or an error.
    pub async fn expect_status(&mut self, state: &str) -> Value {
        loop {
            let msg = self.recv().await;
            if msg.get("method").and_then(|m| m.as_str()) == Some("navigator/status")
                && msg["params"]["state"].as_str() == Some(state)
            {
                return msg;
            }
        }
    }

    /// Collects every message readable within a virtual-time window, then
    /// stops. Used to assert that NOTHING (e.g. a stale remark) arrives.
    pub async fn collect_for(&mut self, window: Duration) -> Vec<Value> {
        let mut out = Vec::new();
        let mut scratch = [0u8; 4096];
        loop {
            match tokio::time::timeout(window, self.reader.read(&mut scratch)).await {
                Ok(Ok(n)) if n > 0 => {
                    self.buf.extend_from_slice(&scratch[..n]);
                    while let Some((msg, consumed)) = try_parse(&self.buf) {
                        self.buf.drain(..consumed);
                        out.push(msg);
                    }
                }
                _ => return out,
            }
        }
    }

    pub async fn initialize(&mut self, options: Value) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": null,
                "capabilities": {},
                "initializationOptions": options
            }
        }))
        .await;
        let resp = self.recv().await;
        assert!(resp.get("result").is_some(), "initialize failed: {resp}");
        self.send(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}))
            .await;
    }

    pub async fn did_open(&mut self, uri: &str, text: &str, version: i32) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": "rust",
                    "version": version,
                    "text": text
                }
            }
        }))
        .await;
    }

    pub async fn did_change(&mut self, uri: &str, text: &str, version: i32) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }]
            }
        }))
        .await;
    }

    pub async fn did_save(&mut self, uri: &str) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didSave",
            "params": { "textDocument": { "uri": uri } }
        }))
        .await;
    }

    pub async fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        }))
        .await;
    }
}

/// Parses one framed message; returns it plus total bytes consumed.
fn try_parse(buf: &[u8]) -> Option<(Value, usize)> {
    if buf.is_empty() {
        return None;
    }
    let s = String::from_utf8_lossy(buf);
    let idx = s.find("\r\n\r\n")?;
    let header = &s[..idx];
    let content_len: usize = header
        .lines()
        .find_map(|l| {
            l.to_lowercase()
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap_or(0);
    let body_start = idx + 4;
    let total = body_start + content_len;
    if buf.len() < total {
        return None;
    }
    let msg = serde_json::from_str(&s[body_start..total]).ok()?;
    Some((msg, total))
}

/// Spawns the server on duplex pairs; returns the client side. Wireup:
/// client writes -> server input; server output -> client reads. The
/// instinct is [`ScriptedDecision::routing_to_metacognition`]: changes are
/// worth a short metacognition remark and drift enough to wake cognition
/// when an intent exists.
pub fn spawn_server(deps: NavigatorDeps) -> LspClient {
    let deps = deps.with_decision(Arc::new(ScriptedDecision::routing_to_metacognition()));
    let (server_input, client_write) = tokio::io::duplex(64 * 1024);
    let (client_read, server_output) = tokio::io::duplex(64 * 1024);
    tokio::spawn(navigator::serve(server_input, server_output, deps));
    LspClient {
        writer: client_write,
        reader: client_read,
        buf: Vec::new(),
    }
}
