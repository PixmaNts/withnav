# Navai — Layer-0 Meta-Plan (PLAN.md)

**Status:** Historical: the Phase 1 plan, kept for reference. It is not kept up to date.
For what the code does today read [`ARCHITECTURE.md`](../ARCHITECTURE.md) (module map and flows) and
[`CONTEXT.md`](../CONTEXT.md). The notes marked "Today" below correct the facts that changed.
**Schema version of this document:** 1
**Authoritative spec:** [`docs/SPEC.md`](SPEC.md) (the original design; each section carries a status line). This PLAN.md is the *engineering
anticipation* layer that freezes cross-cutting decisions so later phases add code without
rewriting earlier work. Where SPEC and PLAN disagree, SPEC wins on requirements; PLAN wins on
the concrete engineering choice that satisfies the requirement.

> This file is written FIRST (Wave 0, before any code) and is the frozen reference from commit 1.
> It freezes **wire semantics and interfaces**, not a blanket "Rust enums never change" promise.
> Phase-1 crate versions are pinned; Phase 2–4 crates are FORWARD DECLARATIONS whose *schema /
> interface* is frozen now but whose exact versions are re-pinned at implementation time.

---

## 1. Roadmap — the four vertical phases

Mirrors SPEC §28 as-corrected. Each phase is a demonstrable end-to-end slice; a phase is NOT
complete merely because types compile.

- **Phase 1 — Single-agent vertical slice.** One headless agent edits a Rust project through
  OmniRoute. Hand-rolled OpenAI SSE core, stream accumulator (text + `reasoning_content` +
  tool-calls), 5 native tools (`read`/`write`/`edit`/`bash`/`grep`; today also `glob` and `fetch`,
  plus `ask`, and `skill` per session), `ask` control tool,
  `Continue`/`Done`/`Yield` loop with cancellation, CLI `run` + `auth`. **This is the only phase
  decomposed to executable depth right now.**
- **Phase 2 — Bus, plugins, MCP, server.** Event envelopes + session sequencer + bounded
  subscribers; per-actor inboxes + command routing; move stdout rendering behind an event
  subscriber; Axum SSE/health API (the WebSocket route was removed; HTTP and SSE only); hook-plugin supervisor (JSON-RPC stdio, RFC-6902
  patches, deterministic ordering, timeouts, failure policies); MCP via `rmcp`.
- **Phase 3 — Non-blocking subagents + persistence.** Refactor the loop into a reusable actor
  constructor; `spawn_agent` (returns immediately); parent/child cancellation; event-sourced
  SQLite as a durable bus subscriber; pure replay reducers; interrupted-not-replayed resume.
- **Phase 4 — Personal usability layer.** Hierarchical `AGENTS.md`/`rules.md`, persona
  frontmatter (`agents/<name>.md`), `load_skill` discovery, `notify` hot-reload, the co-agent
  plugin, and a Ratatui client that consumes the server API.

**Strict scope fence (SPEC §29):** exactly four foundational primitives — (1) the core loop +
native control invariants, (2) the event bus + actor mailboxes, (3) external MCP/hook-plugin
boundaries, (4) the headless server API. Everything else is a *composition* of those. Phase 1
implements only primitive (1). No second orchestration engine, no second provider protocol, no
embedded scripting runtime, no self-recompilation, no forced-todo/goal completion.

---

## 2. Locked crate manifest

Versions checked 2026-09-03 against docs.rs. **Phase-1 crates are PINNED.** Phase 2–4 crates are
**forward declarations**: the row is fixed (we commit to that crate for that concern), but the
exact version is re-pinned at implementation time — only the SCHEMA/interface it backs is frozen
now.

### Phase 1 — pinned (enter `Cargo.toml` now)

