//! Navigator LSP integration tests: drive the real server over an in-memory
//! duplex stream with real JSON-RPC framed messages (Content-Length headers),
//! a scripted mock commenter, and tokio's paused clock.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio_util::sync::CancellationToken;

use rust_agent::navigator::commenter::{Commenter, RemarkContext};
use rust_agent::navigator::ports::{
    Answer as DecisionAnswer, DecisionModel, FimAnswer, FimModel, FimRequest, ModelError, Question,
};
use rust_agent::navigator::{self, NavigatorDeps, Remark};
use std::collections::BTreeMap;

// ---------- mock commenter ----------

/// Scripted mock: consumes answers from a shared Vec, in order. An answer is:
/// - `Remark(line, text)` => Ok(Some(...))
/// - `Silent` => Ok(None)
/// - `Fail(msg)` => Err(msg)
///
/// When empty, returns Silent.
#[derive(Clone)]
enum Answer {
    Remark(u32, &'static str),
    Silent,
    Fail(&'static str),
}

#[derive(Clone, Default)]
struct MockCommenter {
    answers: Arc<std::sync::Mutex<Vec<Answer>>>,
    /// Number of remark() calls observed.
    calls: Arc<std::sync::Mutex<usize>>,
    /// When set, remark() waits on this token before answering (lets tests
    /// interleave an edit while the request is in flight).
    gate: Option<Arc<CancellationToken>>,
}

impl MockCommenter {
    fn new(answers: Vec<Answer>) -> Self {
        Self {
            answers: Arc::new(std::sync::Mutex::new(answers)),
            calls: Arc::new(std::sync::Mutex::new(0)),
            gate: None,
        }
    }

    fn gated(answers: Vec<Answer>, gate: Arc<CancellationToken>) -> Self {
        Self {
            answers: Arc::new(std::sync::Mutex::new(answers)),
            calls: Arc::new(std::sync::Mutex::new(0)),
            gate: Some(gate),
        }
    }

    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

impl Commenter for MockCommenter {
    fn remark<'a>(
        &'a self,
        _ctx: &'a RemarkContext,
        cancel: &'a CancellationToken,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<Remark>, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            // The answer is assigned when the call starts: a cancelled call
            // still consumes its answer (it is discarded), like a real LLM
            // request whose tokens are thrown away.
            *self.calls.lock().unwrap() += 1;
            let answer = {
                let mut answers = self.answers.lock().unwrap();
                if answers.is_empty() {
                    Answer::Silent
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
                Answer::Remark(line, text) => Ok(Some(Remark {
                    line,
                    text: text.to_string(),
                })),
                Answer::Silent => Ok(None),
                Answer::Fail(msg) => Err(msg.to_string()),
            }
        })
    }
}

// ---------- LSP client harness ----------

struct LspClient {
    writer: DuplexStream,
    reader: DuplexStream,
    /// Leftover bytes from previous reads (messages can coalesce).
    buf: Vec<u8>,
}

impl LspClient {
    fn frame(&mut self, msg: &Value) -> String {
        let body = serde_json::to_string(msg).unwrap();
        format!("Content-Length: {}\r\n\r\n{}", body.len(), body)
    }

