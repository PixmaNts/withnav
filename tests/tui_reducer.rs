//! Reducer tests over JSONL fixtures built from the crate's real event types.
//! Regenerate the fixtures with
//! `cargo test --test tui_reducer -- --ignored regenerate_fixtures`.
use navai::{
    bus::event::*,
    provider::{AssistantMessage, StreamDelta, ToolCall, Usage},
    tools::ToolStatus,
    tui::{
        protocol::*,
        reducer::{handle_action, update},
        state::*,
    },
};
use serde::Serialize;
use serde_json::{json, Value};
use ulid::Ulid;

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tui");

// ---------------------------------------------------------------- fixture generator

struct Script {
    seq: u64,
    out: Vec<EventEnvelope>,
}
impl Script {
    fn new() -> Self {
        Self {
            seq: 0,
            out: vec![],
        }
    }
    fn raw(&mut self, kind: &str, payload: Value) -> &mut Self {
        self.out.push(EventEnvelope {
            schema_version: 1,
            event_id: Ulid(1000 + self.seq as u128),
            seq: self.seq,
            session_id: Ulid(1),
            actor_id: ROOT_ACTOR_ID.into(),
            kind: kind.into(),
            occurred_at: "2026-10-02T00:00:00.000Z".into(),
            causation_id: None,
            correlation_id: None,
            payload,
        });
        self.seq += 1;
        self
    }
    fn push<T: Serialize>(&mut self, kind: EventKind, p: &T) -> &mut Self {
        self.raw(kind.as_str(), serde_json::to_value(p).unwrap())
    }
    fn start(&mut self) -> &mut Self {
        self.push(
            EventKind::SessionStart,
            &SessionStart {
                workspace: "/ws".into(),
                root_actor_id: ROOT_ACTOR_ID.into(),
            },
        )
    }
    fn user(&mut self, text: &str) -> &mut Self {
        self.push(
            EventKind::MessageReceived,
            &MessageReceived {
                command_id: Ulid(7),
                source: "user:http".into(),
                target: ROOT_ACTOR_ID.into(),
                mode: "next-turn".into(),
                content: text.into(),
            },
        )
    }
    fn turn_start(&mut self, turn: u128) -> &mut Self {
        self.push(
            EventKind::TurnStart,
            &TurnStart {
                turn_id: Ulid(turn),
                actor_id: ROOT_ACTOR_ID.into(),
                task_id: Ulid(9),
            },
        )
    }
    fn turn_end(&mut self, turn: u128, outcome: TurnEndOutcome, usage: Option<Usage>) -> &mut Self {
        self.push(
            EventKind::TurnEnd,
            &TurnEnd {
                turn_id: Ulid(turn),
                actor_id: ROOT_ACTOR_ID.into(),
                outcome,
                usage,
                duration_ms: 5,
                error: None,
            },
        )
    }
    fn delta(&mut self, turn: u128, step: u128, delta: StreamDelta) -> &mut Self {
        self.push(
            EventKind::LlmDelta,
            &LlmDelta {
                turn_id: Ulid(turn),
                step_id: Ulid(step),
                delta,
            },
        )
    }
    fn response(
        &mut self,
        turn: u128,
        step: u128,
        text: &str,
        thinking: &str,
        calls: Vec<ToolCall>,
        usage: Option<Usage>,
    ) -> &mut Self {
        self.push(
            EventKind::LlmResponse,
            &LlmResponse {
                turn_id: Ulid(turn),
                step_id: Ulid(step),
                message: AssistantMessage {
                    text: text.into(),
                    thinking: thinking.into(),
                    tool_calls: calls,
                    finish_reason: None,
                    usage,
                },
                usage,
                finish_reason: None,
            },
        )
    }
    fn pre(&mut self, id: &str, name: &str, args: Value) -> &mut Self {
        self.push(
            EventKind::ToolPreCall,
            &ToolPreCall {
                tool_call_id: id.into(),
                name: name.into(),
                source: ToolSource::Native,
                args,
                actor_id: ROOT_ACTOR_ID.into(),
            },
        )
    }
    fn post(
        &mut self,
        id: &str,
        name: &str,
        status: ToolStatus,
        out: &str,
        meta: Value,
    ) -> &mut Self {
        self.push(
            EventKind::ToolPostCall,
            &ToolPostCall {
                tool_call_id: id.into(),
                name: name.into(),
                status,
                output: out.into(),
                metadata: meta,
            },
        )
    }
}

fn usage(p: u64, c: u64) -> Usage {
    Usage {
        prompt_tokens: p,
        completion_tokens: c,
        total_tokens: p + c,
    }
}
fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        args,
        args_valid: true,
    }
}
fn text(t: &str) -> StreamDelta {
    StreamDelta::Text { text: t.into() }
}
fn thinking(t: &str) -> StreamDelta {
    StreamDelta::Thinking { text: t.into() }
}
fn without_deltas(v: &[EventEnvelope]) -> Vec<EventEnvelope> {
    v.iter()
        .filter(|e| e.kind != "llm.delta")
        .cloned()
        .collect()
}

fn turn_live() -> Vec<EventEnvelope> {
    let mut s = Script::new();
    s.start().user("hello").turn_start(100);
    s.delta(100, 201, thinking("Let me "))
        .delta(100, 201, thinking("think."));
    s.delta(100, 201, text("Hi "))
        .delta(100, 201, text("there"));
    s.response(
        100,
        201,
        "Hi there",
        "Let me think.",
        vec![],
        Some(usage(10, 5)),
    );
    s.turn_end(100, TurnEndOutcome::Done, Some(usage(10, 5)));
    s.out
}