| Crate | Version | Features | Concern |
|---|---|---|---|
| `tokio` | 1.53 | rt-multi-thread, macros, process, signal, fs, io-util | async runtime, subprocess, ctrl_c |
| `reqwest` | (workspace latest) | json, stream, rustls-tls; **default-features=false** | HTTP client, `bytes_stream()` (rustls, NOT native-tls, for reproducible builds) |
| `serde` | 1 | derive | serialization |
| `serde_json` | 1 | — | JSON DTOs, tool args |
| `toml` | 0.8 | — | (reserved; config precedence) |
| `thiserror` | 2.0.20 | — | typed domain errors |
| `anyhow` | 1.0.104 | — | **main.rs / CLI boundary ONLY** |
| `tracing` | 0.1 | — | structured logging |
| `tracing-subscriber` | 0.3 | env-filter, fmt | log init from `RUST_LOG` (redacts Authorization) |
| `tokio-util` | 0.7 | rt | `CancellationToken` |
| `ulid` | 1 | std | sortable IDs |
| `clap` | 4.6.6 | derive | CLI (`run`/`serve`/`tui`/`auth`) |
| `nix` | 0.31.3 | signal | `killpg` for bash process-group termination (Unix/Linux-only) |
| `directories` | 6.0.0 | — | XDG config/data dirs (`ProjectDirs`) |
| `tempfile` | 3 | — | atomic temp+rename writes |
| `regex` | 1 | — | builtin `grep` fallback when `rg` is absent |
| **[dev] `proptest`** | 1 | — | SSE arbitrary-chunk-boundary property test |
| **[dev] `wiremock`** or raw `TcpListener` helper | — | — | local mock HTTP SSE server for the loop/e2e |

`rust-toolchain.toml` pins a stable channel + target `x86_64-unknown-linux-gnu` (nix is
Unix-only; Linux-only is acceptable per the operating environment). MSRV is pinned in the
toolchain file. `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` are part of
the F2 gate.

### Phase 2–4 — forward declarations (interface frozen, version re-pinned later)

| Crate | Concern | Phase |
|---|---|---|
| `axum` | SSE/health server API (no WebSocket) | 2 |
| `rmcp` | MCP client transport | 2 |
| `rusqlite` | 0.40.2 | single-writer event store | 3 |
| `rusqlite_migration` | 2.6.0 | `user_version` migrations | 3 |
| `ratatui` + `crossterm` | TUI client | 4 |
| `notify` | hot-reload file watcher | 4 |

**None of these Phase 2–4 crates may enter `Cargo.toml` during Phase 1** (scope fence; enforced
by gate F4).

---

## 3. Error taxonomy

All library modules use **typed domain enums with `thiserror`**. `anyhow` appears **only** in
`main.rs`/CLI boundaries. No `String` error types. No catch-all `Other(String)` variant.

- **`SseError`** — `Utf8`, `UnexpectedEof`, `MalformedFrame(String)` (includes the oversize-frame
  case). Provider-agnostic; knows nothing about `[DONE]`, JSON, or tool-calls.
- **`ProviderError`** — `Http(#[from] reqwest::Error)`, `Sse(#[from] SseError)`,
  `Api { status: u16, message: String, partial: Option<Box<AssistantMessage>> }`,
  `MissingApiKey`, `UnknownModelAlias(String)`, `WrongContentType(String)`,
  **`Canceled { partial: Option<Box<AssistantMessage>> }`**.
- **`ToolError`** — `PathTraversal(PathBuf)`, `NotFound(PathBuf)`, `NoMatch`,
  `AmbiguousMatch { count: usize }`, `Timeout`, `Canceled`, **`Denied { code, message }`**,
  `Io(#[from] std::io::Error)`.
- **`ConfigError`** — `Read(#[from] std::io::Error)`, `ParseJson(#[from] serde_json::Error)`,
  `ParseToml(#[from] toml::de::Error)`, `MissingProvider(String)`.
- **`AgentError`** — top-level, `#[from]`-wraps each domain enum.

### Load-bearing rules (folded corrections)