    async fn send(&mut self, msg: &Value) {
        let frame = self.frame(msg);
        self.writer.write_all(frame.as_bytes()).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    /// Reads one framed JSON-RPC message from the server. Keeps leftover
    /// bytes buffered so coalesced messages are not lost between calls.
    async fn recv(&mut self) -> Value {
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
    async fn expect(&mut self, method: &str, seen: &mut Vec<Value>) -> Value {
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
    async fn expect_status(&mut self, state: &str) -> Value {
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
    async fn collect_for(&mut self, window: Duration) -> Vec<Value> {
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

    async fn initialize(&mut self, options: Value) {
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

    async fn did_open(&mut self, uri: &str, text: &str, version: i32) {
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

    async fn did_change(&mut self, uri: &str, text: &str, version: i32) {
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

    async fn did_save(&mut self, uri: &str) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didSave",
            "params": { "textDocument": { "uri": uri } }
        }))
        .await;
    }

    async fn notify(&mut self, method: &str, params: Value) {
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

/// Spawns the server on duplex pairs; returns the client side.
/// Wireup: client writes -> server input; server output -> client reads.
fn spawn_server(deps: NavigatorDeps) -> LspClient {
    let deps = deps.with_decision(Arc::new(ScriptedDecision));
    let (server_input, client_write) = tokio::io::duplex(64 * 1024);
    let (client_read, server_output) = tokio::io::duplex(64 * 1024);
    tokio::spawn(navigator::serve(server_input, server_output, deps));
    LspClient {
        writer: client_write,
        reader: client_read,
        buf: Vec::new(),
    }
}

/// The default pipeline's answers for existing integration scenarios:
/// changes are worth a short metacognition remark, but do not drift from
/// intent enough to wake cognition.
struct ScriptedDecision;

impl DecisionModel for ScriptedDecision {
    fn decide<'a>(
        &'a self,
        _state: &'a str,
        questions: &'a BTreeMap<String, Question>,
        _cancel: &'a CancellationToken,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<BTreeMap<String, DecisionAnswer>, ModelError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let mut answers = BTreeMap::new();
            for (name, question) in questions {
                match question {
                    Question::Noul { .. } => {
                        answers.insert(name.clone(), DecisionAnswer::Noul { noul: 0.9 });
                    }
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
                        answers.insert(
                            name.clone(),
                            DecisionAnswer::Choice {
                                choice: choice.to_string(),
                                probabilities,
                                confidence: 1.0,
                            },
                        );
                    }
                    Question::Score { criteria, .. } => {
                        let probabilities = (0..criteria.len())
                            .map(|i| (i.to_string(), if i == 0 { 1.0 } else { 0.0 }))
                            .collect();
                        answers.insert(
                            name.clone(),
                            DecisionAnswer::Score {
                                score: 0.0,
                                legend: criteria
                                    .iter()
                                    .enumerate()
                                    .map(|(i, s)| (i.to_string(), s.clone()))
                                    .collect(),
                                probabilities,
                                confidence: 1.0,
                            },
                        );
                    }
                }
            }
            Ok(answers)
        })
    }
}

struct ScriptedFim {
    text: &'static str,
}

impl FimModel for ScriptedFim {
    fn complete<'a>(
        &'a self,
        _request: FimRequest,
        _cancel: &'a CancellationToken,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<FimAnswer, ModelError>> + Send + 'a>,
    > {
        Box::pin(async move {
            Ok(FimAnswer {
                text: self.text.to_string(),
                usage: Default::default(),
                latency: Duration::from_millis(5),
            })
        })
    }
}

const URI: &str = "file:///ws/src/main.rs";

fn base_text() -> String {
    ["fn main() {", "    println!(\"hello\");", "}"].join("\n")
}
/// >= 3 changed lines relative to base.
fn big_edit() -> String {
    [
        "fn main() {",
        "    let x = 1;",
        "    let y = 2;",
        "    println!(\"{x}-{y}\");",
        "}",
    ]
    .join("\n")
}

// ---------- tests ----------

/// After a 5-line change and 3 s idle, one navigator/remark arrives.
#[tokio::test(start_paused = true)]
async fn idle_after_big_edit_emits_remark() {
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let mock = MockCommenter::new(vec![Answer::Remark(1, "careful: x is unused")]);
    let mut client = spawn_server(NavigatorDeps::new(Arc::new(mock.clone()), log_path.clone()));

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;

    tokio::time::advance(Duration::from_millis(3001)).await;
    let mut seen = Vec::new();
    let remark = client.expect("navigator/remark", &mut seen).await;
    let params = &remark["params"];
    assert_eq!(params["uri"], URI);
    assert_eq!(params["line"], 1);
    assert_eq!(params["text"], "careful: x is unused");
    assert!(params["id"].as_str().is_some());

    let records = std::fs::read_to_string(log_path).unwrap();
    assert!(records.contains("\"kind\":\"remark\""));
    assert!(records.contains("\"kind\":\"trigger\""));
}

/// A 1-line change never triggers (skip: too_few_changed_lines).
#[tokio::test(start_paused = true)]
async fn one_line_change_never_triggers() {
    let mock = MockCommenter::new(vec![Answer::Remark(0, "should not happen")]);
    let tmp = tempfile::tempdir().unwrap();
    let mut client = spawn_server(NavigatorDeps::new(
        Arc::new(mock.clone()),
        tmp.path().join("nav.jsonl"),
    ));

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    // only one line changed
    client
        .did_change(URI, "fn main() {\n    println!(\"hi\");\n}", 2)
        .await;

    tokio::time::advance(Duration::from_millis(10_000)).await;
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(mock.calls(), 0, "commenter must not be called");
}

/// A diagnostics event no trigger listens to logs nothing; a too-small
/// change logs a light record, without the rendered state.
#[tokio::test(start_paused = true)]
async fn skips_logs_light_or_not_at_all() {
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let mock = MockCommenter::new(vec![Answer::Silent]);
    let mut client = spawn_server(NavigatorDeps::new(Arc::new(mock), log_path.clone()));

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client
        .notify("navigator/diagnostics", json!({"uri": URI, "items": []}))
        .await;
    client
        .did_change(URI, "fn main() {\n    println!(\"hi\");\n}", 2)
        .await;
    client.did_save(URI).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let records = std::fs::read_to_string(&log_path).unwrap_or_default();
    let triggers: Vec<Value> = records
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|r| r["kind"] == "trigger")
        .collect();
    assert_eq!(triggers.len(), 1, "{records}");
    assert_eq!(triggers[0]["event"], "save");
    assert_eq!(triggers[0]["reason"], "too_few_changed_lines");
    assert!(triggers[0].get("template_inputs").is_none());
    assert!(triggers[0].get("rendered_state").is_none());
}

/// Cooldown suppresses a second remark on the same file.
#[tokio::test(start_paused = true)]
async fn cooldown_suppresses_second_remark() {
    let mock = MockCommenter::new(vec![
        Answer::Remark(0, "first"),
        Answer::Remark(0, "second"),
    ]);
    let tmp = tempfile::tempdir().unwrap();
    let mut client = spawn_server(NavigatorDeps::new(
        Arc::new(mock.clone()),
        tmp.path().join("nav.jsonl"),
    ));

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    tokio::time::advance(Duration::from_millis(3001)).await;
    let mut seen = Vec::new();
    let first = client.expect("navigator/remark", &mut seen).await;
    assert_eq!(first["params"]["text"], "first");
    client.expect_status("idle").await;

    // A second big edit within the 45 s cooldown: the idle timer fires but
    // the policy skips, so no second commenter call happens.
    let bigger = format!("{}\n// a\n// b\n// c\n", big_edit());
    client.did_change(URI, &bigger, 3).await;
    tokio::time::advance(Duration::from_millis(5000)).await;
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(mock.calls(), 1, "cooldown must suppress the second call");

    // After the cooldown elapses, a new edit fires again.
    tokio::time::advance(Duration::from_millis(45_000)).await;
    let bigger2 = format!("{}\n// d\n// e\n// f\n", big_edit());
    client.did_change(URI, &bigger2, 4).await;
    tokio::time::advance(Duration::from_millis(3001)).await;
    let second = client.expect("navigator/remark", &mut seen).await;
    assert_eq!(second["params"]["text"], "second");
}

/// Silent answer emits no remark.
#[tokio::test(start_paused = true)]
async fn silent_answer_emits_no_remark() {
    let mock = MockCommenter::new(vec![Answer::Silent]);
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let mut client = spawn_server(NavigatorDeps::new(Arc::new(mock), log_path.clone()));

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    tokio::time::advance(Duration::from_millis(3001)).await;
    // Silence: the "idle" status after "thinking" is the sync point.
    client.expect_status("idle").await;

    let records = std::fs::read_to_string(log_path).unwrap();
    assert!(records.contains("\"kind\":\"silent\""));
    assert!(!records.contains("\"kind\":\"remark\""));
}

/// Commenter error emits navigator/status error and the server stays alive.
#[tokio::test(start_paused = true)]
async fn commenter_error_emits_status_error_and_survives() {
    let mock = MockCommenter::new(vec![Answer::Fail("provider exploded")]);
    let tmp = tempfile::tempdir().unwrap();
    let mut client = spawn_server(NavigatorDeps::new(
        Arc::new(mock),
        tmp.path().join("nav.jsonl"),
    ));

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    tokio::time::advance(Duration::from_millis(3001)).await;
    let status = client.expect_status("error").await;
    assert_eq!(status["params"]["message"], "provider exploded");

    // The server is still alive: a request answers.
    client
        .send(&json!({"jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": null}))
        .await;
    let resp = client.recv().await;
    assert_eq!(resp["id"], 99);
    assert!(resp.get("result").is_some());
}

/// Typing again before the mock answers yields no remark (in-flight cancel).
#[tokio::test(start_paused = true)]
async fn retyping_cancels_in_flight_request() {
    let gate = Arc::new(CancellationToken::new());
    // The gated call holds "stale"; any later call answers Silent.
    let mock = MockCommenter::gated(
        vec![Answer::Remark(0, "stale"), Answer::Silent],
        gate.clone(),
    );
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let mut client = spawn_server(NavigatorDeps::new(Arc::new(mock), log_path.clone()));

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    tokio::time::advance(Duration::from_millis(3001)).await;
    // "thinking" proves the first (gated) call is in flight.
    client.expect_status("thinking").await;

    // The user types again: the in-flight call must be cancelled.
    client
        .did_change(URI, &format!("{}\n// tail\n", big_edit()), 3)
        .await;
    tokio::time::sleep(Duration::from_millis(1)).await;

    // Release the gate: the cancelled call must not emit "stale". A later
    // idle window may legitimately fire a fresh call (Silent).
    gate.cancel();
    tokio::time::advance(Duration::from_millis(10_000)).await;
    tokio::time::sleep(Duration::from_millis(10)).await;

    let msgs = client.collect_for(Duration::from_millis(100)).await;
    for m in &msgs {
        assert_ne!(
            m["params"]["text"].as_str().unwrap_or(""),
            "stale",
            "cancelled answer leaked: {m}"
        );
    }
    let records = std::fs::read_to_string(log_path).unwrap();
    assert!(
        !records.contains("\"text\":\"stale\""),
        "cancelled answer must not be logged"
    );
}

/// Save fires the trigger without waiting for the idle timer.
#[tokio::test(start_paused = true)]
async fn save_triggers_immediately() {
    let mock = MockCommenter::new(vec![Answer::Remark(2, "watch the unwrap")]);
    let tmp = tempfile::tempdir().unwrap();
    let mut client = spawn_server(NavigatorDeps::new(
        Arc::new(mock),
        tmp.path().join("nav.jsonl"),
    ));

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    // No time advance: save must trigger on its own.
    client.did_save(URI).await;
    let mut seen = Vec::new();
    let remark = client.expect("navigator/remark", &mut seen).await;
    assert_eq!(remark["params"]["text"], "watch the unwrap");
}

/// Feedback is written to the log.
#[tokio::test(start_paused = true)]
async fn feedback_is_written_to_log() {
    let mock = MockCommenter::new(vec![]);
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let mut client = spawn_server(NavigatorDeps::new(Arc::new(mock), log_path.clone()));

    client.initialize(json!({})).await;
    client
        .notify(
            "navigator/feedback",
            json!({"id": "abc123", "verdict": "useful"}),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let records = std::fs::read_to_string(log_path).unwrap();
    assert!(records.contains("\"kind\":\"feedback\""));
    assert!(records.contains("\"verdict\":\"useful\""));
    assert!(records.contains("\"id\":\"abc123\""));
}

/// didClose clears state and notifies navigator/clear.
#[tokio::test(start_paused = true)]
async fn did_close_clears_remark() {
    let mock = MockCommenter::new(vec![]);
    let tmp = tempfile::tempdir().unwrap();
    let mut client = spawn_server(NavigatorDeps::new(
        Arc::new(mock),
        tmp.path().join("nav.jsonl"),
    ));

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client
        .notify(
            "textDocument/didClose",
            json!({"textDocument": {"uri": URI}}),
        )
        .await;
    let mut seen = Vec::new();
    let clear = client.expect("navigator/clear", &mut seen).await;
    assert_eq!(clear["params"]["uri"], URI);
}

/// The pipeline's configured pre-filter, rather than the prototype's LSP
/// minChangedLines setting, controls whether the instinct runs.
#[tokio::test(start_paused = true)]
async fn config_update_changes_idle_threshold() {
    let mock = MockCommenter::new(vec![Answer::Remark(0, "cfg-driven")]);
    let tmp = tempfile::tempdir().unwrap();
    let mut client = spawn_server(NavigatorDeps::new(
        Arc::new(mock),
        tmp.path().join("nav.jsonl"),
    ));

    client
        .initialize(json!({"idleMs": 1000, "minChangedLines": 1, "cooldownMs": 60000}))
        .await;
    client.did_open(URI, &base_text(), 1).await;
    // The built-in pipeline still requires at least three changed lines.
    client.did_change(URI, &big_edit(), 2).await;
    tokio::time::advance(Duration::from_millis(3001)).await;
    let mut seen = Vec::new();
    let remark = client.expect("navigator/remark", &mut seen).await;
    assert_eq!(remark["params"]["text"], "cfg-driven");
}

/// Disabled config suppresses everything.
#[tokio::test(start_paused = true)]
async fn disabled_config_suppresses_evaluation() {
    let mock = MockCommenter::new(vec![Answer::Remark(0, "nope")]);
    let tmp = tempfile::tempdir().unwrap();
    let mut client = spawn_server(NavigatorDeps::new(
        Arc::new(mock.clone()),
        tmp.path().join("nav.jsonl"),
    ));

    client.initialize(json!({"enabled": false})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    tokio::time::advance(Duration::from_millis(10_000)).await;
    client.did_save(URI).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(mock.calls(), 0);
}

/// navigator/intent and navigator/diagnostics are accepted and stored
/// (observable through the log's remark after a trigger).
#[tokio::test(start_paused = true)]
async fn intent_and_diagnostics_are_accepted() {
    let mock = MockCommenter::new(vec![Answer::Silent]);
    let tmp = tempfile::tempdir().unwrap();
    let mut client = spawn_server(NavigatorDeps::new(
        Arc::new(mock),
        tmp.path().join("nav.jsonl"),
    ));

    client.initialize(json!({})).await;
    client
        .notify("navigator/intent", json!({"text": "fix the parser"}))
        .await;
    client
        .notify(
            "navigator/diagnostics",
            json!({
                "uri": URI,
                "items": [{"line": 1, "severity": "error", "message": "mismatched types"}]
            }),
        )
        .await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    tokio::time::advance(Duration::from_millis(3001)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Server must still respond to a request (custom notifications didn't
    // break it).
    client
        .send(&json!({"jsonrpc": "2.0", "id": 7, "method": "shutdown", "params": null}))
        .await;
    let resp = client.recv().await;
    assert!(resp.get("result").is_some(), "server broken: {resp}");
}

/// navigator/inlineCompletion answers with an item and request id from the
/// reflex service, including the result range.
#[tokio::test(start_paused = true)]
async fn inline_completion_answers_empty_items() {
    let mock = MockCommenter::new(vec![]);
    let tmp = tempfile::tempdir().unwrap();
    let deps = NavigatorDeps::new(Arc::new(mock), tmp.path().join("nav.jsonl"))
        .with_fim(Arc::new(ScriptedFim { text: "hello" }))
        .with_model_ids(Some("mercury-edit-2".into()), None, None);
    let mut client = spawn_server(deps);

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": "navigator/inlineCompletion",
            "params": {"uri": URI, "version": 1, "line": 0, "character": 4}
        }))
        .await;
    tokio::time::advance(Duration::from_millis(151)).await;
    let resp = client.recv().await;
    assert_eq!(resp["id"], 21);
    assert_eq!(resp["result"]["items"][0]["text"], "hello");
    assert!(resp["result"]["requestId"].as_str().is_some());
}

/// navigator/reflexShown is accepted and logged as a reflex_shown record.
#[tokio::test(start_paused = true)]
async fn reflex_shown_is_logged() {
    let mock = MockCommenter::new(vec![]);
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let mut client = spawn_server(NavigatorDeps::new(Arc::new(mock), log_path.clone()));

    client.initialize(json!({})).await;
    client
        .notify(
            "navigator/reflexShown",
            json!({"requestId": "req1", "shownAtMs": 1759000000123u64}),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let records = std::fs::read_to_string(log_path).unwrap();
    assert!(records.contains("\"kind\":\"reflex_shown\""));
    assert!(records.contains("\"request_id\":\"req1\""));
    assert!(records.contains("\"shown_at_ms\":1759000000123"));
}

/// Trigger records carry all pipeline-era replay fields as well as the
/// requested and delivered outcome.
#[tokio::test(start_paused = true)]
async fn trigger_records_carry_replay_fields() {
    let mock = MockCommenter::new(vec![Answer::Remark(1, "careful")]);
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let mut client = spawn_server(NavigatorDeps::new(Arc::new(mock), log_path.clone()));

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    tokio::time::advance(Duration::from_millis(3001)).await;
    let mut seen = Vec::new();
    let _ = client.expect("navigator/remark", &mut seen).await;

    let records = std::fs::read_to_string(log_path).unwrap();
    let trigger: Value = records
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .find(|v: &Value| v["kind"] == "trigger")
        .expect("a trigger record");
    assert!(trigger["trigger_id"].as_str().is_some());
    assert!(trigger["run_id"].as_str().is_some());
    assert_eq!(trigger["uri"], URI);
    assert_eq!(trigger["version"], 2);
    assert_eq!(trigger["event"], "idle");
    assert_eq!(trigger["prefilter"]["outcome"], "pass");
    assert_eq!(trigger["prefilter"]["inputs"]["changed_lines"], 4);
    assert_eq!(trigger["requested_action"], "remark");
    assert_eq!(trigger["delivered"], "remark");
    assert!(trigger["remark_id"].as_str().is_some());
    assert!(trigger["pipeline_hash"].as_str().is_some());
    assert!(trigger["template_inputs"].is_object());
    assert!(trigger["rendered_state"].as_str().is_some());
    assert!(trigger["questions"].is_object());
    assert!(trigger["answers"].is_object());
    assert_eq!(trigger["branch"], 1);
}

/// With `server_info` in the deps, `initialized` delivers
/// `navigator/serverInfo` with exactly those values: the extension learns
/// the cognition chat server's port and bearer token.
#[tokio::test(start_paused = true)]
async fn server_info_arrives_after_initialized_with_the_configured_values() {
    let mock = MockCommenter::new(vec![]);
    let tmp = tempfile::tempdir().unwrap();
    let deps = NavigatorDeps::new(Arc::new(mock), tmp.path().join("nav.jsonl")).with_server_info(
        navigator::ServerInfoParams {
            port: 45678,
            token: "tok-123".to_string(),
        },
    );
    let mut client = spawn_server(deps);

    client.initialize(json!({})).await;
    let mut seen = Vec::new();
    let info = client.expect("navigator/serverInfo", &mut seen).await;
    assert_eq!(info["params"]["port"], 45678);
    assert_eq!(info["params"]["token"], "tok-123");
}

// ---------- wake_cognition delivery ----------

/// A feed whose active conversation is a scripted poster: records every
/// text posted and answers Ok. Active means one driver message was
/// observed for a registered session.
async fn feed_with_active_session() -> (
    navigator::ConversationFeed,
    Arc<std::sync::Mutex<Vec<String>>>,
) {
    use navigator::conversation::SessionPost;
    let feed = navigator::ConversationFeed::new();
    let posted = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = posted.clone();
    let post: SessionPost = Arc::new(move |text| {
        sink.lock().unwrap().push(text);
        Box::pin(async { Ok::<(), String>(()) })
    });
    let session_id = ulid::Ulid::new();
    feed.register_session(session_id, post);
    feed.observe(&rust_agent::bus::event::EventEnvelope::new(
        session_id,
        1,
        rust_agent::bus::event::EventKind::MessageReceived,
        serde_json::json!({
            "command_id": ulid::Ulid::new().to_string(),
            "source": "user:http",
            "target": "agent:main",
            "mode": "next-turn",
            "content": "add an auth remove command",
        }),
    ))
    .await;
    (feed, posted)
}

/// With a feed whose active session exists and an instinct that requests
/// `wake_cognition`, the branch posts ONCE through the feed and the
/// trigger record is delivered `wake` with the session id.
#[tokio::test(start_paused = true)]
async fn wake_cognition_posts_once_and_logs_wake() {
    let mock = MockCommenter::new(vec![]);
    let tmp = tempfile::tempdir().unwrap();
    let (feed, posted) = feed_with_active_session().await;
    // The intent record must exist for the built-in wake branch.
    let ws = tmp.path();
    std::fs::create_dir_all(ws.join(".navigator")).unwrap();
    std::fs::write(
        ws.join(".navigator/intent.md"),
        "## Intent\nadd an auth remove command",
    )
    .unwrap();

    let deps = NavigatorDeps::new(Arc::new(mock), ws.join("nav.jsonl"))
        .with_conversation(feed)
        .with_dirs(Some(ws.to_path_buf()), None);
    // spawn_server wires its ScriptedDecision, whose noul answers (0.9)
    // fire the built-in drift branch when an intent exists.
    let mut client = spawn_server(deps);

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    client.did_save(URI).await;
    // The wake path ends with the idle status.
    client.expect_status("idle").await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let posted = posted.lock().unwrap().clone();
    assert_eq!(posted.len(), 1, "exactly one wake posted");
    let message = &posted[0];
    assert!(message.contains(URI), "names the file: {message}");
    assert!(
        message.contains("auth remove command"),
        "carries the intent: {message}"
    );

    let records = std::fs::read_to_string(ws.join("nav.jsonl")).unwrap();
    let trigger: Value = records
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .find(|v: &Value| v["kind"] == "trigger")
        .expect("a trigger record");
    assert_eq!(trigger["requested_action"], "wake_cognition");
    assert_eq!(trigger["delivered"], "wake");
    assert!(trigger["session"].as_str().is_some(), "session id logged");
}

/// Without a feed (or with no active conversation), the wake branch keeps
/// today's behaviour: nothing posted, delivered `none`, reason
/// `cognition_unavailable`.
#[tokio::test(start_paused = true)]
async fn wake_cognition_without_a_feed_logs_cognition_unavailable() {
    let mock = MockCommenter::new(vec![]);
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    std::fs::create_dir_all(ws.join(".navigator")).unwrap();
    std::fs::write(
        ws.join(".navigator/intent.md"),
        "## Intent\nadd an auth remove command",
    )
    .unwrap();
    let feed = navigator::ConversationFeed::new(); // registered nothing: no active session
    let deps = NavigatorDeps::new(Arc::new(mock), ws.join("nav.jsonl"))
        .with_conversation(feed)
        .with_dirs(Some(ws.to_path_buf()), None);
    // spawn_server wires its ScriptedDecision, whose noul answers (0.9)
    // fire the built-in drift branch when an intent exists.
    let mut client = spawn_server(deps);

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    client.did_save(URI).await;
    client.expect_status("idle").await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let records = std::fs::read_to_string(ws.join("nav.jsonl")).unwrap();
    let trigger: Value = records
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .find(|v: &Value| v["kind"] == "trigger")
        .expect("a trigger record");
    assert_eq!(trigger["delivered"], "none");
    assert_eq!(trigger["reason"], "cognition_unavailable");
}

/// With a `ConversationFeed` holding a digest (an active session with
/// driver messages), the logged trigger's `template_inputs.conversation`
/// carries that digest: the instinct and the metacognition see what the
/// driver and the cognition agreed.
#[tokio::test(start_paused = true)]
async fn trigger_template_inputs_carry_the_conversation_digest() {
    let mock = MockCommenter::new(vec![Answer::Silent]);
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let (feed, _posted) = feed_with_active_session().await;
    let deps = NavigatorDeps::new(Arc::new(mock), log_path.clone()).with_conversation(feed);
    let mut client = spawn_server(deps);

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    tokio::time::advance(Duration::from_millis(3001)).await;
    client.expect_status("idle").await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let records = std::fs::read_to_string(log_path).unwrap();
    let trigger: Value = records
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .find(|v: &Value| v["kind"] == "trigger")
        .expect("a trigger record");
    let conversation = trigger["template_inputs"]["conversation"]
        .as_str()
        .expect("conversation template input");
    assert!(
        conversation.contains("driver: add an auth remove command"),
        "the digest: {conversation}"
    );
}

/// Without `server_info` in the deps, NO `navigator/serverInfo` ever leaves
/// the server (the chat stays unannounced when its server failed to bind).
#[tokio::test(start_paused = true)]
async fn without_server_info_no_notification_is_sent() {
    let mock = MockCommenter::new(vec![]);
    let tmp = tempfile::tempdir().unwrap();
    let mut client = spawn_server(NavigatorDeps::new(
        Arc::new(mock),
        tmp.path().join("nav.jsonl"),
    ));

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    tokio::time::advance(Duration::from_millis(3001)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let msgs = client.collect_for(Duration::from_millis(100)).await;
    assert!(
        msgs.iter()
            .all(|m| m.get("method").and_then(|m| m.as_str()) != Some("navigator/serverInfo")),
        "no serverInfo without server_info deps: {msgs:?}"
    );
}

// ---------- reflex context refresh ----------

use rust_agent::navigator::ports::{ReflexContextModel, ReflexContextRequest};

/// A reflex-context model answering from a script (one entry per call;
/// `Err` fails the refresh) and recording every request it received.
#[derive(Clone, Default)]
struct ScriptedReflexContext {
    answers: Arc<std::sync::Mutex<Vec<Result<&'static str, &'static str>>>>,
    requests: Arc<std::sync::Mutex<Vec<ReflexContextRequest>>>,
}

impl ScriptedReflexContext {
    fn new(answers: Vec<Result<&'static str, &'static str>>) -> Self {
        Self {
            answers: Arc::new(std::sync::Mutex::new(answers)),
            requests: Arc::default(),
        }
    }

    fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl ReflexContextModel for ScriptedReflexContext {
    fn refresh<'a>(
        &'a self,
        request: &'a ReflexContextRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, ModelError>> + Send + 'a>>
    {
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

/// A FIM that records the prompt of every request.
#[derive(Clone, Default)]
struct RecordingFim {
    prompts: Arc<std::sync::Mutex<Vec<String>>>,
}

impl FimModel for RecordingFim {
    fn complete<'a>(
        &'a self,
        request: FimRequest,
        _cancel: &'a CancellationToken,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<FimAnswer, ModelError>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.prompts.lock().unwrap().push(request.prompt);
            Ok(FimAnswer {
                text: "x".to_string(),
                usage: Default::default(),
                latency: Duration::from_millis(5),
            })
        })
    }
}

impl LspClient {
    /// One inline completion at the document start; returns once answered.
    async fn complete(&mut self, id: u64, version: i32) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "navigator/inlineCompletion",
            "params": {"uri": URI, "version": version, "line": 0, "character": 4}
        }))
        .await;
        tokio::time::advance(Duration::from_millis(151)).await;
        // Skip notifications (idle-timer status) until the response.
        loop {
            let resp = self.recv().await;
            if resp["id"] == id {
                return;
            }
        }
    }
}

fn driver_message(
    session_id: ulid::Ulid,
    seq: u64,
    content: &str,
) -> rust_agent::bus::event::EventEnvelope {
    rust_agent::bus::event::EventEnvelope::new(
        session_id,
        seq,
        rust_agent::bus::event::EventKind::MessageReceived,
        json!({
            "command_id": ulid::Ulid::new().to_string(),
            "source": "user:http",
            "target": "agent:main",
            "mode": "next-turn",
            "content": content,
        }),
    )
}

/// The first completion serves the intent summary; once the refresh has
/// completed the next one carries the refreshed text. Inside a minute with
/// unchanged inputs the model is not called again; a changed conversation
/// digest after a minute refreshes again; a changed digest inside a minute
/// does not.
#[tokio::test(start_paused = true)]
async fn reflex_context_is_refreshed_by_the_metacognition() {
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let feed = navigator::ConversationFeed::new();
    let session_id = ulid::Ulid::new();
    feed.register_session(
        session_id,
        Arc::new(|_| Box::pin(async { Ok::<(), String>(()) })),
    );
    feed.observe(&driver_message(session_id, 1, "use anyhow for errors"))
        .await;

    let model = ScriptedReflexContext::new(vec![Ok("REFRESHED-ONE"), Ok("REFRESHED-TWO")]);
    let fim = RecordingFim::default();
    let deps = NavigatorDeps::new(Arc::new(MockCommenter::new(vec![])), log_path.clone())
        .with_fim(Arc::new(fim.clone()))
        .with_reflex_context(Arc::new(model.clone()))
        .with_model_ids(None, None, Some("fast-model".into()))
        .with_conversation(feed.clone());
    let mut client = spawn_server(deps);

    client.initialize(json!({})).await;
    client
        .notify("navigator/intent", json!({"text": "fix the parser"}))
        .await;
    client.did_open(URI, &base_text(), 1).await;

    // 1. Intent summary first; the refresh starts in the background.
    client.complete(21, 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(model.calls(), 1);
    {
        let prompts = fim.prompts.lock().unwrap();
        assert!(prompts[0].contains("fix the parser"), "{}", prompts[0]);
        assert!(!prompts[0].contains("REFRESHED-ONE"));
    }
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests[0].intent, "fix the parser");
        assert!(requests[0].conversation.contains("use anyhow for errors"));
        assert_eq!(requests[0].file, URI);
        assert_eq!(requests[0].language_id, "rust");
        assert!(requests[0].excerpt.contains("   1 | fn main() {"));
    }