fn tools_live() -> Vec<EventEnvelope> {
    let mut s = Script::new();
    s.start().user("fix it").turn_start(100);
    s.delta(100, 201, thinking("plan"));
    s.delta(
        100,
        201,
        StreamDelta::ToolCallStarted {
            index: 0,
            name: "read".into(),
        },
    );
    s.delta(
        100,
        201,
        StreamDelta::ToolCallArgs {
            index: 0,
            fragment: "{\"path\":\"a".into(),
        },
    );
    s.delta(
        100,
        201,
        StreamDelta::ToolCallArgs {
            index: 0,
            fragment: ".rs\"}".into(),
        },
    );
    s.delta(
        100,
        201,
        StreamDelta::ToolCallStarted {
            index: 1,
            name: "bash".into(),
        },
    );
    s.delta(
        100,
        201,
        StreamDelta::ToolCallArgs {
            index: 1,
            fragment: "{\"command\":\"ls\"}".into(),
        },
    );
    s.response(
        100,
        201,
        "Reading.",
        "plan",
        vec![
            call("c1", "read", json!({"path":"a.rs"})),
            call("c2", "bash", json!({"command":"ls"})),
        ],
        Some(usage(100, 20)),
    );
    s.pre("c1", "read", json!({"path":"a.rs"}));
    s.post(
        "c1",
        "read",
        ToolStatus::Ok,
        "fn main(){}",
        json!({"duration_ms":18}),
    );
    s.pre("c2", "bash", json!({"command":"ls"}));
    s.post(
        "c2",
        "bash",
        ToolStatus::Error,
        "boom",
        json!({"exit_code":101,"duration_ms":1420}),
    );
    s.delta(100, 202, text("Fixed")).delta(100, 202, text("!"));
    s.response(100, 202, "Fixed!", "", vec![], Some(usage(150, 10)));
    s.turn_end(100, TurnEndOutcome::Done, Some(usage(150, 10)));
    s.out
}

fn tool_statuses() -> Vec<EventEnvelope> {
    let mut s = Script::new();
    s.start().user("go").turn_start(100);
    s.response(
        100,
        201,
        "",
        "",
        vec![
            call("ok", "read", json!({"path":"x"})),
            call("err", "bash", json!({"command":"false"})),
            call("den", "write", json!({"path":"/etc/x"})),
            call("int", "bash", json!({"command":"sleep 9"})),
        ],
        Some(usage(10, 1)),
    );
    for (id, name, args) in [
        ("ok", "read", json!({"path":"x"})),
        ("err", "bash", json!({"command":"false"})),
        ("den", "write", json!({"path":"/etc/x"})),
        ("int", "bash", json!({"command":"sleep 9"})),
    ] {
        s.pre(id, name, args);
    }
    s.post(
        "ok",
        "read",
        ToolStatus::Ok,
        "data",
        json!({"duration_ms":3}),
    );
    s.post(
        "err",
        "bash",
        ToolStatus::Error,
        "\u{1b}[31mfail\u{1b}[0m",
        json!({"exit_code":1,"duration_ms":2000}),
    );
    s.post("den", "write", ToolStatus::Denied, "denied", Value::Null);
    s.post("int", "bash", ToolStatus::Interrupted, "", json!({}));
    // post-call whose pre-call and response were never seen
    s.post(
        "orphan",
        "grep",
        ToolStatus::Ok,
        "match",
        json!({"duration_ms":1}),
    );
    s.response(100, 202, "after", "", vec![], Some(usage(20, 2)));
    s.turn_end(100, TurnEndOutcome::Done, Some(usage(20, 2)));
    s.out
}

fn interrupted_turn() -> Vec<EventEnvelope> {
    let mut s = Script::new();
    s.start().user("go").turn_start(100);
    s.response(
        100,
        201,
        "",
        "",
        vec![
            call("t1", "bash", json!({"command":"sleep 9"})),
            call("t2", "read", json!({"path":"x"})),
            call("t3", "write", json!({"path":"y"})),
        ],
        Some(usage(10, 1)),
    );
    s.pre("t1", "bash", json!({"command":"sleep 9"}));
    s.turn_end(100, TurnEndOutcome::Interrupted, None);
    s.out
}

fn yield_ask() -> Vec<EventEnvelope> {
    let mut s = Script::new();
    s.start().user("flash it").turn_start(100);
    s.response(
        100,
        201,
        "",
        "",
        vec![call("a1", "ask", json!({"question":"insert SD card"}))],
        Some(usage(10, 1)),
    );
    s.push(
        EventKind::AgentYield,
        &AgentYield {
            actor_id: ROOT_ACTOR_ID.into(),
            turn_id: Ulid(100),
            question: "insert SD card".into(),
            kind: Some("ask".into()),
            details: None,
        },
    );
    s.turn_end(100, TurnEndOutcome::Yielded, Some(usage(10, 1)));
    s.raw(
        "agent.state",
        json!({"actor_id":"agent:main","from":"running","to":"yielded"}),
    );
    s.user("done");
    s.turn_start(101);
    s.response(101, 202, "thanks", "", vec![], Some(usage(30, 2)));
    s.turn_end(101, TurnEndOutcome::Done, Some(usage(30, 2)));
    s.out
}

fn usage_fallback() -> Vec<EventEnvelope> {
    let mut s = Script::new();
    s.start().user("go").turn_start(100);
    s.response(
        100,
        201,
        "a",
        "",
        vec![call("c1", "read", json!({}))],
        Some(usage(100, 10)),
    );
    s.pre("c1", "read", json!({}));
    s.post("c1", "read", ToolStatus::Ok, "x", json!({}));
    s.response(100, 202, "b", "", vec![], None);
    s.turn_end(100, TurnEndOutcome::Done, Some(usage(150, 20)));
    s.out
}

fn usage_missing() -> Vec<EventEnvelope> {
    let mut s = Script::new();
    s.start().user("go").turn_start(100);
    s.response(100, 201, "a", "", vec![], None);
    s.turn_end(100, TurnEndOutcome::Done, None);
    s.out
}

fn unknown_kind() -> Vec<EventEnvelope> {
    let mut s = Script::new();
    s.start().user("hi");
    s.raw("future.kind", json!({"anything": [1, 2]}));
    s.raw("task.done", json!({}));
    s.user("again");
    s.out
}

fn stopped() -> Vec<EventEnvelope> {
    let mut s = Script::new();
    s.start().user("hi").turn_start(100);
    s.response(100, 201, "hello", "", vec![], Some(usage(10, 1)));
    s.turn_end(100, TurnEndOutcome::Done, Some(usage(10, 1)));
    s.push(
        EventKind::SessionEnd,
        &SessionEnd {
            reason: "stop".into(),
            final_seq: 5,
        },
    );
    s.out
}

fn resumed() -> Vec<EventEnvelope> {
    let mut s = Script::new();
    s.seq = 6;
    s.start().user("again").turn_start(102);
    s.response(102, 301, "welcome back", "", vec![], Some(usage(20, 2)));
    s.turn_end(102, TurnEndOutcome::Done, Some(usage(20, 2)));
    s.out
}

const QUOTA_MESSAGE: &str = "provider API error (status 429): Quota preflight blocked";

