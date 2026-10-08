# Handoff: WithNav — InstinctFlow

Design reference: `WithNav InstinctFlow.dc.html` (open in a browser next to `support.js`). Top bar switches **theme**. InstinctFlow was previously called Jev flows. Layers, model names and agents are specified in `SETTINGS.md`.

This is a design, not as-built. Where the code disagrees, the code is right; items marked **(proposal)** do not exist in the code yet.

## InstinctFlow (WebUI)

Purpose: let the Captain build chains of **System One mini-decisions** run by the instinct layer on agent-loop hooks. ADR 0005 holds: flows are data (TOML), reloaded on save, validated at load.

### Layout
Top bar → three columns: flow list (220px) · canvas (flex) · inspector (340px).

- **Flow list:** grouped by hook (real events from `src/bus/event.rs`: `session.start`, `message.received`, `tool.pre_call`, `tool.post_call`, `context.updated`, `subagent.spawned`, `turn.end`, …), each row with event, flow name, blocking/async icon, on/off. Then **Sub-flows** ("used by N") and **Variables** (name, scope session/workspace, current value).
- **Flow header:** name, file `.withnav/flows/<name>.toml`, `on <event>` pill. **Blocking** toggle + **budget ms** + **on timeout** action (per flow). Sub-flows show "called from …, runs inside the caller's budget". Save.
- **Toolbar:** + Decision (Noul, Choice, Score), + Logic (Expression, Sub-flow), + Action (list below); zoom; Canvas / TOML view.
- **Canvas:** dotted grid, draggable nodes, bezier wires from output ports (right edge) to the input port (left edge, header height). Node anatomy, fixed heights so ports line up: header 30px (type icon, TYPE tag, name) · body 60px (instructions, 2 lines; `state: …` line) · run strip 24px (answer · ms · cost, or "not reached" / "no run yet") · one 22px row per output port. Selected node: accent border + soft ring.
- **Run bar** (bottom): sample event, Run sample / Clear run, path taken on its own wrapping line (`tool.pre_call › effect = local_change › asked_for = no › …`), Instinct calls · cost · tokens, budget bar `614 / 800 ms` (or "async").
- **Run mode:** reached nodes full opacity with tinted run strip; unreached at 50%; taken wires accent 2.2px, others dashed muted.

### Node types
- **Trigger** — event + filter expression; inspector lists events with "can block" / "async only" **(proposal: blocking allowed on `message.received`, `turn.start`, `tool.pre_call`, `permission.requested`, `subagent.spawned`)** and shows the sample event JSON.
- **Decision** (the three System One types, `src/provider/systemone.rs`):
  - `noul` — instructions; threshold slider; ports `yes ≥ t`, `no`.
  - `choice` — instructions + options (name → criteria); **one port per option**; run shows probability bars.
  - `score` — instructions + 2–10 ordered levels; "at least level" threshold; ports `≥ level`, `below`.
  - Inspector: type selector, instructions, **State** = inputs sent with the question (chips + "Add input" catalog with checkboxes) and **State as sent** preview. Input catalog groups: Event (per hook), Session (`session.id`, `session.agent`, `session.model`, `session.workspace`), Conversation with Nav (`last_captain`, `last_turns`, `digest`), Intent record, Workspace (root, branch, dirty files), Editor (open files, cursor, selection — IDE only), WithNav settings, Variables, **Upstream answers** (earlier nodes' outputs).
- **Expression** — combines answers, inputs and variables (`asked_for < 0.4 && session.agent == "plan"`); no Instinct call; ports `true`, `false`.
- **Sub-flow** — calls another flow as one node; input mapping; its declared outputs become ports; "Open ›".
- **Action** (ends a chain; may have a `then` port to chain another action): allow tool call, deny + inject note, ask the Captain, inject note for Nav, trigger compaction, switch agent, switch variant, spawn subagent (agent, task, delivery), set variable, log only, show remark, wake Nav, run skill / command.

Inspector bottom: **Last sample run** — raw System One answer JSON (`noul` / `choice` + probabilities + confidence / `score` + legend), ms and cost per node.

### Example flows in the mock
- `guard-mutations` on `tool.pre_call`, blocking 800 ms → choice `effect` → noul `asked_for` / sub-flow `trusted-host` / score `blast_radius` → expression `plan_overreach` → allow / ask / deny.
- `compaction-advisor` on `context.updated` (async) → noul `natural_break` → compact → set var; or score `urgency` → expression → inject note / log.
- `route-agent` on `message.received`, blocking 600 ms → choice `request_kind` → keep / switch agent plan / build → switch variant / spawn agent `lux`.
- `trusted-host` sub-flow → choice `host_kind` → noul `looks_safe` → return `trusted` / `unknown`.

### TOML (proposal)
```toml
[flow]
name = "guard-mutations"
on = "tool.pre_call"
when = 'tool in ["bash", "write", "edit", "fetch"]'
blocking = true
budget_ms = 800
on_timeout = "ask_driver"

[[node]]
id = "effect"
type = "choice"
state = ["event.tool", "event.args", "session.agent"]
instructions = "What will this call do to the workspace?"
[node.criteria]
read_only = "Only reads files or prints information."
local_change = "Changes files inside the workspace."
[node.next]
read_only = "allow"
local_change = "asked"
```
The TOML view in the mock is generated from the canvas data and shows the full shape.

## Implementation order (suggested)
1. Config model: `models.json` as catalogue only; `config.json` gains `layers`, `agents.<name>.model`/variant and `default_agent` (config wins over `.md`); `<gateway>/<id>` names; settings validation as in `settings.rs`.
2. Server routes for settings read/write and provider test; WebUI Settings page; VS Code settings webview.
3. Flow runtime: load/validate `.withnav/flows/*.toml`, hook subscriptions on the bus, blocking with budget + on-timeout, System One calls through the instinct client, expressions, sub-flows, actions, variables; log every run (state, answers, ms, cost) for replay.
4. WebUI flow editor: canvas, inspector, TOML view, sample run.