    // 2. The refreshed text is served; same inputs inside a minute: no call.
    client.complete(22, 1).await;
    assert!(fim.prompts.lock().unwrap()[1].contains("REFRESHED-ONE"));
    assert_eq!(model.calls(), 1);

    // 3. A changed digest inside a minute does not refresh.
    feed.observe(&driver_message(session_id, 2, "and add a --force flag"))
        .await;
    client.complete(23, 1).await;
    assert_eq!(model.calls(), 1);

    // 4. After a minute, with the digest changed, it refreshes.
    tokio::time::advance(Duration::from_secs(61)).await;
    client.complete(24, 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(model.calls(), 2);
    assert!(model.requests.lock().unwrap()[1]
        .conversation
        .contains("and add a --force flag"));
    client.complete(25, 1).await;
    assert!(fim.prompts.lock().unwrap()[4].contains("REFRESHED-TWO"));

    // 5. Unchanged inputs after another minute: still no call.
    tokio::time::advance(Duration::from_secs(61)).await;
    client.complete(26, 1).await;
    assert_eq!(model.calls(), 2);

    // The refresh calls are logged against the metacognition.
    let records = std::fs::read_to_string(log_path).unwrap();
    let calls: Vec<Value> = records
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .filter(|v: &Value| v["kind"] == "call" && v["purpose"] == "reflex_context")
        .collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["layer"], "metacognition");
    assert_eq!(calls[0]["model"], "fast-model");
}

