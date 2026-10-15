# Handoff: WithNav — Settings

Design reference: `WithNav Settings.dc.html` (open in a browser next to `support.js`). Top bar switches **screen** (Settings · TUI, Settings · WebUI, Settings · VS Code) and **theme** (Ayu Dark/Light, Nord Dark/Light, same tokens as `README.md` and `src/tui/theme.rs`).

This is a design, not as-built. Where the code disagrees, the code is right; items marked **(proposal)** do not exist in the code yet.

## Vocabulary and storage (supersedes earlier drafts)

No aliases in the UI. Four **layers**:

| Layer | API | Models | Notes |
| --- | --- | --- | --- |
| Reflex | `fim` | exactly one | LSP only: runs when an IDE is attached (VS Code now; Zed, IntelliJ later). |
| Instinct | `systemone` | exactly one | Typed decisions. Fixed 2 s deadline. Runs InstinctFlow, permission review, the pipeline. |
| Metacognition | `chat` | exactly one | Fast (flash) model. **Variant = one the model offers**, `low` by default (no fixed off/low/medium/high scale). Also approves risky calls (see Permissions). |
| Cognition | `chat` | one per **agent** | Several agents and several flows (subagents) at once. |

**Agents** (the word is *agent*, never "profile"): `build`, `plan`, `vigie`, `mousse`, `lux`, … Each has a model, a variant offered by that model, and a mode `primary`, `subagent` or `all` (as in `AgentMode`). Prompt, tools and access live in `.withnav/agents/<name>.md` (also `~/.config/withnav/agents/`). The cognition has a **default agent** (`default_agent`, a primary agent), not a default model. Flow inputs use `session.agent`; the flow action is "switch agent".

**Storage:**
- `models.json` = **catalogue only** (gateways and their models).
- `config.json` = `layers` (one model each), `agents.<name>.model` / variant, `default_agent`, plus the existing keys (`permissions`, `subagents`, `agent`, `tui`). A model set in `config.json` **wins over** a `model:` line in the agent's `.md`.

**Model names** are `<gateway>/<id>`, e.g. `omniroute/cc/claude-opus-5-5`, `omniroute/typesafe/jev-1.13`, `inception/mercury-edit-2`. Every model list is **grouped by gateway, then by provider** (the segment after the gateway: `cc`, `zai`, `openai`, `opencode-go`, `combo`, `typesafe`; none for `inception/mercury-edit-2`), with **collapsible groups**, and filtered by the target's API.

Mock assignment: build → `omniroute/cc/claude-opus-5-5` · high, plan → `omniroute/zai/glm-5.3` · high, vigie → `omniroute/openai/gpt-6-luna` · medium, mousse → `omniroute/opencode-go/deepseek-v4-flash` · low, lux → `omniroute/openai/gpt-6-astra` · high, cartographe → `omniroute/openai/gpt-6-luna` · medium, sondeur → `omniroute/cc/claude-opus-5-5` · high (mode `all`). The `zai` / `openai` provider segments and every variant list are placeholders.

**Who writes what:** only the Settings screens write `config.json`. The session model picker (see README) is a **session override for the current agent**; it never touches the config and offers "Back to config".

## Settings

### TUI (Ratatui) — `/config` view inside the WithNav TUI
Opened with `/config` (or the palette); replaces the transcript area, `esc` returns to the session. Same chrome as the TUI session: row 1 header (`◆ nav`, cwd, branch, `/config`, save status `● N unsaved` / `✓ written` / `up to date`, config file paths), bottom key-hint bar.
- **Left column (~22 cols):** sections numbered 1–6 — Gateways, Layers, Agents, Permissions, Subagents, Appearance — each with a live one-line summary and a `●N` count of unsaved changes. `1…6` jumps, `tab` / `shift+tab` cycles.
- **Center:** section title + source file (`config.json → layers, default_agent`), one-line description, then `key  value` rows grouped under dim sub-headers (gateway name with reachability, layer name with API, agent name with mode and file). Focused row: accent left bar + tint, key in full text color, inline hint on the right. Unsaved rows marked `●` (blue) in the gutter.
  - Value colors by type: model/string = text, choice = accent text, integer = orange, boolean `true` green / `false` red.
  - Keys: `↑↓` move · `←→` cycle a choice / ±1 an integer · `space` toggle boolean · `enter` opens the model picker on model rows, edits strings/lists inline · `/` filter.