- **`Canceled` is a DISTINCT variant, never an error-alias.** Phase-3 replay marks in-flight ops
  as *interrupted* vs *error* (SPEC §17.3); conflating cancel with error corrupts replay
  classification. The `partial` field on `Api`/`Canceled` (an `Option<Box<AssistantMessage>>`)
  does NOT blur that classification — cancellation stays its own variant.
- **Partial-message representation:** both `Api{...,partial}` and `Canceled{partial}` carry the
  partially-accumulated normalized `AssistantMessage` so a mid-stream API error or a cancellation
  after partial text/thinking/tool-call fragments preserves content (a unit variant cannot).
  `AssistantMessage` is defined in `provider/mod.rs`; `error.rs` references it.
- **`ToolError::Denied` is structured and matches SPEC §12.2 verbatim.** The model-facing denial
  result is `{status:"denied", error:{code, plugin_id?, message}}` — **`plugin_id` is present in
  the frozen shape even though plugins are Phase 2**, so Phase-2 hook denial reuses it exactly.
- **`ToolResult.status` includes `Interrupted`** (cancelled tool) in addition to `Ok`/`Denied`/
  `Error` — replay must tell an interrupted tool from an errored one.

### `TurnOutcome` / `RunOutcome`

```rust
enum TurnOutcome { Done, Yielded, Interrupted, Error }
```

Carries message finish-reason + usage metadata for the turn result.

---

## 4. Module map (per SPEC §27)

Phase-1 files are created now; the rest are the frozen destinations (empty until their phase).

> **Today:** this tree is the Phase 1 plan, not the code. `plugins/`, `persistence/` and `config/`
> were never created, and there is no `tools/mcp.rs`. Persistence is `session/store.rs`, rules and
> skills and agent files are in `agent/`, and `session/`, `navigator/`, `tui/`, `paths/` and
> `settings.rs` exist beyond this map. See [`ARCHITECTURE.md`](../ARCHITECTURE.md).

```
src/
├── main.rs            # thin bin: clap Cli, tracing init, dispatch (anyhow boundary)   [P1]
├── app.rs             # run/auth wiring, render(UiEvent) seam, SIGINT→cancel           [P1]
├── error.rs           # typed taxonomy (§3)                                            [P1]
├── agent/
│   ├── mod.rs
│   ├── actor.rs       # AgentActor + reusable constructor (subagent seam, SPEC §14.1)  [P1]
│   ├── loop.rs        # run_agent — ONE readable primary loop                          [P1]
│   ├── prompt.rs      # prompt builder + embedded system contract                      [P1]
│   └── response.rs    # classify → StepOutcome                                         [P1]
├── provider/
│   ├── mod.rs         # stream accumulator + AssistantMessage + on_delta seam          [P1]
│   ├── config.rs      # models.json loader + alias resolution                          [P1]
│   ├── auth.rs        # auth.json 0600 store + env fallback                            [P1]
│   ├── openai.rs      # typed request/response DTOs + reqwest POST                     [P1]
│   └── sse.rs         # hand-rolled provider-agnostic SSE frame decoder                [P1]
├── tools/
│   ├── mod.rs         # Tool trait + Registry + path safety + bounded output           [P1]
│   ├── native.rs      # read/write/edit/bash/grep                                      [P1]
│   ├── control.rs     # ask; today glob and fetch too, skill is in agent/skills/        [P1: ask]
│   └── mcp.rs         # MCP client tools                                               [P2]
├── bus/               # event envelope, command, inbox, sequencer                      [P2]
├── plugins/           # protocol, hooks, supervisor                                    [P2]
├── persistence/       # sqlite, migrations, replay                                     [P3]
├── config/            # rules, persona, skills, watch                                  [P4]
├── server/            # routes, sse (no websocket)                                    [P2]
└── tui/               # client, view                                                   [P4]
```

**LIB + BIN split:** all behavior lives in the library; `main.rs` is a thin CLI shell (enables
library-level tests and Phase-4 reuse). No generic `manager`/`engine`/`framework` abstraction
without a second concrete use (SPEC §27). The loop stays legible as one primary function
(SPEC §8.2).

