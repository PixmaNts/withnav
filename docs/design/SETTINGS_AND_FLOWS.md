# Handoff: Navai — Settings and Jev flows

> **Design reference, not as-built; 0.6.0 proposals after grilling.** Agreed decisions,
> [`CONTEXT.md`](../../CONTEXT.md) and the authoritative 0.5.0 target
> `.scratch/models-layers-agents/spec.md` in the main checkout take precedence over the design.
> Code documents current behavior, not authority to override the spec. The 0.5.0 foundations
> are not claimed complete. Settings screens, flow runtime/editor and their controls below
> are proposals requiring their own spec; mock behavior does not establish shipped functionality.

Design reference: `Navai Settings and Flows.dc.html` (open in a browser next to `support.js`). Top bar switches **screen** (Jev flows · WebUI, Settings · WebUI, Settings · VS Code) and **theme** (Ayu Dark/Light, Nord Dark/Light, same tokens as `README.md` and `src/tui/theme.rs`).

HTML remains reference only, not a source of configuration or security requirements.

## Vocabulary and storage (supersedes earlier drafts)

No aliases in the UI. Four **layers**:

| Layer | API | Models | Notes |
| --- | --- | --- | --- |
| Reflex | `fim` | exactly one | LSP only: runs when an IDE is attached (VS Code now; Zed, IntelliJ later). |
| Instinct | `systemone` | exactly one | Jev. Fixed 2 s deadline. Runs Jev flows, permission review, the navigator pipeline. |
| Metacognition | `chat` | exactly one | Fast (flash) model. **Variant = one the model offers**: `low` when offered, otherwise the model's default variant. Also reviews risky calls (see Permissions). |
| Cognition | `chat` | one per **agent** | Several agents and several flows (subagents) at once. |

**Agents** (the word is *agent*, never "profile"): `build`, `plan`, `librarian`, `explorer`, `oracle`, … Each has a model, a variant offered by that model, and a mode `primary`, `subagent` or `all` (as in `AgentMode`). Prompt, tools and access live in `.navai/agents/<name>.md` (also `~/.config/navai/agents/`). The cognition has a **default agent** (`default_agent`, a primary agent), not a default model. Flow inputs use `session.agent`; the flow action is "switch agent".

**Storage:**
- `models.json` = **catalogue only** (gateways and their models).
- `config.json` = `layers` (one model each), `agents.<name>.model` / variant, `default_agent`, plus the existing keys (`permissions`, `subagents`, `agent`, `tui`). A model set in `config.json` **wins over** a `model:` line in the agent's `.md`.

**Model names** are `<gateway>/<id>`, e.g. `omniroute/cc/claude-opus-5-5`, `omniroute/typesafe/jev-1.13`, `inception/mercury-edit-2`. Every model list is **grouped by gateway, then by provider** (the segment after the gateway: `cc`, `zai`, `openai`, `opencode-go`, `combo`, `typesafe`; none for `inception/mercury-edit-2`), with **collapsible groups**, and filtered by the target's API.

Mock assignment: build → `omniroute/cc/claude-opus-5-5` · high, plan → `omniroute/zai/glm-5.3` · high, librarian → `omniroute/openai/gpt-6-luna` · medium, explorer → `omniroute/opencode-go/deepseek-v4-flash` · low, oracle → `omniroute/openai/gpt-6-astra` · high (mode `all`). The `zai` / `openai` provider segments and every variant list are placeholders.

**Who writes what:** proposed Settings assignment controls write `config.json`; gateway/catalog controls target `models.json`, catalog only. The session model picker and `/model` / `/variant` are **overrides for the current (session, agent)**, persisted with the session, never assignment config edits. Switching agents and back restores the override; a subagent with its own model does not inherit it. A subagent without a model uses the parent's effective model. `/model` without a variant takes the new model's default variant. `default_agent` is `build` when absent. Search ignores collapsed gateway/provider groups.

**Combo variants:** OmniRoute does not advertise effort tiers for combos; the client import declares `low`, `medium`, `high` on every `combo/*` catalog entry via the `reasoning_effort` request parameter. Import merges rather than overwrites manually declared variants. A combo name suffix is not an effort setting.