/// A failed refresh keeps the previous context (here: the intent summary),
/// logs the error, and is retried only after the minute.
#[tokio::test(start_paused = true)]
async fn reflex_context_refresh_failure_keeps_the_previous_context() {
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let model = ScriptedReflexContext::new(vec![Err("boom"), Ok("RECOVERED")]);
    let fim = RecordingFim::default();
    let deps = NavigatorDeps::new(Arc::new(MockCommenter::new(vec![])), log_path.clone())
        .with_fim(Arc::new(fim.clone()))
        .with_reflex_context(Arc::new(model.clone()));
    let mut client = spawn_server(deps);

    client.initialize(json!({})).await;
    client
        .notify("navigator/intent", json!({"text": "fix the parser"}))
        .await;
    client.did_open(URI, &base_text(), 1).await;

    client.complete(31, 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    client.complete(32, 1).await;
    {
        let prompts = fim.prompts.lock().unwrap();
        assert!(prompts[1].contains("fix the parser"), "{}", prompts[1]);
    }
    assert_eq!(model.calls(), 1, "no retry inside the minute");
    let records = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        records.contains("reflex context refresh") && records.contains("boom"),
        "the error is logged: {records}"
    );

    // After the minute the inputs are still "never computed": retried.
    tokio::time::advance(Duration::from_secs(61)).await;
    client.complete(33, 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    client.complete(34, 1).await;
    assert_eq!(model.calls(), 2);
    assert!(fim.prompts.lock().unwrap()[3].contains("RECOVERED"));
}