---

## 5. TDD policy

- **Test-first per todo:** write the failing test, implement, then green. Implementation + test
  are ONE unit of work, never separated.
- Rust built-in `#[test]` / `#[tokio::test]`; unit tests inline, integration tests under
  `tests/`. `proptest` for the SSE chunk-boundary property. `wiremock`/raw `TcpListener` for the
  mock HTTP server. No extra test framework.
- Every todo has a **happy** and a **failure** QA scenario with the exact `cargo test`/`cargo run`
  invocation and an evidence path `.omo/evidence/task-<N>-navai-phase1.txt`.
- **Anti-fake rule:** "SSE works" / "it compiles" is never an acceptance criterion. Two DISTINCT
  test suites, never conflated:
  - **SSE framing suite** (provider-agnostic, `provider::sse`): the 13 named framing tests +
    `prop_arbitrary_chunk_boundaries` = **14**. Does NOT interpret `[DONE]`, tool-calls, or JSON.
  - **Accumulator/provider suite** (`provider::mod`): `[DONE]`, sparse/out-of-order tool-call
    indices, interleaved fragments, invalid metadata/JSON, partial-failure representation, usage
    capture, bytes-after-`[DONE]`, HTTP status + wrong content-type, and real reqwest-stream
    cancellation.
- **Live dependency (no skip/fallback):** the live e2e (todo 14) REQUIRES OmniRoute at
  `http://localhost:20128/v1` + `auth.json`. If the proxy is unavailable the live test **FAILS or
  is recorded as an external BLOCKED gate — never a passing skip, fallback, or best-effort
  substitution**. `#[ignore]` keeps it out of offline CI, but the mock-only run never satisfies
  the live exit criterion.

---

## 6. Frozen SQLite event-sourced schema (implemented Phase 3, schema frozen NOW)

The event log is canonical; `messages` and similar tables are rebuildable projections
(SPEC §17.1). Written now so Phase 3 adds code, not new decisions.

```sql
workspaces(
    workspace_id   TEXT PRIMARY KEY,
    root_path      TEXT NOT NULL,
    created_at     TEXT NOT NULL,
    last_opened_at TEXT NOT NULL
)

sessions(
    session_id    TEXT PRIMARY KEY,
    workspace_id  TEXT NOT NULL,
    root_actor_id TEXT NOT NULL,
    status        TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
)

actors(
    actor_id        TEXT PRIMARY KEY,
    session_id      TEXT NOT NULL,
    parent_actor_id TEXT,
    persona         TEXT NOT NULL,
    model           TEXT NOT NULL,
    state           TEXT NOT NULL
)

events(
    session_id     TEXT NOT NULL,
    seq            INTEGER NOT NULL,
    event_id       TEXT NOT NULL UNIQUE,
    actor_id       TEXT NOT NULL,
    kind           TEXT NOT NULL,
    occurred_at    TEXT NOT NULL,
    causation_id   TEXT,
    correlation_id TEXT,
    payload_json   TEXT NOT NULL,
    PRIMARY KEY (session_id, seq)
)

messages(
    message_id      TEXT PRIMARY KEY,
    session_id      TEXT NOT NULL,
    actor_id        TEXT NOT NULL,
    turn_id         TEXT NOT NULL,
    role            TEXT NOT NULL,
    content_json    TEXT NOT NULL,
    source_event_id TEXT NOT NULL
)

inbox_messages(
    command_id      TEXT PRIMARY KEY,
    session_id      TEXT NOT NULL,
    source_actor_id TEXT NOT NULL,
    target_actor_id TEXT NOT NULL,
    mode            TEXT NOT NULL,
    content_json    TEXT NOT NULL,
    status          TEXT NOT NULL,
    created_at      TEXT NOT NULL
)

plugin_state(
    plugin_id  TEXT NOT NULL,
    session_id TEXT NOT NULL,
    state_json TEXT NOT NULL,
    PRIMARY KEY (plugin_id, session_id)
)
```