**Security boundary:** no reusable bash/file folder approvals; only eligible `fetch` origin grants are reusable. Native config mutation always asks the driver before review or scratch exemptions. Literal config references in bash ask too, but bash has ambient access: detection/review is best effort, not a sandbox. Gated outside-project reads are reviewed; deterministic sensitive native reads require metacognition then the driver. These constraints apply to all proposed screens and flow actions, including an "allow" node.

## Settings

### WebUI — full page with sidebar
Sidebar: Providers, Layers, Permissions, Subagents, Appearance (each with a one-line live summary), "Back to <workspace>", file paths at the bottom. Top bar: search, "Saved" status (settings save immediately, atomic write as in `settings::save`).

- **Gateways** (`models.json → providers`, catalogue only) — list (status, base URL, "used by N") + detail: Base URL, API key env + key source (environment / auth store, never shown), catalog format, connect / request timeout (instinct ignores it, 2 s), headers table, **catalogue grouped by provider** (collapsible; id, full name, API, context, variants), "Used by" chips (layers + agents). Actions: Test connection, Refresh catalog, Add gateway.
- **Layers** — four cards (layer, API tag, current model, note). Selecting:
  - Reflex / Instinct / Metacognition: gateway › provider, API locked to the layer, model, context, max output, "one model at a time", key `config.json → layers.<layer>`. Metacognition adds **Variant** buttons from the model (`low` tagged default). Reflex lists editors (VS Code connected; Zed, IntelliJ planned).
  - Cognition: default agent; agents table (agent, mode, model, gateway, variant, "set default" — primary/all only); detail: model with note "saved in `config.json → agents.<name>.model`, wins over `model:` in `<name>.md`", variant buttons from the model, mode `primary · subagent · all`, "up to N subagents per session" → Subagents.
  - Below either: **catalogue picker** grouped gateway › provider (collapsible), filtered by the target's API; clicking assigns the model to the selected layer or agent.
- **Permissions** — `permissions.automatic_review` (ADR 0010). On: each `bash`, `write` and `edit` is reviewed before it runs, shown as three steps — **1 Instinct** classifies effects, scope and risk; read-only and low-risk project changes run. **2 Metacognition** must approve risky or outside-project changes. **3 Driver** decides on any doubt, timeout or reviewer failure (allow once or deny); sensitive reads always end here. Off: every `bash`, `write` and `edit` is asked. `fetch` keeps its origin grants: **allow once, allow origin, or deny**. `permissions.scratch_dirs` list (absolute paths, empty = off, reset to default).
- **Subagents** — `subagents.max_concurrent` stepper 1–64 (default 4) with slot preview; `subagents.watchdog_interval_secs` and `agent.watchdog_interval_secs` presets Off / 1 min / 5 min / 15 min / 1 h (default 300, 0 disables).
- **Appearance** — four theme cards with mini previews; writes `tui.theme` (`ayu-dark`, `ayu-light`, `nord-dark`, `nord-light`).

### VS Code — native-style settings webview
An editor tab "Navai Settings" that mimics the VS Code settings editor: search box with count, "open JSON" icon, scope tab, TOC on the left (Gateways › each gateway; Layers › reflex, instinct, metacognition, cognition › agents; Permissions; Subagents; Appearance), rows on the right as `Category › sub: **Title**`, description, control (text, number, dropdown, checkbox, list with "Add Item"), blue left bar + "Modified" for non-default values. The extension's own editor settings (`navai.idleMs`, `navai.reflex.enabled`, …) stay in VS Code settings; link to them from the TOC footer. Use VS Code theme colors (`--vscode-*`) in the real webview.

## Jev flows (WebUI)

Purpose: let the driver build chains of **System One mini-decisions** run by the instinct layer on agent-loop hooks. ADR 0005 holds: flows are data (TOML), reloaded on save, validated at load.

### Layout
Top bar → three columns: flow list (220px) · canvas (flex) · inspector (340px).

