//! Measures the work a step does on the conversation transcript before a model request, and the
//! database work a step and a turn cause. A measurement, not a check: both tests are ignored and
//! print Markdown tables. Run them by hand, one at a time, on a release build:
//!
//! ```text
//! cargo test --release --test transcript_step_cost -- --ignored --nocapture --test-threads 1
//! ```
//!
//! Its own test binary: it installs a counting global allocator and a SQLite statement hook for
//! the whole process, which no other test may share.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::hint::black_box;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use withnav::agent::actor::AgentActor;
use withnav::provider::openai::ChatMessage;

mod common;
use common::server::{TestServerConfig, send_text};
use common::sse::{http_sse, sse_body, text_chunk, tool_call_chunk};

// ---------------------------------------------------------------------------------------------
// Heap allocations, counted only on a thread that asked for it.

struct Counting;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

thread_local! {
    // A const initializer without a destructor: reading it never allocates, so the allocator
    // can consult it.
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

fn note_allocation(size: usize) {
    if COUNTING.try_with(Cell::get).unwrap_or(false) {
        ALLOCATIONS.fetch_add(1, Relaxed);
        ALLOCATED_BYTES.fetch_add(size as u64, Relaxed);
    }
}

// SAFETY: every call is forwarded to the system allocator unchanged; the counters are atomics.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note_allocation(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note_allocation(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note_allocation(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Allocations (count, bytes requested) made by `f` on this thread.
fn allocations_of<T>(f: impl FnOnce() -> T) -> (T, u64, u64) {
    let (count, bytes) = (ALLOCATIONS.load(Relaxed), ALLOCATED_BYTES.load(Relaxed));
    COUNTING.with(|c| c.set(true));
    let out = black_box(f());
    COUNTING.with(|c| c.set(false));
    (
        out,
        ALLOCATIONS.load(Relaxed) - count,
        ALLOCATED_BYTES.load(Relaxed) - bytes,
    )
}

// ---------------------------------------------------------------------------------------------
// Part A: the passes `run_loop` makes over the transcript before each request
// (src/agent/loop.rs, the block after "The safe boundary before a request").

const SECRETS: [(&str, &str); 3] = [
    ("API_TOKEN", "tok-4f9c1e7a2b8d6c30"),
    ("DB_PASSWORD", "pw-91d2e8a7c4b3f605"),
    ("SIGNING_KEY", "sk-0a7b3c9d1e5f2468"),
];

/// A tool result of about `bytes` bytes of source-like text; with `secrets`, every one of the
/// three secret values appears in it once.
fn tool_output(step: usize, bytes: usize, secrets: bool) -> String {
    let mut out = String::with_capacity(bytes + 128);
    let mut line = 0;
    while out.len() < bytes {
        line += 1;
        out.push_str(&format!(
            "{line:>5}\tlet value_{step}_{line} = compute(&input[{line}..], options.step({step}));\n"
        ));
    }
    if secrets {
        for (_, value) in SECRETS {
            out.push_str(&format!("token = \"{value}\"\n"));
        }
    }
    out
}

/// A conversation of one Captain message and `steps` steps, each an assistant message with a
/// short text and one `read` call, then a ~6.6 KB tool result: ~2 MB at 300 steps.
fn conversation(steps: usize, secrets: bool) -> Vec<ChatMessage> {
    let mut messages = vec![
        json!({"role": "user", "content": "Read the module files one by one and summarise them."}),
    ];
    for step in 0..steps {
        let id = format!("call_{step}");
        messages.push(json!({
            "role": "assistant",
            "content": format!("Reading file {step} next to follow the call chain."),
            "tool_calls": [{"id": id, "type": "function", "function": {
                "name": "read", "arguments": json!({"path": format!("src/module_{step}.rs")}).to_string()
            }}]
        }));
        messages.push(json!({"role": "tool", "tool_call_id": id, "content": tool_output(step, 6_600, secrets)}));
    }
    messages
        .into_iter()
        .map(|m| serde_json::from_value(m).expect("a ChatMessage"))
        .collect()
}

fn transcript_bytes(transcript: &[ChatMessage]) -> usize {
    serde_json::to_string(transcript)
        .expect("serializable")
        .len()
}

#[derive(Clone, Copy, PartialEq)]
enum Redaction {
    /// No secret access: the redaction pass does not run.
    Off,
    /// Three session secrets, none of which appears in the transcript.
    Absent,
    /// Three session secrets, each appearing in every tool result.
    Present,
}

impl Redaction {
    fn label(self) -> &'static str {
        match self {
            Redaction::Off => "no secrets",
            Redaction::Absent => "3 secrets, no match",
            Redaction::Present => "3 secrets, in every result",
        }
    }
}