- **Model picker overlay:** same look as the session `/models` picker (grouped gateway › provider, foldable, `●` current, `›` cursor), titled `model for layers.metacognition` / `agents.build`, marked **writes config.json**. Only models of the row's API are listed (fim / systemone / chat).
- **Right panel (~36 cols):** FOCUSED — full config path, description, type, allowed values (variants come from the model; integer ranges; theme ids), saved value. PENDING — diff per changed key (`- old` red, `+ new` green), `[ctrl+s] write` and `[ctrl+z] revert`. Note: written for every client; a `/models` pick in a session never writes here.
- **Rows per section:** Gateways → `base_url`, `api_key_env` (+ key source), `catalog`, `connect_timeout_ms`, `request_timeout_ms` per gateway (models.json). Layers → `layers.{reflex,instinct,metacognition}.model`, `layers.metacognition.variant`, `default_agent` (primary agents only). Agents → `agents.<name>.model`, `agents.<name>.variant`. Permissions → `permissions.scratch_dirs`, and a read-only explanation of the rules. Subagents → `subagents.max_concurrent` (1–64), `subagents.watchdog_interval_secs`, `agent.watchdog_interval_secs` (0 · 60 · 300 · 900 · 3600). Appearance → `tui.theme` (applied live).
- **Saving:** changes are staged, not written, until `ctrl+s` (unlike the WebUI, which saves immediately). Leaving with unsaved changes asks: write · discard · stay **(proposal)**.

### WebUI — full page with sidebar
Sidebar: Providers, Layers, Permissions, Subagents, Appearance (each with a one-line live summary), "Back to <workspace>", file paths at the bottom. Top bar: search, "Saved" status (settings save immediately, atomic write as in `settings::save`).

- **Gateways** (`models.json → providers`, catalogue only) — list (status, base URL, "used by N") + detail: Base URL, API key env + key source (environment / auth store, never shown), catalog format, connect / request timeout (instinct ignores it, 2 s), headers table, **catalogue grouped by provider** (collapsible; id, full name, API, context, variants), "Used by" chips (layers + agents). Actions: Test connection, Refresh catalog, Add gateway.
- **Layers** — four cards (layer, API tag, current model, note). Selecting:
  - Reflex / Instinct / Metacognition: gateway › provider, API locked to the layer, model, context, max output, "one model at a time", key `config.json → layers.<layer>`. Metacognition adds **Variant** buttons from the model (`low` tagged default). Reflex lists editors (VS Code connected; Zed, IntelliJ planned).
  - Cognition: default agent; agents table (agent, mode, model, gateway, variant, "set default" — primary/all only); detail: model with note "saved in `config.json → agents.<name>.model`, wins over `model:` in `<name>.md`", variant buttons from the model, mode `primary · subagent · all`, "up to N subagents per session" → Subagents.
  - Below either: **catalogue picker** grouped gateway › provider (collapsible), filtered by the target's API; clicking assigns the model to the selected layer or agent.
- **Permissions** — a read-only card explains how a call is decided (ADR 0015), in three steps — **1 Rules** (a deny stops the call; credentials and WithNav's own configuration always need the Captain), **2 Permission flow** (Instinct judges whether the Captain asked for it, or whether it is safe), **3 Captain** (Once, Always or Reject). Rules are managed with `nav permissions`. `permissions.scratch_dirs` list (absolute paths, empty = off, reset to default). `permissions.automatic_review` and `permissions.review_timeout_secs` were removed with the per-call review.
- **Subagents** — `subagents.max_concurrent` stepper 1–64 (default 4) with slot preview; `subagents.watchdog_interval_secs` and `agent.watchdog_interval_secs` presets Off / 1 min / 5 min / 15 min / 1 h (default 300, 0 disables).
- **Appearance** — four theme cards with mini previews; writes `tui.theme` (`ayu-dark`, `ayu-light`, `nord-dark`, `nord-light`).

### VS Code — native-style settings webview
An editor tab "WithNav Settings" that mimics the VS Code settings editor: search box with count, "open JSON" icon, scope tab, TOC on the left (Gateways › each gateway; Layers › reflex, instinct, metacognition, cognition › agents; Permissions; Subagents; Appearance), rows on the right as `Category › sub: **Title**`, description, control (text, number, dropdown, checkbox, list with "Add Item"), blue left bar + "Modified" for non-default values. The extension's own editor settings (`withnav.idleMs`, `withnav.reflex.enabled`, …) stay in VS Code settings; link to them from the TOC footer. Use VS Code theme colors (`--vscode-*`) in the real webview.