- **Flow list:** grouped by hook (real events from `src/bus/event.rs`: `session.start`, `message.received`, `tool.pre_call`, `tool.post_call`, `context.updated`, `subagent.spawned`, `turn.end`, …), each row with event, flow name, blocking/async icon, on/off. Then **Sub-flows** ("used by N") and **Variables** (name, scope session/workspace, current value).
- **Flow header:** name, file `.navai/flows/<name>.toml`, `on <event>` pill. **Blocking** toggle + **budget ms** + **on timeout** action (per flow). Sub-flows show "called from …, runs inside the caller's budget". Save.
- **Toolbar:** + Decision (Noul, Choice, Score), + Logic (Expression, Sub-flow), + Action (list below); zoom; Canvas / TOML view.
- **Canvas:** dotted grid, draggable nodes, bezier wires from output ports (right edge) to the input port (left edge, header height). Node anatomy, fixed heights so ports line up: header 30px (type icon, TYPE tag, name) · body 60px (instructions, 2 lines; `state: …` line) · run strip 24px (answer · ms · cost, or "not reached" / "no run yet") · one 22px row per output port. Selected node: accent border + soft ring.
- **Run bar** (bottom): sample event, Run sample / Clear run, path taken on its own wrapping line (`tool.pre_call › effect = local_change › asked_for = no › …`), Jev calls · cost · tokens, budget bar `614 / 800 ms` (or "async").
- **Run mode:** reached nodes full opacity with tinted run strip; unreached at 50%; taken wires accent 2.2px, others dashed muted.

### Node types
- **Trigger** — event + filter expression; inspector lists events with "can block" / "async only" **(proposal: blocking allowed on `message.received`, `turn.start`, `tool.pre_call`, `permission.requested`, `subagent.spawned`)** and shows the sample event JSON.
- **Decision** (the three System One types, `src/provider/systemone.rs`):
  - `noul` — instructions; threshold slider; ports `yes ≥ t`, `no`.
  - `choice` — instructions + options (name → criteria); **one port per option**; run shows probability bars.
  - `score` — instructions + 2–10 ordered levels; "at least level" threshold; ports `≥ level`, `below`.
  - Inspector: type selector, instructions, **State** = inputs sent with the question (chips + "Add input" catalog with checkboxes) and **State as sent** preview. Input catalog groups: Event (per hook), Session (`session.id`, `session.agent`, `session.model`, `session.workspace`), Conversation with the cognition (`last_driver`, `last_turns`, `digest`), Intent record, Workspace (root, branch, dirty files), Editor (open files, cursor, selection — IDE only), Navai settings, Variables, **Upstream answers** (earlier nodes' outputs).
- **Expression** — combines answers, inputs and variables (`asked_for < 0.4 && session.agent == "plan"`); no Jev call; ports `true`, `false`.
- **Sub-flow** — calls another flow as one node; input mapping; its declared outputs become ports; "Open ›".
- **Action** (ends a chain; may have a `then` port to chain another action): allow tool call, deny + inject note, ask the driver, inject note into cognition, trigger compaction, switch agent, switch variant, spawn subagent (agent, task, delivery), set variable, log only, show remark, wake cognition, run skill / command.

Inspector bottom: **Last sample run** — raw System One answer JSON (`noul` / `choice` + probabilities + confidence / `score` + legend), ms and cost per node.

### Example flows in the mock
- `guard-mutations` on `tool.pre_call`, blocking 800 ms → choice `effect` → noul `asked_for` / sub-flow `trusted-host` / score `blast_radius` → expression `plan_overreach` → allow / ask / deny.
- `compaction-advisor` on `context.updated` (async) → noul `natural_break` → compact → set var; or score `urgency` → expression → inject note / log.
- `route-agent` on `message.received`, blocking 600 ms → choice `request_kind` → keep / switch agent plan / build → switch variant / spawn agent `oracle`.
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

## Implementation order (suggested, not shipped)
1. Prerequisite 0.5.0 target, still requiring completion/validation: `models.json` as catalogue only; `config.json` gains `layers`, `agents.<name>.model`/variant and `default_agent` (config wins over `.md`); `<gateway>/<id>` names; persisted per-session/per-agent overrides. Not a 0.6.0 editor deliverable.
2. Server routes for settings read/write and provider test; WebUI Settings page; VS Code settings webview.
3. Flow runtime: load/validate `.navai/flows/*.toml`, hook subscriptions on the bus, blocking with budget + on-timeout, System One calls through the instinct client, expressions, sub-flows, actions, variables; log every run (state, answers, ms, cost) for replay.
4. WebUI flow editor: canvas, inspector, TOML view, sample run.