fn actor(steps: usize, redaction: Redaction, workspace: &Path) -> AgentActor {
    use withnav::tools::{Registry, ToolContext};
    let mut registry = Registry::new();
    registry.register(Box::new(withnav::tools::native::ReadTool));
    registry.register(Box::new(withnav::tools::native::GrepTool));
    registry.register(Box::new(withnav::tools::bash::BashTool));
    registry.register(Box::new(withnav::tools::control::AskTool));
    let secrets = (redaction != Redaction::Off).then(|| {
        let store = Arc::new(withnav::secrets::SecretStore::in_memory());
        let owner = ulid::Ulid::generate();
        for (name, value) in SECRETS {
            store
                .put(
                    owner,
                    workspace,
                    name,
                    value,
                    withnav::secrets::SecretTtl::Session,
                    withnav::secrets::SecretScope::Session,
                )
                .expect("secret stored");
        }
        withnav::secrets::SecretAccess::new(store, owner, workspace)
    });
    let mut actor = AgentActor::new(
        registry,
        ToolContext {
            secrets,
            binding: None,
            workspace: workspace.to_path_buf(),
            writable_roots: vec![workspace.to_path_buf()],
            readable_roots: Vec::new(),
        },
        withnav::provider::config::ProviderConfig {
            base_url: "http://127.0.0.1:9/v1".to_string(),
            api_key_env: None,
            headers: Default::default(),
            connect_timeout_ms: 100,
            request_timeout_ms: 100,
            catalog_format: None,
        },
        "key".to_string(),
        "mock-model".to_string(),
        withnav::provider::config::ReasoningHistory::Preserve,
        Some(8_192),
        tokio_util::sync::CancellationToken::new(),
    );
    // A budget, so the estimate runs as it does for every model with a known window.
    actor.input_budget = Some(u64::MAX);
    actor.transcript = conversation(steps, redaction == Redaction::Present);
    actor
}

const PASSES: [&str; 6] = [
    "effective_history",
    "redaction",
    "image plan",
    "clone",
    "request_history",
    "schemas + estimate",
];

/// One pre-request pipeline, pass by pass as `run_loop` runs it. Returns each pass's duration.
fn pipeline(actor: &AgentActor) -> [Duration; 6] {
    let mut times = [Duration::ZERO; 6];
    let mut at = Instant::now();
    let mut lap = |i: usize| {
        let now = Instant::now();
        times[i] = now - at;
        at = now;
    };
    let role_message = actor.role_message();
    let mut transcript_history = actor.effective_history();
    lap(0);
    if let Some(secrets) = &actor.tool_context.secrets {
        transcript_history
            .iter_mut()
            .for_each(|m| secrets.redact_message(m));
    }
    lap(1);
    let image_plan = withnav::agent::images::plan(&transcript_history, actor.images);
    let image_ids = image_plan.send_ids(&transcript_history);
    lap(2);
    let copy = transcript_history.clone();
    lap(3);
    let history = actor.request_history(copy);
    lap(4);
    let tools = actor.effective_tool_schemas();
    let total = withnav::agent::context::estimate(
        role_message.as_deref(),
        actor.rules.message(),
        actor.effective_skills_catalog(),
        &history,
        &tools,
    )
    .total();
    lap(5);
    black_box((image_ids, history, total));
    times
}