fn error_live() -> Vec<EventEnvelope> {
    let mut s = Script::new();
    s.start().user("hello").turn_start(100);
    s.push(
        EventKind::TurnEnd,
        &TurnEnd {
            turn_id: Ulid(100),
            actor_id: ROOT_ACTOR_ID.into(),
            outcome: TurnEndOutcome::Error,
            usage: None,
            duration_ms: 5,
            error: Some(TurnError {
                code: "provider".into(),
                message: format!("{QUOTA_MESSAGE}\n{}", "x".repeat(400)),
            }),
        },
    );
    s.out
}

fn scenarios() -> Vec<(&'static str, Vec<EventEnvelope>)> {
    let (t, l) = (turn_live(), tools_live());
    vec![
        ("turn_replay", without_deltas(&t)),
        ("turn_live", t),
        ("tools_replay", without_deltas(&l)),
        ("tools_live", l),
        ("error_replay", without_deltas(&error_live())),
        ("error_live", error_live()),
        ("tool_statuses", tool_statuses()),
        ("interrupted_turn", interrupted_turn()),
        ("yield_ask", yield_ask()),
        ("usage_fallback", usage_fallback()),
        ("usage_missing", usage_missing()),
        ("unknown_kind", unknown_kind()),
        ("stopped", stopped()),
        ("resumed", resumed()),
    ]
}
fn jsonl(v: &[EventEnvelope]) -> String {
    v.iter()
        .map(|e| serde_json::to_string(e).unwrap() + "\n")
        .collect()
}

#[test]
#[ignore = "rewrites tests/fixtures/tui/*.jsonl"]
fn regenerate_fixtures() {
    std::fs::create_dir_all(DIR).unwrap();
    for (name, envs) in scenarios() {
        std::fs::write(format!("{DIR}/{name}.jsonl"), jsonl(&envs)).unwrap();
    }
}

#[test]
fn committed_fixtures_match_the_generator() {
    for (name, envs) in scenarios() {
        let on_disk = std::fs::read_to_string(format!("{DIR}/{name}.jsonl")).unwrap();
        assert_eq!(on_disk, jsonl(&envs), "{name}.jsonl is stale; regenerate");
    }
}

// ---------------------------------------------------------------- helpers