### Durability / engine decisions (frozen)

- **`PRIMARY KEY (session_id, seq)`** — total order within a session; `seq` is monotonic, assigned
  by the session sequencer. Wall-clock time is NEVER an ordering key.
- **WAL mode**, `foreign_keys=ON`, **`synchronous=NORMAL`** (FULL only if newest-event loss is
  unacceptable), explicit `busy_timeout`.
- **Single-writer actor** via `rusqlite` behind a **bounded channel** (writes serialized; SQLite
  never touched directly across the async runtime — avoids accidental blocking).
- Migrations via **`rusqlite_migration`** using `user_version`, applied at startup by the writer
  actor, forward-only. **`schema_version` reserved = 1** for forward-compat.
- Payload + `seq` committed in the SAME transaction; durable events acknowledged BEFORE broadcast
  to non-durable subscribers (SPEC §17.2). `llm.delta` MAY be non-durable; final `llm.response`,
  messages, tool calls/results, state changes, hook decisions MUST be durable.
- Tool outputs bounded before persistence; secrets never duplicated across payloads; DB file
  created user-only; encryption-at-rest is out of scope and documented to the operator (§17.4).

---

## 7. Frozen cross-cutting types (Phase 1 defines, later phases consume)

Freeze **wire semantics**, not a promise the Rust types never evolve.

### 7.1 `EventEnvelope` (SPEC §10.1)

```rust
struct EventEnvelope {
    schema_version: u16,          // reserved = 1
    event_id: EventId,            // ULID
    seq: u64,                     // assigned by session sequencer
    session_id: SessionId,        // ULID
    actor_id: ActorId,            // STRING grammar (see below)
    kind: String,                 // reserved names unprefixed; plugins use plugin.<id>.<name>
    occurred_at: DateTime<Utc>,
    causation_id: Option<Ulid>,
    correlation_id: Option<Ulid>,
    payload: serde_json::Value,
}
```

### 7.2 ID newtypes — CORRECT backing (not all ULIDs)

- **`ActorId` = STRING** following the SPEC grammar: `agent:main`, `user:tui-1`. **NOT a ULID.**
- **`ToolCallId` = OPAQUE PROVIDER STRING** — supplied by the model/provider, never generated by
  us.
- **ULID-backed:** `SessionId`, `TurnId`, `StepId`, `EventId`, `CommandId`, `MessageId`, `TaskId`.

### 7.3 Command vs event split (SPEC §6.4)

A **command** is targeted (`command_id`, `source_actor_id`, `target_actor_id`, `mode`, `content`,
`created_at`). An **event** is immutable + broadcast, carrying `seq` for total order. The
implementation MUST keep these distinct; Phase 1 has no bus yet but the types are reserved here.

### 7.4 Loop types (SPEC §8.1 / §8.3)

```rust
enum StepOutcome {
    Continue { tool_calls: Vec<ToolCall> },
    Done     { response: AssistantMessage },
    Yield    { request: HumanRequest },
}

struct HumanRequest { question: String, kind: Option<String>, details: Option<Value> }

enum TurnOutcome { Done, Yielded, Interrupted, Error }
```

### 7.5 Transcript `ContentBlock` — PROVISIONAL

```rust
enum ContentBlock { Text(..), Thinking(..), ToolUse(..), ToolResult(..) }
```

Marked **PROVISIONAL** until its serialized schema + unknown-variant policy are specified (its
on-wire form is not frozen this phase; only that thinking is a SEPARATE block from text).

### 7.6 `ToolResult` wire shape (SPEC §12.2)

```rust
struct ToolResult {
    status: ToolStatus,           // Ok | Denied | Error | Interrupted
    output: String,
    truncated: Option<Truncated>, // { original_bytes: usize }
    error: Option<ToolErrorObj>,  // { code: String, plugin_id: Option<String>, message: String }
    metadata: Value,              // e.g. grep { engine: "rg" | "builtin" }
}
```