/// The same passes, each under the allocation counter: (allocations, bytes) per pass.
fn pipeline_allocations(actor: &AgentActor) -> [(u64, u64); 6] {
    let mut out = [(0, 0); 6];
    let (mut transcript_history, n, b) = allocations_of(|| actor.effective_history());
    out[0] = (n, b);
    let ((), n, b) = allocations_of(|| {
        if let Some(secrets) = &actor.tool_context.secrets {
            transcript_history
                .iter_mut()
                .for_each(|m| secrets.redact_message(m));
        }
    });
    out[1] = (n, b);
    let (image_ids, n, b) = allocations_of(|| {
        withnav::agent::images::plan(&transcript_history, actor.images)
            .send_ids(&transcript_history)
    });
    out[2] = (n, b);
    let (copy, n, b) = allocations_of(|| transcript_history.clone());
    out[3] = (n, b);
    let (history, n, b) = allocations_of(|| actor.request_history(copy));
    out[4] = (n, b);
    let (total, n, b) = allocations_of(|| {
        let role_message = actor.role_message();
        let tools = actor.effective_tool_schemas();
        withnav::agent::context::estimate(
            role_message.as_deref(),
            actor.rules.message(),
            actor.effective_skills_catalog(),
            &history,
            &tools,
        )
        .total()
    });
    out[5] = (n, b);
    black_box((image_ids, total));
    out
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn percentile(mut values: Vec<f64>, p: f64) -> f64 {
    values.sort_by(f64::total_cmp);
    let rank = ((values.len() as f64 - 1.0) * p).round() as usize;
    values[rank]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1_000.0
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

const ITERATIONS: usize = 100;

#[test]
#[ignore = "manual measurement; see the module documentation"]
fn pre_request_pipeline_cost() {
    let tmp = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("tempdir");
    let workspace = std::fs::canonicalize(tmp.path()).expect("workspace");
    let mut timing = String::from(
        "| steps | transcript | redaction | effective_history | redaction | image plan | clone | request_history | schemas + estimate | whole (median) | whole (p95) |\n|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    let mut allocations = String::from(
        "| steps | redaction | pass | allocations | MiB allocated |\n|---:|---|---|---:|---:|\n",
    );
    for steps in [50, 100, 200, 300] {
        for redaction in [Redaction::Off, Redaction::Absent, Redaction::Present] {
            let actor = actor(steps, redaction, &workspace);
            let size = transcript_bytes(&actor.transcript);
            // Warm-up: first-touch page faults and lazy statics stay out of the numbers.
            pipeline(&actor);
            let runs: Vec<[Duration; 6]> = (0..ITERATIONS).map(|_| pipeline(&actor)).collect();
            let pass = |i: usize| median(runs.iter().map(|r| ms(r[i])).collect());
            let whole: Vec<f64> = runs.iter().map(|r| ms(r.iter().sum())).collect();
            timing.push_str(&format!(
                "| {steps} | {:.2} MiB | {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} |\n",
                mib(size as u64),
                redaction.label(),
                pass(0),
                pass(1),
                pass(2),
                pass(3),
                pass(4),
                pass(5),
                median(whole.clone()),
                percentile(whole, 0.95),
            ));
            if steps == 300 {
                let counted = pipeline_allocations(&actor);
                for (name, (count, bytes)) in PASSES.iter().zip(counted) {
                    allocations.push_str(&format!(
                        "| {steps} | {} | {name} | {count} | {:.2} |\n",
                        redaction.label(),
                        mib(bytes)
                    ));
                }
                let (count, bytes) = counted
                    .iter()
                    .fold((0, 0), |(n, b), (cn, cb)| (n + cn, b + cb));
                allocations.push_str(&format!(
                    "| {steps} | {} | **all passes** | {count} | {:.2} |\n",
                    redaction.label(),
                    mib(bytes)
                ));
            }
        }
    }
    println!(
        "\n### Pre-request passes, milliseconds (median of {ITERATIONS})\n\n{timing}\n### Heap allocations of one pre-request pipeline\n\n{allocations}"
    );
}

// ---------------------------------------------------------------------------------------------
// Part B: SQLite statements and pages written, per step and per turn, through the real server.

use rusqlite::ffi;

/// Statements and pages, by database: `history.db` and every other one.
#[derive(Default)]
struct Sql {
    reads: AtomicU64,
    writes: AtomicU64,
    transcript_updates: AtomicU64,
    transactions: AtomicU64,
    other_db: AtomicU64,
    /// Pages the history connections' pagers wrote (to the WAL, in WAL mode).
    history_pages: AtomicU64,
}

static SQL: Sql = Sql {
    reads: AtomicU64::new(0),
    writes: AtomicU64::new(0),
    transcript_updates: AtomicU64::new(0),
    transactions: AtomicU64::new(0),
    other_db: AtomicU64::new(0),
    history_pages: AtomicU64::new(0),
};

#[derive(Clone, Copy, Default, Debug)]
struct SqlCount {
    reads: u64,
    writes: u64,
    transcript_updates: u64,
    transactions: u64,
    other_db: u64,
    history_pages: u64,
}

impl SqlCount {
    fn now() -> Self {
        Self {
            reads: SQL.reads.load(Relaxed),
            writes: SQL.writes.load(Relaxed),
            transcript_updates: SQL.transcript_updates.load(Relaxed),
            transactions: SQL.transactions.load(Relaxed),
            other_db: SQL.other_db.load(Relaxed),
            history_pages: SQL.history_pages.load(Relaxed),
        }
    }

    fn since(self, before: Self) -> Self {
        Self {
            reads: self.reads - before.reads,
            writes: self.writes - before.writes,
            transcript_updates: self.transcript_updates - before.transcript_updates,
            transactions: self.transactions - before.transactions,
            other_db: self.other_db - before.other_db,
            history_pages: self.history_pages - before.history_pages,
        }
    }

    fn history_statements(self) -> u64 {
        self.reads + self.writes + self.transcript_updates + self.transactions
    }
}

thread_local! {
    /// Set while the test reads the database itself, so its own queries are not counted.
    static PROBING: Cell<bool> = const { Cell::new(false) };
}

/// `SQLITE_TRACE_PROFILE`: once per finished top-level statement, on the thread that ran it,
/// which owns the connection at that moment.
unsafe extern "C" fn on_statement(
    _mask: c_uint,
    _ctx: *mut c_void,
    statement: *mut c_void,
    _elapsed: *mut c_void,
) -> c_int {
    if PROBING.try_with(Cell::get).unwrap_or(false) {
        return 0;
    }
    let statement = statement.cast::<ffi::sqlite3_stmt>();
    // SAFETY: SQLite hands a live statement of a live connection to the callback.
    unsafe {
        let db = ffi::sqlite3_db_handle(statement);
        let file = ffi::sqlite3_db_filename(db, c"main".as_ptr());
        let history = !file.is_null() && CStr::from_ptr(file).to_bytes().ends_with(b"history.db");
        if !history {
            SQL.other_db.fetch_add(1, Relaxed);
            return 0;
        }
        let (mut pages, mut high) = (0, 0);
        ffi::sqlite3_db_status(
            db,
            ffi::SQLITE_DBSTATUS_CACHE_WRITE,
            &mut pages,
            &mut high,
            1,
        );
        SQL.history_pages.fetch_add(pages as u64, Relaxed);
        let sql = CStr::from_ptr(ffi::sqlite3_sql(statement)).to_bytes();
        let sql = sql.trim_ascii_start();
        let starts = |prefix: &[u8]| {
            sql.len() >= prefix.len() && sql[..prefix.len()].eq_ignore_ascii_case(prefix)
        };
        let counter = if ["BEGIN", "COMMIT", "END", "ROLLBACK", "SAVEPOINT", "RELEASE"]
            .iter()
            .any(|p| starts(p.as_bytes()))
        {
            &SQL.transactions
        } else if starts(b"UPDATE sessions SET transcript") {
            &SQL.transcript_updates
        } else if ffi::sqlite3_stmt_readonly(statement) != 0 {
            &SQL.reads
        } else {
            &SQL.writes
        };
        counter.fetch_add(1, Relaxed);
    }
    0
}

/// Installs [`on_statement`] on a connection being opened.
unsafe extern "C" fn trace_connection(
    db: *mut ffi::sqlite3,
    _error: *mut *mut c_char,
    _api: *const ffi::sqlite3_api_routines,
) -> c_int {
    // SAFETY: called by SQLite with the connection it is opening.
    unsafe {
        ffi::sqlite3_trace_v2(
            db,
            ffi::SQLITE_TRACE_PROFILE,
            Some(on_statement),
            std::ptr::null_mut(),
        )
    }
}

/// One model request as the scripted provider saw it.
struct Arrival {
    /// The request head was read: the client has dispatched the request.
    head: SystemTime,
    body_bytes: usize,
    /// Statements and pages up to the head's arrival.
    sql: SqlCount,
}

const TURNS: usize = 30;
/// Tool steps per turn; the turn's last request is answered with a text.
const TOOL_STEPS: usize = 9;

/// Serves the agent's requests over full HTTP/1.1 framing, without delay: within a turn, the
/// first `TOOL_STEPS` requests read file `<turn>_<step>.txt`, the next one ends the turn. A
/// request that carries no tools (a title, a summary) gets a short text and is not recorded.
async fn spawn_provider(ws: std::path::PathBuf, arrivals: Arc<Mutex<Vec<Arrival>>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock bind");
    let url = format!("http://{}/v1", listener.local_addr().expect("mock addr"));
    tokio::spawn(async move {
        let mut agent_requests = 0;
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = Vec::new();
            let mut chunk = vec![0u8; 64 * 1024];
            let mut head_at = None;
            let request = loop {
                let n = match sock.read(&mut chunk).await {
                    Ok(0) | Err(_) => break None,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&chunk[..n]);
                let Some(idx) = buf.windows(4).position(|b| b == b"\r\n\r\n") else {
                    continue;
                };
                let at = *head_at.get_or_insert_with(|| (SystemTime::now(), SqlCount::now()));
                let head = String::from_utf8_lossy(&buf[..idx]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if buf.len() >= idx + 4 + length {
                    break Some((at, String::from_utf8_lossy(&buf[idx + 4..]).into_owned()));
                }
            };
            let Some(((head, sql), body)) = request else {
                continue;
            };
            let response = if body.contains("\"tools\"") {
                let (turn, step) = (
                    agent_requests / (TOOL_STEPS + 1),
                    agent_requests % (TOOL_STEPS + 1),
                );
                agent_requests += 1;
                arrivals.lock().expect("arrivals").push(Arrival {
                    head,
                    body_bytes: body.len(),
                    sql,
                });
                if step < TOOL_STEPS {
                    tool_call_chunk(
                        &format!("call_{turn}_{step}"),
                        "read",
                        &json!({"path": ws.join(format!("{turn}_{step}.txt"))}),
                    )
                } else {
                    text_chunk(&format!("Turn {turn} done."))
                }
            } else {
                text_chunk("Reading files")
            };
            let _ = sock.write_all(&http_sse(&sse_body(&[&response]))).await;
            let _ = sock.flush().await;
            let _ = sock.shutdown().await;
        }
    });
    url
}

fn event_time(event: &withnav::bus::event::EventEnvelope) -> SystemTime {
    let at = time::OffsetDateTime::parse(
        &event.occurred_at,
        &time::format_description::well_known::Rfc3339,
    )
    .expect("an RFC 3339 event time");
    SystemTime::UNIX_EPOCH + Duration::from_nanos(at.unix_timestamp_nanos() as u64)
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual measurement; see the module documentation"]
async fn step_and_turn_cost_through_the_server() {
    // SAFETY: nothing has opened a database or read the environment yet in this test.
    unsafe {
        std::env::remove_var("WITHNAV_MAX_STEPS");
        rusqlite::auto_extension::register_auto_extension(trace_connection)
            .expect("statement hook registered");
    }
    let tmp = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    for turn in 0..TURNS {
        for step in 0..TOOL_STEPS {
            std::fs::write(
                ws.join(format!("{turn}_{step}.txt")),
                tool_output(turn * TOOL_STEPS + step, 7_200, false),
            )
            .unwrap();
        }
    }
    let arrivals = Arc::new(Mutex::new(Vec::new()));
    let url = spawn_provider(ws.clone(), arrivals.clone()).await;
    // A window far above the final ~2 MB prompt: the estimate runs at every step, compaction
    // never does.
    let state = TestServerConfig::mock(tmp.path(), &url)
        .chat_tools(50_000_000)
        .allow_everything()
        .state();
    let history_db = tmp.path().join("history.db");
    let id = state.create_session(&ws, None, false).await.unwrap();
    let mut live = state
        .subscribe(id, None)
        .await
        .unwrap()
        .live
        .expect("a live session");
    assert!(
        SqlCount::now().history_statements() > 0,
        "the statement hook saw no history.db statement"
    );

    let read_transcript_bytes = || -> u64 {
        PROBING.with(|p| p.set(true));
        let conn = rusqlite::Connection::open(&history_db).expect("history.db");
        let bytes: i64 = conn
            .query_row(
                "SELECT length(transcript) FROM sessions WHERE id = ?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .unwrap_or(0);
        drop(conn);
        PROBING.with(|p| p.set(false));
        bytes as u64
    };

    let mut steps = String::from(
        "| turn | request body | tool result -> request dispatched, ms (median / max) | history.db statements per step (median / max) |\n|---:|---:|---:|---:|\n",
    );
    let mut turns = String::from(
        "| turn | transcript | statements (read / write / transcript UPDATE / txn) | other DBs | pages written | bytes written to WAL | db + WAL on disk |\n|---:|---:|---|---:|---:|---:|---:|\n",
    );
    let mut all_latencies = Vec::new();
    let page_size = 4_096_u64;
    let mut total_pages = 0;
    for turn in 0..TURNS {
        let before = SqlCount::now();
        let mut tool_results = Vec::new();
        send_text(&state, id, "Continue reading the next files.").await;
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let event = tokio::time::timeout_at(deadline.into(), live.recv())
                .await
                .expect("the turn ends within 120 s")
                .expect("the session stays live");
            match event.kind.as_str() {
                "tool.post_call" => tool_results.push(event_time(&event)),
                "context.compacted" => panic!("the measurement must not compact"),
                "turn.end" => {
                    assert_eq!(event.payload["outcome"], "done", "{event:?}");
                    break;
                }
                _ => {}
            }
        }
        // The transcript is saved after the turn's last events: wait for its UPDATE.
        let saved = Instant::now() + Duration::from_secs(10);
        while SqlCount::now().transcript_updates == before.transcript_updates {
            assert!(
                Instant::now() < saved,
                "the transcript is saved after a turn"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        state.history().flush().await.expect("flush");
        let delta = SqlCount::now().since(before);
        total_pages += delta.history_pages;

        let arrivals = arrivals.lock().expect("arrivals");
        let base = turn * (TOOL_STEPS + 1);
        assert_eq!(
            arrivals.len(),
            base + TOOL_STEPS + 1,
            "one request per step"
        );
        assert_eq!(tool_results.len(), TOOL_STEPS, "one tool result per step");
        let latencies: Vec<f64> = (0..TOOL_STEPS)
            .map(|step| {
                let dispatched = arrivals[base + step + 1].head;
                ms(dispatched
                    .duration_since(tool_results[step])
                    .unwrap_or_default())
            })
            .collect();
        let statements: Vec<f64> = (0..TOOL_STEPS)
            .map(|step| {
                arrivals[base + step + 1]
                    .sql
                    .since(arrivals[base + step].sql)
                    .history_statements() as f64
            })
            .collect();
        all_latencies.extend(latencies.iter().copied());
        let body = arrivals[base + TOOL_STEPS].body_bytes as u64;
        drop(arrivals);
        steps.push_str(&format!(
            "| {} | {:.2} MiB | {:.0} / {:.0} | {:.0} / {:.0} |\n",
            turn + 1,
            mib(body),
            median(latencies.clone()),
            percentile(latencies, 1.0),
            median(statements.clone()),
            percentile(statements, 1.0),
        ));
        let transcript = read_transcript_bytes();
        let wal = history_db.with_extension("db-wal");
        turns.push_str(&format!(
            "| {} | {:.2} MiB | {} / {} / {} / {} | {} | {} | {:.2} MiB | {:.2} MiB |\n",
            turn + 1,
            mib(transcript),
            delta.reads,
            delta.writes,
            delta.transcript_updates,
            delta.transactions,
            delta.other_db,
            delta.history_pages,
            mib(delta.history_pages * (page_size + 24)),
            mib(file_size(&history_db) + file_size(&wal)),
        ));
    }
    PROBING.with(|p| p.set(true));
    let actual_page_size: i64 = rusqlite::Connection::open(&history_db)
        .unwrap()
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        actual_page_size as u64, page_size,
        "WAL bytes assume this page size"
    );
    println!(
        "\n### Per step (tool result event -> next request head at the provider; ms resolution)\n\nAll steps: median {:.0} ms, p95 {:.0} ms, max {:.0} ms.\n\n{steps}\n### Per turn ({TOOL_STEPS} tool steps + 1 final answer)\n\nPages are counted by SQLITE_DBSTATUS_CACHE_WRITE on history.db connections: pages the pager wrote to the WAL, at {page_size} B + a 24 B frame header each. Checkpoint copies into the main file are not included. Whole run: {total_pages} pages, {:.2} MiB written to the WAL.\n\n{turns}",
        median(all_latencies.clone()),
        percentile(all_latencies.clone(), 0.95),
        percentile(all_latencies, 1.0),
        mib(total_pages * (page_size + 24)),
    );
}