fn load(name: &str) -> Vec<EventEnvelope> {
    std::fs::read_to_string(format!("{DIR}/{name}.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}
fn st() -> State {
    State {
        workspace: "/ws".into(),
        ..Default::default()
    }
}
fn envelope(s: &State, e: &EventEnvelope) -> ClientEvent {
    ClientEvent::Envelope {
        generation: s.generation.clone(),
        envelope: e.clone(),
    }
}
fn feed(s: &mut State, envs: &[EventEnvelope]) {
    for e in envs {
        let ev = envelope(s, e);
        update(s, ev).unwrap();
    }
}
fn run(name: &str) -> State {
    let mut s = st();
    feed(&mut s, &load(name));
    s
}
fn ui() -> UiState {
    UiState::default()
}
fn ids(s: &State) -> Vec<String> {
    s.conversation.blocks.iter().map(|b| b.id.clone()).collect()
}
#[derive(Debug, Clone)]
struct Tool {
    name: String,
    args: Value,
    status: Option<ToolStatus>,
    output: String,
    duration_ms: Option<u64>,
    exit_code: Option<i32>,
}
fn tool(s: &State, id: &str) -> Tool {
    let bid = format!("tool:{id}");
    match s
        .conversation
        .blocks
        .iter()
        .find(|b| b.id == bid)
        .map(|b| &b.kind)
    {
        Some(BlockKind::ToolCall {
            name,
            args,
            status,
            output,
            duration_ms,
            exit_code,
            ..
        }) => Tool {
            name: name.clone(),
            args: args.clone(),
            status: *status,
            output: output.clone(),
            duration_ms: *duration_ms,
            exit_code: *exit_code,
        },
        other => panic!("no tool block {bid}: {other:?}"),
    }
}
fn steps(s: &State) -> Vec<(String, String)> {
    s.conversation
        .blocks
        .iter()
        .filter_map(|b| match &b.kind {
            BlockKind::AssistantStep { text, thinking, .. } => {
                Some((text.clone(), thinking.clone()))
            }
            _ => None,
        })
        .collect()
}
fn with_session(s: &mut State, id: &str) {
    s.session = Some(SessionMetadata {
        session_id: id.into(),
        actor_state: "idle".into(),
        workspace: "/ws".into(),
        model: "a".into(),
        current_seq: 0,
    });
}
fn created(s: &State, id: &str) -> ClientEvent {
    ClientEvent::Created {
        generation: s.generation.clone(),
        result: Ok(CreateSessionResponse {
            session_id: id.into(),
            root_actor_id: "agent:main".into(),
            actor_state: "idle".into(),
        }),
    }
}
fn resumed_ev(s: &State, id: &str) -> ClientEvent {
    ClientEvent::Resumed {
        generation: s.generation.clone(),
        result: Ok(CreateSessionResponse {
            session_id: id.into(),
            root_actor_id: "agent:main".into(),
            actor_state: "idle".into(),
        }),
    }
}
fn catalog() -> Vec<ModelEntry> {
    vec![
        ModelEntry {
            alias: "a".into(),
            model: "vendor/a".into(),
            context_window: Some(1000),
            ..Default::default()
        },
        ModelEntry {
            alias: "b".into(),
            model: "vendor/b".into(),
            context_window: None,
            ..Default::default()
        },
    ]
}
fn resumed_now(s: &mut State, id: &str) -> Result<Vec<Effect>, ProtocolError> {
    let ev = resumed_ev(s, id);
    update(s, ev)
}
fn created_now(s: &mut State, id: &str) -> Result<Vec<Effect>, ProtocolError> {
    let ev = created(s, id);
    update(s, ev)
}
fn models_now(s: &mut State, default: Option<String>) -> Result<Vec<Effect>, ProtocolError> {
    let ev = ClientEvent::Models {
        generation: s.generation.clone(),
        result: Ok(ModelsBody {
            models: catalog(),
            default,
        }),
    };
    update(s, ev)
}
fn submitted_now(
    s: &mut State,
    revision: u64,
    result: Result<SubmitResponse, String>,
) -> Result<Vec<Effect>, ProtocolError> {
    let ev = ClientEvent::Submitted {
        generation: s.generation.clone(),
        revision,
        result,
    };
    update(s, ev)
}
fn mutate(envs: &mut [EventEnvelope], f: impl Fn(&mut EventEnvelope)) {
    envs.iter_mut().for_each(f);
}

// ---------------------------------------------------------------- plan section 6

#[test]
fn delta_then_response_does_not_duplicate() {
    let live = load("turn_live");
    let mut s = st();
    // up to and including the deltas: the draft grows
    feed(&mut s, &live[..7]);
    assert_eq!(steps(&s), vec![("Hi there".into(), "Let me think.".into())]);
    feed(&mut s, &live[7..]);
    assert_eq!(steps(&s), vec![("Hi there".into(), "Let me think.".into())]);
    assert_eq!(
        ids(&s),
        vec!["1".to_string(), format!("step:{}", Ulid(201))]
    );
}

#[test]
fn several_steps_and_tools_in_a_turn() {
    let s = run("tools_replay");
    assert_eq!(
        steps(&s),
        vec![
            ("Reading.".into(), "plan".into()),
            ("Fixed!".into(), "".into())
        ]
    );
    assert_eq!(tool(&s, "c1").status, Some(ToolStatus::Ok));
    assert_eq!(tool(&s, "c2").status, Some(ToolStatus::Error));
    let key = ("agent:main".to_string(), Ulid(100).to_string());
    assert_eq!(s.conversation.tools_by_turn[&key], vec!["c1", "c2"]);
    assert_eq!(s.conversation.blocks.len(), 5); // user, step1, c1, c2, step2
    assert_eq!(s.activity, Activity::Idle);
}

#[test]
fn replay_equals_live() {
    for (live, replay) in [("turn_live", "turn_replay"), ("tools_live", "tools_replay")] {
        let (l, r) = (run(live), run(replay));
        assert!(!l.conversation.blocks.is_empty());
        assert_eq!(
            l.conversation.blocks, r.conversation.blocks,
            "{live} vs {replay}"
        );
        assert_eq!(l.conversation.session_tokens, r.conversation.session_tokens);
        assert_eq!(l.activity, r.activity);
    }
}

#[test]
fn disconnect_mid_step_then_canonical_recovery() {
    let (live, replay) = (load("tools_live"), load("tools_replay"));
    let mut s = st();
    feed(&mut s, &live[..8]); // streaming, no llm.response yet
    assert_eq!(steps(&s), vec![("".into(), "plan".into())]);
    for ev in [
        ClientEvent::StreamDisconnected {
            generation: s.generation.clone(),
        },
        ClientEvent::Reconnecting {
            generation: s.generation.clone(),
        },
        ClientEvent::StreamConnected {
            generation: s.generation.clone(),
        },
    ] {
        update(&mut s, ev).unwrap();
    }
    assert_eq!(s.connection, ConnectionStatus::Connected);
    let cursor = s.last_applied_seq.unwrap();
    let rest: Vec<_> = replay.iter().filter(|e| e.seq > cursor).cloned().collect();
    feed(&mut s, &rest);
    assert_eq!(
        s.conversation.blocks,
        run("tools_replay").conversation.blocks
    );
}

#[test]
fn duplicate_seqs_cursor_zero_and_gaps() {
    let mut s = st();
    assert_eq!(s.last_applied_seq, None);
    let replay = load("tools_replay"); // has gaps: llm.delta seqs are absent
    feed(&mut s, &replay[..1]);
    assert_eq!(s.last_applied_seq, Some(0)); // seq 0 is a real cursor
    feed(&mut s, &replay[..1]); // duplicate
    feed(&mut s, &replay);
    feed(&mut s, &replay); // whole replay again
    let once = run("tools_replay");
    assert_eq!(s.conversation.blocks, once.conversation.blocks);
    assert_eq!(
        s.conversation.session_tokens,
        once.conversation.session_tokens
    );
    assert_eq!(s.last_applied_seq, Some(replay.last().unwrap().seq));
}

#[test]
fn resync_resets_projection_and_stale_generation_is_dropped() {
    let mut s = run("tools_replay");
    let before = s.conversation.blocks.clone();
    let gen = s.generation.clone();
    let effects = update(
        &mut s,
        ClientEvent::Resync {
            generation: gen.clone(),
        },
    )
    .unwrap();
    assert_eq!(
        effects,
        vec![Effect::Subscribe {
            session_id: String::new(),
            cursor: None
        }]
    );
    assert!(s.conversation.blocks.is_empty());
    assert_eq!(s.last_applied_seq, None);
    feed(&mut s, &load("tools_replay"));
    assert_eq!(s.conversation.blocks, before);
    // an envelope of an obsolete generation changes nothing
    let stale = ClientEvent::Envelope {
        generation: Generation(99),
        envelope: load("resumed")[1].clone(),
    };
    assert!(update(&mut s, stale).unwrap().is_empty());
    assert_eq!(s.conversation.blocks, before);
}

#[test]
fn tool_ok_error_denied_interrupted_missing_pre_call_exit_code_duration() {
    let s = run("tool_statuses");
    let ok = tool(&s, "ok");
    assert_eq!(
        (ok.status, ok.duration_ms, ok.exit_code, ok.output.as_str()),
        (Some(ToolStatus::Ok), Some(3), None, "data")
    );
    let err = tool(&s, "err");
    assert_eq!(
        (err.status, err.duration_ms, err.exit_code),
        (Some(ToolStatus::Error), Some(2000), Some(1))
    );
    assert_eq!(err.output, "fail"); // escape sequences stripped
    assert_eq!(tool(&s, "den").status, Some(ToolStatus::Denied));
    assert_eq!(tool(&s, "den").duration_ms, None);
    assert_eq!(tool(&s, "int").status, Some(ToolStatus::Interrupted));
    let orphan = tool(&s, "orphan");
    assert_eq!(
        (orphan.status, orphan.args, orphan.name.as_str()),
        (Some(ToolStatus::Ok), Value::Null, "grep")
    );
}

#[test]
fn yield_and_answer_including_ask() {
    let live = load("yield_ask");
    let mut s = st();
    feed(&mut s, &live[..6]); // through turn.end yielded
    assert_eq!(s.activity, Activity::AwaitingUser);
    assert!(s
        .conversation
        .blocks
        .iter()
        .any(|b| b.kind == BlockKind::YieldQuestion("insert SD card".into())));
    // the ask call had no pre/post pair: it is not left "running"
    assert_eq!(tool(&s, "a1").status, Some(ToolStatus::Ok));
    feed(&mut s, &live[6..]);
    assert_eq!(s.activity, Activity::Idle);
    assert!(s
        .conversation
        .blocks
        .iter()
        .any(|b| matches!(&b.kind, BlockKind::Message { text, .. } if text == "done")));
    assert_eq!(steps(&s).last().unwrap().0, "thanks");
}

#[test]
fn usage_once_per_step_and_final_usage_not_doubled() {
    let s = run("turn_replay");
    assert_eq!(s.conversation.session_tokens, Some(15)); // turn.end repeats the response usage
    assert_eq!(s.conversation.context_tokens, Some(10));
    assert_eq!(s.conversation.completion_tokens, Some(5));
    let s = run("tools_replay");
    assert_eq!(s.conversation.session_tokens, Some(120 + 160));
    assert_eq!(s.conversation.context_tokens, Some(150));
    assert!(!s.conversation.missing_usage);
}

#[test]
fn missing_usage_and_context_window_stay_unknown() {
    let s = run("usage_missing");
    assert_eq!(s.conversation.session_tokens, None);
    assert_eq!(s.conversation.context_tokens, None);
    assert!(s.conversation.missing_usage);
    let mut s = st();
    models_now(&mut s, Some("b".into())).unwrap();
    assert_eq!(s.selected_model_alias.as_deref(), Some("b"));
    assert_eq!(s.context_window, None);
}

#[test]
fn unknown_kinds_are_ignored_but_advance_the_cursor() {
    let s = run("unknown_kind");
    assert_eq!(s.last_applied_seq, Some(4));
    assert_eq!(s.conversation.blocks.len(), 2);
    assert!(s.last_protocol_error.is_none());
}

#[test]
fn malformed_payload_and_schema_mismatch_are_protocol_errors() {
    let mut envs = load("turn_replay");
    let mut s = st();
    let user = envs.remove(1);
    let mut bad = user.clone();
    bad.payload = json!({"source": 5});
    let ev = envelope(&s, &bad);
    assert!(update(&mut s, ev).is_err());
    let mut bad = envs[1].clone(); // turn.start
    bad.kind = "llm.response".into();
    bad.payload = json!({"nope": 1});
    let ev = envelope(&s, &bad);
    assert!(update(&mut s, ev).is_err());
    let mut bad = user;
    bad.schema_version = 2;
    let ev = envelope(&s, &bad);
    assert_eq!(update(&mut s, ev).unwrap_err().0, "unsupported schema 2");
}

#[test]
fn stop_then_resume() {
    let mut s = run("stopped");
    with_session(&mut s, "S1");
    assert_eq!(s.activity, Activity::Stopped);
    // submitting after Stop resumes first, then subscribes from the cursor, then sends
    let fx = handle_action(
        &mut s,
        &mut ui(),
        Action::SubmitAt {
            text: "again".into(),
            revision: 4,
        },
    );
    assert_eq!(fx, vec![Effect::Resume("S1".into())]);
    let fx = resumed_now(&mut s, "S1").unwrap();
    assert_eq!(
        fx[1],
        Effect::Subscribe {
            session_id: "S1".into(),
            cursor: Some(5)
        }
    );
    assert!(matches!(&fx[2], Effect::SendAt { revision: 4, .. }));
    feed(&mut s, &load("resumed"));
    assert_eq!(s.activity, Activity::Idle);
    assert_eq!(steps(&s).len(), 2);
    assert_eq!(steps(&s)[1].0, "welcome back");
    assert_eq!(
        s.conversation
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Message { .. }))
            .count(),
        2
    );
}

// ---------------------------------------------------------------- regressions (bug numbers of the review)

#[test]
fn bug1_new_conversation_resets_cursor_and_bumps_generation() {
    let mut s = run("turn_replay");
    with_session(&mut s, "S1");
    let old = s.generation.clone();
    let fx = handle_action(&mut s, &mut ui(), Action::NewConversation);
    assert_eq!(fx, vec![Effect::Unsubscribe]);
    assert_eq!(
        (s.last_applied_seq, s.activity, s.session.is_none()),
        (None, Activity::Idle, true)
    );
    assert!(s.conversation.blocks.is_empty() && s.conversation.pending_tools.is_empty());
    assert_ne!(s.generation, old);
    // events in flight from the old stream are dropped
    let stale = ClientEvent::Envelope {
        generation: old,
        envelope: load("turn_replay")[1].clone(),
    };
    update(&mut s, stale).unwrap();
    assert!(s.conversation.blocks.is_empty());
    // the new conversation's seq 0 is applied
    feed(&mut s, &load("turn_replay"));
    assert_eq!(s.last_applied_seq, Some(8));
    assert!(!s.conversation.blocks.is_empty());
}

#[test]
fn bug1_pick_model_with_a_session_starts_a_conversation_without_one_it_does_not() {
    let mut s = run("turn_replay");
    models_now(&mut s, None).unwrap();
    let gen = s.generation.clone();
    let fx = handle_action(&mut s, &mut ui(), Action::PickModel("b".into()));
    assert!(fx.is_empty());
    assert_eq!(
        (s.generation.clone(), s.conversation.blocks.is_empty()),
        (gen.clone(), false)
    ); // no session: untouched
    with_session(&mut s, "S1");
    let fx = handle_action(&mut s, &mut ui(), Action::PickModel("a".into()));
    assert_eq!(fx, vec![Effect::Unsubscribe]);
    assert_ne!(s.generation, gen);
    assert_eq!(s.last_applied_seq, None);
    assert!(s.conversation.blocks.is_empty());
    assert_eq!(s.selected_model_alias.as_deref(), Some("a"));
}

#[test]
fn bug1_resume_other_id_resets_same_stopped_id_keeps_cursor() {
    let mut s = run("stopped");
    with_session(&mut s, "S1");
    let gen = s.generation.clone();
    let fx = handle_action(&mut s, &mut ui(), Action::ResumeSession("S1".into()));
    assert_eq!(fx, vec![Effect::Resume("S1".into())]);
    assert_eq!(
        (s.generation.clone(), s.last_applied_seq),
        (gen.clone(), Some(5))
    );
    assert!(!s.conversation.blocks.is_empty());
    let fx = resumed_now(&mut s, "S1").unwrap();
    assert!(fx.contains(&Effect::Subscribe {
        session_id: "S1".into(),
        cursor: Some(5)
    }));
    // another id: fresh start
    let fx = handle_action(&mut s, &mut ui(), Action::ResumeSession("S2".into()));
    assert_eq!(fx, vec![Effect::Unsubscribe, Effect::Resume("S2".into())]);
    assert_ne!(s.generation, gen);
    assert_eq!(s.last_applied_seq, None);
    let fx = resumed_now(&mut s, "S2").unwrap();
    assert!(fx.contains(&Effect::Subscribe {
        session_id: "S2".into(),
        cursor: None
    }));
}

#[test]
fn bug2_resync_keeps_catalog_model_sessions_pending_submit_and_notice() {
    let mut s = run("turn_replay");
    with_session(&mut s, "S1");
    s.catalog = catalog();
    s.selected_model_alias = Some("a".into());
    s.sessions = vec![SessionListEntry {
        id: "S0".into(),
        title: None,
        created_at: "t".into(),
        updated_at: "t".into(),
        message_count: 1,
        live: false,
    }];
    s.pending_submit_text = Some("draft".into());
    s.notice = Some("note".into());
    s.workspace = "/ws".into();
    let gen = s.generation.clone();
    let fx = update(
        &mut s,
        ClientEvent::Resync {
            generation: gen.clone(),
        },
    )
    .unwrap();
    assert_eq!(
        fx,
        vec![Effect::Subscribe {
            session_id: "S1".into(),
            cursor: None
        }]
    );
    assert_eq!(s.generation, gen); // same stream replays: no bump
    assert_eq!(s.catalog.len(), 2);
    assert_eq!(s.selected_model_alias.as_deref(), Some("a"));
    assert_eq!(s.sessions.len(), 1);
    assert_eq!(s.pending_submit_text.as_deref(), Some("draft"));
    assert_eq!(s.notice.as_deref(), Some("note"));
    assert_eq!(s.session.as_ref().unwrap().session_id, "S1");
    assert!(s.conversation.blocks.is_empty());
}

#[test]
fn bug3_create_and_resume_use_the_workspace() {
    let mut s = st();
    s.workspace = "/home/me/proj".into();
    let fx = handle_action(&mut s, &mut ui(), Action::Submit("hi".into()));
    match &fx[0] {
        Effect::Create(r) => assert_eq!(
            (r.workspace.as_str(), r.resume.as_deref()),
            ("/home/me/proj", None)
        ),
        other => panic!("{other:?}"),
    }
    created_now(&mut s, "S1").unwrap();
    assert_eq!(s.session.as_ref().unwrap().workspace, "/home/me/proj");
    let mut s2 = st();
    s2.workspace = "/home/me/proj".into();
    resumed_now(&mut s2, "S1").unwrap();
    assert_eq!(s2.session.as_ref().unwrap().workspace, "/home/me/proj");
}

#[test]
fn bug4_submit_revision_reaches_send_and_clear_editor() {
    let mut s = st();
    let fx = handle_action(
        &mut s,
        &mut ui(),
        Action::SubmitAt {
            text: "hi".into(),
            revision: 7,
        },
    );
    assert!(matches!(fx[0], Effect::Create(_)));
    let fx = created_now(&mut s, "S1").unwrap();
    let Effect::SendAt { request, revision } = fx.last().unwrap().clone() else {
        panic!("{fx:?}")
    };
    assert_eq!(revision, 7);
    assert_eq!(request.content.len(), 1);
    assert!(s.pending_submit_text.is_none());
    let ok = Ok(SubmitResponse {
        command_id: "c".into(),
        accepted: true,
        actor_state: "idle".into(),
        notice: None,
    });
    let fx = submitted_now(&mut s, 7, ok).unwrap();
    assert_eq!(fx, vec![Effect::ClearEditor { revision: 7 }]);
    // Submit(text) is SubmitAt with revision 0 and the reducer no longer invents revisions
    let fx = handle_action(&mut s, &mut ui(), Action::Submit("again".into()));
    assert!(matches!(fx[0], Effect::SendAt { revision: 0, .. }));
    assert_eq!(s.editor_revision, 0);
}

#[test]
fn bug5_post_call_keeps_args_and_name_and_exit_code_lands_on_its_own_block() {
    let s = run("tools_replay");
    let c2 = tool(&s, "c2");
    assert_eq!(c2.args, json!({"command":"ls"}));
    assert_eq!(c2.exit_code, Some(101));
    // c2 is not the last block (the final step follows) and c1 has no exit code
    assert!(matches!(
        s.conversation.blocks.last().unwrap().kind,
        BlockKind::AssistantStep { .. }
    ));
    assert_eq!(tool(&s, "c1").exit_code, None);
    // post-call without a name keeps the one already known
    let mut envs = load("tools_replay");
    for e in envs.iter_mut().filter(|e| e.kind == "tool.post_call") {
        e.payload.as_object_mut().unwrap().remove("name");
    }
    let mut s = st();
    feed(&mut s, &envs);
    assert_eq!(
        (tool(&s, "c1").name.as_str(), tool(&s, "c1").args),
        ("read", json!({"path":"a.rs"}))
    );
}

#[test]
fn bug6_interrupted_or_error_turn_settles_tools_and_planned_tools_do_not_run() {
    for outcome in ["interrupted", "error"] {
        let mut envs = load("interrupted_turn");
        envs.last_mut().unwrap().payload["outcome"] = json!(outcome);
        let mut s = st();
        feed(&mut s, &envs);
        for id in ["t1", "t2", "t3"] {
            assert_eq!(
                tool(&s, id).status,
                Some(ToolStatus::Interrupted),
                "{outcome} {id}"
            );
        }
        assert_eq!(s.activity, Activity::Idle);
    }
    // a done turn leaves a running tool alone but a never-pre-called plan is settled
    let mut envs = load("interrupted_turn");
    envs.last_mut().unwrap().payload["outcome"] = json!("done");
    let mut s = st();
    feed(&mut s, &envs);
    assert_eq!(tool(&s, "t1").status, None);
    assert_eq!(tool(&s, "t2").status, Some(ToolStatus::Interrupted));
}

#[test]
fn bug6_session_end_interrupts_unfinished_tools_and_tools_by_turn_uses_the_actor() {
    let mut envs = load("interrupted_turn");
    envs.pop(); // no turn.end
    let n = envs.len() as u64;
    let mut end = envs[0].clone();
    end.seq = n;
    end.kind = "session.end".into();
    end.payload = serde_json::to_value(SessionEnd {
        reason: "stop".into(),
        final_seq: n,
    })
    .unwrap();
    envs.push(end);
    let mut s = st();
    feed(&mut s, &envs);
    assert_eq!(s.activity, Activity::Stopped);
    assert_eq!(tool(&s, "t1").status, Some(ToolStatus::Interrupted));
    assert_eq!(tool(&s, "t3").status, Some(ToolStatus::Interrupted));
    // actor id comes from the envelope
    let mut envs = load("tools_replay");
    mutate(&mut envs, |e| e.actor_id = "agent:sub".into());
    let mut s = st();
    feed(&mut s, &envs);
    let keys: Vec<_> = s.conversation.tools_by_turn.keys().cloned().collect();
    assert_eq!(keys, vec![("agent:sub".to_string(), Ulid(100).to_string())]);
}

#[test]
fn bug7_local_block_ids_are_unique_and_stable() {
    let mut s = st();
    let g = s.generation.clone();
    for _ in 0..2 {
        update(
            &mut s,
            ClientEvent::Submitted {
                generation: g.clone(),
                revision: 1,
                result: Err("boom".into()),
            },
        )
        .unwrap();
        update(
            &mut s,
            ClientEvent::DeliveryUncertain {
                generation: g.clone(),
            },
        )
        .unwrap();
        let notice = Ok(SubmitResponse {
            command_id: "c".into(),
            accepted: true,
            actor_state: "idle".into(),
            notice: Some("downgraded".into()),
        });
        update(
            &mut s,
            ClientEvent::Submitted {
                generation: g.clone(),
                revision: 1,
                result: notice,
            },
        )
        .unwrap();
    }
    let all = ids(&s);
    assert_eq!(all.len(), 6);
    let mut dedup = all.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(dedup.len(), 6);
    assert!(all.iter().all(|i| i.starts_with("local:")));
    assert_eq!(all[0], "local:1");
    // the counter survives a new conversation
    handle_action(&mut s, &mut ui(), Action::NewConversation);
    let g = s.generation.clone();
    update(&mut s, ClientEvent::DeliveryUncertain { generation: g }).unwrap();
    assert_eq!(ids(&s), vec!["local:7"]);
}

#[test]
fn bug8_turn_end_usage_fallback_works_on_replay() {
    // the last response of the turn reported no usage; turn.end carries it
    let s = run("usage_fallback");
    assert!(s.conversation.current_step.is_none()); // no deltas on replay
    assert_eq!(s.conversation.session_tokens, Some(110 + 170));
    assert_eq!(s.conversation.context_tokens, Some(150));
    assert_eq!(s.conversation.completion_tokens, Some(20));
    assert!(!s.conversation.missing_usage);
}

#[test]
fn bug9_metadata_refreshes_display_and_stale_metadata_cannot_clobber_a_pick() {
    let mut s = st();
    models_now(&mut s, Some("a".into())).unwrap();
    assert_eq!(
        (s.selected_model_name.as_deref(), s.context_window),
        (Some("vendor/a"), Some(1000))
    );
    with_session(&mut s, "S1");
    let old = s.generation.clone();
    handle_action(&mut s, &mut ui(), Action::PickModel("b".into()));
    let meta = |g: &Generation, model: &str| ClientEvent::Metadata {
        generation: g.clone(),
        result: Ok(SessionMetadata {
            session_id: "S1".into(),
            actor_state: "idle".into(),
            workspace: "/ws".into(),
            model: model.into(),
            current_seq: 3,
        }),
    };
    update(&mut s, meta(&old, "a")).unwrap(); // stale: the old conversation's metadata
    assert_eq!(
        (
            s.selected_model_alias.as_deref(),
            s.selected_model_name.as_deref(),
            s.context_window
        ),
        (Some("b"), Some("vendor/b"), None)
    );
    assert!(s.session.is_none());
    // current metadata refreshes name and window from the catalog
    let g = s.generation.clone();
    update(&mut s, meta(&g, "a")).unwrap();
    assert_eq!(
        (
            s.selected_model_alias.as_deref(),
            s.selected_model_name.as_deref(),
            s.context_window
        ),
        (Some("a"), Some("vendor/a"), Some(1000))
    );
}

#[test]
fn bug10_tool_fragments_are_keyed_by_index_and_never_rendered() {
    let live = load("tools_live");
    let mut s = st();
    feed(&mut s, &live[..9]); // through the last tool-call fragment
    let mut keys: Vec<_> = s
        .conversation
        .pending_tool_fragments
        .keys()
        .cloned()
        .collect();
    keys.sort();
    assert_eq!(keys, vec!["0", "1"]);
    assert_eq!(
        s.conversation.pending_tool_fragments["0"],
        "{\"path\":\"a.rs\"}"
    );
    assert!(!s
        .conversation
        .blocks
        .iter()
        .any(|b| matches!(b.kind, BlockKind::ToolCall { .. })));
    feed(&mut s, &live[9..10]); // llm.response
    assert!(s.conversation.pending_tool_fragments.is_empty());
    let tools: Vec<_> = ids(&s)
        .into_iter()
        .filter(|i| i.starts_with("tool:"))
        .collect();
    assert_eq!(tools, vec!["tool:c1", "tool:c2"]);
}

#[test]
fn failed_turn_shows_an_error_block_identically_live_and_on_replay() {
    let (live, replay) = (run("error_live"), run("error_replay"));
    assert_eq!(live.conversation.blocks, replay.conversation.blocks);
    assert_eq!(live.activity, Activity::Idle);
    let Some(Block {
        kind: BlockKind::Error(text),
        ..
    }) = live.conversation.blocks.last()
    else {
        panic!("no error block: {:?}", live.conversation.blocks);
    };
    assert!(
        text.starts_with(&format!("provider: {QUOTA_MESSAGE}")),
        "{text}"
    );
    assert!(!text.contains('\n'));
    assert!(text.chars().count() <= 301, "{}", text.chars().count());
}

#[test]
fn interrupted_turn_pushes_a_short_notice_identically_live_and_on_replay() {
    let envs = load("interrupted_turn");
    let live = {
        let mut s = st();
        feed(&mut s, &envs);
        s
    };
    let replay = {
        let mut s = st();
        feed(&mut s, &without_deltas(&envs));
        s
    };
    assert_eq!(live.conversation.blocks, replay.conversation.blocks);
    assert_eq!(
        live.conversation.blocks.last().map(|b| b.kind.clone()),
        Some(BlockKind::Notice("turn interrupted".into()))
    );
}

#[test]
fn message_source_is_kept_as_the_actor_id() {
    let s = run("turn_live");
    assert!(matches!(
        &s.conversation.blocks[0].kind,
        BlockKind::Message { source, .. } if source == "user:http"
    ));
}

// ---------------------------------------------------------------- context gauge

fn snapshot(revision: u64, estimate_total: u64) -> ContextSnapshot {
    ContextSnapshot {
        alias: "default".into(),
        variant: None,
        window: 1_000_000,
        window_source: "models.json".into(),
        estimated: true,
        estimate_total,
        breakdown: Default::default(),
        output_reserve: 8192,
        margin: 50_000,
        input_budget: 941_808,
        percent: estimate_total * 100 / 1_000_000,
        last_usage: None,
        skills: Default::default(),
        rule_files: 0,
        incarnation: Ulid(1),
        revision,
    }
}

#[test]
fn context_updated_feeds_the_gauge_and_keeps_the_provider_count_apart() {
    let mut s = run("tools_replay");
    assert_eq!(s.conversation.context_tokens, Some(150));
    assert!(s.conversation.context.is_none());
    let mut script = Script::new();
    script.seq = 1_000;
    script.push(EventKind::ContextUpdated, &snapshot(2, 42_000));
    feed(&mut s, &script.out);
    assert_eq!(
        s.conversation.context.as_ref().unwrap().estimate_total,
        42_000
    );
    assert_eq!(s.conversation.context_tokens, Some(150));
}

#[test]
fn an_older_snapshot_never_replaces_a_newer_one() {
    let mut s = st();
    let mut script = Script::new();
    script.push(EventKind::ContextUpdated, &snapshot(5, 50_000));
    script.push(EventKind::ContextUpdated, &snapshot(4, 10_000));
    feed(&mut s, &script.out);
    assert_eq!(s.conversation.context.as_ref().unwrap().revision, 5);
    let ev = ClientEvent::Context {
        generation: s.generation.clone(),
        result: Ok(snapshot(6, 60_000)),
    };
    update(&mut s, ev).unwrap();
    assert_eq!(
        s.conversation.context.as_ref().unwrap().estimate_total,
        60_000
    );
    // An error (an older server has no endpoint) changes nothing.
    let ev = ClientEvent::Context {
        generation: s.generation.clone(),
        result: Err("HTTP 404".into()),
    };
    update(&mut s, ev).unwrap();
    assert_eq!(s.conversation.context.as_ref().unwrap().revision, 6);
}

#[test]
fn a_malformed_context_payload_is_a_protocol_error() {
    let mut s = st();
    let mut script = Script::new();
    script.raw("context.updated", json!({"window": "big"}));
    let ev = envelope(&s, &script.out[0]);
    assert!(update(&mut s, ev).is_err());
}

#[test]
fn every_stream_connection_reads_the_context_back() {
    let mut s = st();
    models_now(&mut s, Some("a".into())).unwrap();
    // Before a session exists there is nothing to fetch.
    let connected = ClientEvent::StreamConnected {
        generation: s.generation.clone(),
    };
    assert!(update(&mut s, connected).unwrap().is_empty());
    let created = ClientEvent::Created {
        generation: s.generation.clone(),
        result: Ok(CreateSessionResponse {
            session_id: "s1".into(),
            root_actor_id: "agent:main".into(),
            actor_state: "idle".into(),
        }),
    };
    let effects = update(&mut s, created).unwrap();
    assert!(!effects.contains(&Effect::FetchContext("s1".into())));
    let connected = ClientEvent::StreamConnected {
        generation: s.generation.clone(),
    };
    let effects = update(&mut s, connected).unwrap();
    assert_eq!(effects, vec![Effect::FetchContext("s1".into())]);
}

#[test]
fn a_snapshot_missed_during_a_disconnect_is_read_back_on_reconnect() {
    let mut s = st();
    models_now(&mut s, Some("a".into())).unwrap();
    let created = ClientEvent::Created {
        generation: s.generation.clone(),
        result: Ok(CreateSessionResponse {
            session_id: "s1".into(),
            root_actor_id: "agent:main".into(),
            actor_state: "idle".into(),
        }),
    };
    update(&mut s, created).unwrap();
    let mut script = Script::new();
    script.seq = 1_000;
    script.push(EventKind::ContextUpdated, &snapshot(2, 20_000));
    feed(&mut s, &script.out);
    assert_eq!(s.conversation.context.as_ref().unwrap().revision, 2);

    // The stream drops; revision 3 is published while nobody listens.
    for event in [
        ClientEvent::StreamDisconnected {
            generation: s.generation.clone(),
        },
        ClientEvent::Reconnecting {
            generation: s.generation.clone(),
        },
    ] {
        update(&mut s, event).unwrap();
    }
    let connected = ClientEvent::StreamConnected {
        generation: s.generation.clone(),
    };
    let effects = update(&mut s, connected).unwrap();
    assert_eq!(effects, vec![Effect::FetchContext("s1".into())]);
    let fetched = ClientEvent::Context {
        generation: s.generation.clone(),
        result: Ok(snapshot(3, 30_000)),
    };
    update(&mut s, fetched).unwrap();
    let held = s.conversation.context.as_ref().unwrap();
    assert_eq!((held.revision, held.estimate_total), (3, 30_000));
}

#[test]
fn a_resync_keeps_the_gauge_until_the_reconnect_refreshes_it() {
    let mut s = st();
    let held = ClientEvent::Context {
        generation: s.generation.clone(),
        result: Ok(snapshot(3, 30_000)),
    };
    update(&mut s, held).unwrap();
    let resync = ClientEvent::Resync {
        generation: s.generation.clone(),
    };
    update(&mut s, resync).unwrap();
    assert_eq!(s.conversation.context.as_ref().unwrap().revision, 3);
}

#[test]
fn a_resumed_session_s_snapshots_replace_the_previous_incarnations() {
    let mut s = st();
    let held = ClientEvent::Context {
        generation: s.generation.clone(),
        result: Ok(snapshot(40, 400_000)),
    };
    update(&mut s, held).unwrap();
    // Same id, stopped and resumed: a new actor starts its revisions again.
    let mut resumed = snapshot(0, 12_000);
    resumed.incarnation = Ulid(2);
    let fetched = ClientEvent::Context {
        generation: s.generation.clone(),
        result: Ok(resumed.clone()),
    };
    update(&mut s, fetched).unwrap();
    assert_eq!(s.conversation.context.as_ref(), Some(&resumed));
    // Within the new incarnation the revision order applies again.
    let mut script = Script::new();
    script.seq = 1_000;
    let mut next = snapshot(1, 13_000);
    next.incarnation = Ulid(2);
    let mut late = snapshot(0, 1);
    late.incarnation = Ulid(2);
    script.push(EventKind::ContextUpdated, &next);
    script.push(EventKind::ContextUpdated, &late);
    feed(&mut s, &script.out);
    assert_eq!(s.conversation.context.as_ref().unwrap().revision, 1);
}