The `{status, error:{code, plugin_id, message}}` shape matches SPEC §12.2 **verbatim including
`plugin_id`** so Phase-2 hook denial reuses it.

### 7.7 `AssistantMessage` (normalized)

```rust
struct AssistantMessage {
    text: String,
    thinking: String,             // from reasoning_content — SEPARATE, never merged into text
    tool_calls: Vec<ToolCall>,    // { id, name, args: Value }
    finish_reason: Option<String>,
    usage: Option<Usage>,         // captured from the final chunk
}
```

### 7.8 Cancellation hierarchy

`process/session → actor → turn → step → operation`, chained via `tokio_util`
`CancellationToken::child_token()`. Phase 1 wires **session → actor → turn → step**; the **actor**
level is the seam Phase 3 hangs subagents on: foreground (`background=false`) children parent to
the parent TASK token; background (`background=true`) children parent to the SESSION token so they
survive parent-turn completion (SPEC §14.2). No hooks/inbox/steer/interrupt machinery in the
Phase-1 loop — the loop is SPEC §8.2 MINUS those.

### 7.9 Per-model `reasoning_history` policy — P0, load-bearing

Per-model policy in `models.json` `ModelConfig`: **`reasoning_history: preserve | omit`**, default
**`preserve`**. `thinking` is ALWAYS kept in the local transcript. When building continuation
history, map `thinking → reasoning_content` on the assistant message **iff** the model policy is
`preserve`. The validated DeepSeek alias (`opencode-go/deepseek-v4-flash`) uses `preserve`:
DeepSeek Thinking Mode REQUIRES the prior assistant `reasoning_content` to be **replayed** when a
request carries `tools` and continues after a tool call, or the API may return HTTP 400.
**Reasoning is REPLAYED, never stripped.**

---

## 8. Phase-1 loop → actor refactor seam (SPEC §14.1, no rewrite)

The Phase-1 `run_agent` is written so Phase 3 turns it into a reusable actor constructor without a
rewrite: `AgentActor { transcript, provider, registry, model_alias, cancel, state }` already has a
constructor usable for subagents. Hooks and the inbox buffer are **added at marked seams** (the
`turn.start` / `prompt.pre_llm` / `tool.pre_call` / `tool.post_call` points in SPEC §8.2), not
built now. Stdout rendering goes through ONE `render(UiEvent)` fn keyed on stable ids
(session/turn/step/tool_call) so Phase 2's "move stdout behind an event subscriber" is mechanical.

---

## 9. Corrected lifecycles (personal, local, single-user — NOT a deployment tier)

SPEC §20.1/§25.1 define a PERSONAL, LOCAL, single-user tool bound to `127.0.0.1`. There is **no
multi-tenant preprod/prod**. The lifecycles are ENVIRONMENTS on the user's own machine, isolated
by config/data ROOTS — not deployment tiers.

- **dev** — local `cargo run -- run`; `RUST_LOG` tracing; `models.json` + `auth.json` in
  repo/config; tests via `cargo test`.
- **preprod / staging** — the SAME binary running against a **scratch config/DB** on the user's
  machine (or a homelab host such as `pixma-main`). It is an ENVIRONMENT, not a tier. **Isolation
  is CONCRETE and pinned:** a `--config <dir>` / `--data <dir>` override (or explicit
  `XDG_CONFIG_HOME` / `XDG_DATA_HOME`) so a staging run **never touches** the prod
  `~/.config/navai/auth.json`. Config precedence + per-env data dir are pinned so dev
  experiments never pollute prod.
- **prod (personal)** — `cargo build --release`; single binary; loopback only (never a
  non-loopback bind without explicit config, SPEC §20.1); `auth.json` 0600, DB file user-only
  (SPEC §17.4). Optional systemd *user* unit.
- **update** — **external supervisor + atomic binary swap ONLY.** Self-recompilation is a NON-GOAL
  (SPEC §3, §18.4 level 3) — documented as a seam, NOT designed here. Config/persona/plugin
  hot-reload via `notify` is Phase 4. `schema_version` reserved for forward-compat migrations
  applied at startup by the writer actor.
- **backup (corrected — WAL-safe).** A bare `wal_checkpoint` + file-copy is only safe while writes
  are QUIESCED. Safe options, in order of preference:
  1. **`VACUUM INTO`** executed THROUGH the single-writer actor (consistent snapshot),
  2. the SQLite **ONLINE BACKUP API**, or
  3. **stop / quiesce → copy → restart.**
  **Do NOT document naive checkpoint-then-copy as safe.** The append-only event log makes
  **replay the restore path** (SPEC §17.3). Also back up `auth.json` + `models.json` + `agents/*`.
  Encryption at rest is out of scope and must be stated to the operator.
- **crash / resume** — replay durable events through pure reducers; in-flight model/tool ops
  marked **interrupted**, never re-run (SPEC §17.3, §26 risk #5).

### Config & secret precedence (pinned now)

- **`models.json`** location + precedence: workspace `./models.json` wins over
  `~/.config/navai/models.json`; `--models <path>` overrides both.
- **`auth.json`**: file (`~/.config/navai/auth.json`) WINS over the env var named by the
  provider's `api_key_env` (default `OMNIROUTE_API_KEY`); `auth set` writes the file. Created
  **atomically with mode 0600 at creation** (`O_CREAT|mode`, not create-then-chmod). An existing
  `auth.json` with looser perms **fails closed** with a clear "fix perms" error — a
  world-readable secret is never silently trusted. `auth list` masks/omits values; the key is
  NEVER written to tracing or a child-process env. `models.json` stores env-var NAMES
  (`api_key_env`), never secret VALUES.

---

## 10. Frozen native tool schemas (model-facing contract)

The 5 tool JSON schemas are FROZEN here (Phase-2 `tool.pre_call` payloads reference these arg
shapes). Timeouts / output caps / bash grace-ms are NAMED consts (no config file for them in
Phase 1) with env overrides for tests.

- **`read`** — `{ path: string, offset?: int (1-indexed line), limit?: int }`. Normalize path via
  `validate_path`; report `truncated:{original_bytes}`; never invent content; binary/non-UTF8 →
  lossy or structured error.
- **`write`** — `{ path: string, content: string }`. Atomic: tempfile in the target's PARENT dir
  → write → fsync file → rename → fsync parent dir; create parent dirs; clean up temp on failure.
- **`edit`** — `{ path: string, old_string: string, new_string: string, replace_all?: bool }`.
  **Exact literal match** (Claude-Code style; no fuzzy/whitespace-normalize). FAIL on no-match
  (`NoMatch`); FAIL on >1 match (`AmbiguousMatch`) unless `replace_all`; atomic write; **no
  mutation on failure** (file byte-identical).
- **`bash`** — `{ command: string, timeout?: int(ms) }`. Runs via `tokio::process::Command` with
  `.process_group(0)`, cwd = workspace, bounded (head+tail with elision marker, report original
  bytes) stdout+stderr drained CONCURRENTLY. Cancellation AND timeout follow ONE policy:
  `killpg(pgid, SIGTERM)` → grace (default 5s, named const, env override) → `killpg(pgid,
  SIGKILL)` → `child.wait().await` to reap. ESRCH = already-exited. Reader-join is BOUNDED (a
  post-kill drain deadline, then abort readers / drop handles) so a `setsid` descendant holding a
  descriptor open can't hang the tool. **Documented limit:** `setsid`/daemonized descendants that
  leave the process group can escape (PGID cleanup, not cgroup containment) — accepted v0.1
  boundary.
- **`grep`** — `{ pattern: string, path?: string }`. Prefer `rg` via `tokio::process::Command`;
  bound result COUNT and output BYTES independently; **`rg` exit 1 = no-match = empty `Ok`, exit
  ≥2 = error**. If `rg` is absent → **built-in stdlib recursive line-scan fallback** using the
  `regex` crate with the SAME bounds and the SAME `ToolResult` shape (never a "tool unavailable"
  error). Record `metadata.engine = "rg" | "builtin"`.

### Path safety (`validate_path`)

`ToolContext { workspace: PathBuf, writable_roots: Vec<PathBuf> }` supplies roots (Phase 1: CLI
`run --workspace`, default cwd; writable roots = `[workspace, config_dir]`). Algorithm for
not-yet-existing `write` targets: canonicalize the NEAREST EXISTING ancestor, lexically validate
the missing components (reject `..` escapes), confirm the resolved ancestor is inside a root, and
for `write` **REVALIDATE after creating parents**, immediately before opening the same-directory
temp file. Symlink policy: canonicalize then prefix-check (TOCTOU accepted for v0.1, documented).

---

## 11. Provider transport decisions (frozen)

- **Endpoint:** `POST <base_url>/chat/completions`, `stream: true`, bearer auth, OpenAI-compatible
  ONLY (SPEC §3, §21.1). No second protocol. **No `rig` in the core** (SPEC §23) — hand-rolled
  `reqwest` + typed DTOs + focused SSE decoder.
- **Timeouts:** `connect_timeout` (fail fast on connect) + an IDLE/read timeout between chunks
  (long streams are legit). **No short TOTAL-request timeout** (would truncate long generations);
  `request_timeout_ms` is a generous ceiling only.
- **`stream_options.include_usage: true`** so token usage is captured from the final chunk into
  the `AssistantMessage`/turn result.
- **No automatic retry in Phase 1** — 429/5xx surface as `ProviderError::Api`; retry/backoff is a
  documented Phase-2+ decision. Pinned "no retry" so a worker doesn't add hidden retries.
- **SSE framing is strictly separate from OpenAI JSON decoding:** `sse.rs` = generic frames (max
  frame size 4 MiB, matching SPEC §11.3 default → `SseError::MalformedFrame` when exceeded);
  `mod.rs` accumulator interprets `[DONE]`, tool-call fragments, `reasoning_content`, errors,
  content-type, and cancellation.
- **Streaming contract:** the accumulator emits an incremental delta via
  `on_delta: &mut dyn FnMut(StreamDelta)` AS EACH chunk is processed (`Text` / `Thinking` /
  `ToolCallStarted` / `ToolCallArgs`) so the caller renders text BEFORE `[DONE]` — enforced by a
  timeline test (text deltas precede terminal).
- **Cancellation = observable cleanup:** race the cancel token around request send, awaiting
  headers, and EACH stream read + idle timeout; return `Canceled { partial }` (or `Api { partial }`
  for mid-stream error) and DROP the response/stream (closes the connection).
- **Transcript validity:** the normalized assistant message is appended as
  `assistant(text?, reasoning?, tool_calls)` BEFORE its tool results; the loop appends exactly ONE
  `role:tool` message per `tool_call_id`, in logical index order — never orphan tool_calls, never
  a tool result without its assistant tool_call.

---

## 12. Sacred invariant — no forced continuation (SPEC §5.2)

The harness **MUST NOT** equate observable proxies with task success. **PROHIBITED:** refusing to
stop while a todo list is non-empty; treating goal-file deletion as completion; requiring a
particular number of tool calls; continuing merely because token budget remains; inferring success
from a self-reported flag without exposing the result. **Every enforced state has an honest
exit — `Yield`, not fabricated completion or forced continuation.** The Phase-1 `MAX_STEPS` guard
(default 50, env override) stops the loop with a `Yield` carrying a visible "step budget
exhausted" reason — NOT a forced continuation and NOT a silent stop. This is the single most
load-bearing behavioral invariant of the project.

---

## 13. Exit codes (CLI, pinned)

`0` = Done · `2` = Yielded (awaiting human — NOT failure) · `1` = error · `130` = SIGINT
(Ctrl-C → cancel session token → graceful cleanup: abort request, kill bash process groups).
