# Handoff: Navigator — TUI, VS Code extension, WebUI

> **Design reference, not as-built.** Decisions and the authoritative
> `.scratch/models-layers-agents/spec.md` (0.5.0 target), together with
> [`CONTEXT.md`](../../CONTEXT.md), take precedence over these mocks. The spec lives in the
> main checkout's scratch tracker; it is not bundled here. Existing code describes as-built
> behavior, not authority to override agreed decisions. The 0.5.0 foundations are not claimed
> complete. Settings and the flow editor are **0.6.0 proposals**, after their own grilling/spec.
> HTML is reference only; remaining copy/code discrepancies are not implementation requirements.

> **Two design files in this bundle.**
> - `Navigator.dc.html` — sessions, model picker, palette, subagent view, pipeline editor (this README).
> - `Navai Settings and Flows.dc.html` — Settings (WebUI + VS Code) and the Jev flows editor (see **SETTINGS_AND_FLOWS.md**).
>
> **Vocabulary update:** no model *aliases*. Reflex, Instinct and Metacognition are **layers** with exactly one model each (fim, systemone, chat). The Cognition runs **agents** (build, plan, librarian, explorer, oracle…), each with its own model and variant, and has a `default_agent`. Models are named `<gateway>/<id>` and listed grouped by gateway, then provider. Storage and permissions: see SETTINGS_AND_FLOWS.md.

## Overview
Navigator is the pair-programming system built on Navai (repo `PixmaNts/Navai`). The human (Driver) keeps the keyboard; four layers assist: **Reflex** (editor-only FIM ghost-text suggestions), **Instinct** (System One typed decisions), **Metacognition** (chat, line-anchored Remarks, can Wake the cognition) and **Cognition** (the agentic chat, with build/plan agents, subagents, tools, skills, MCP, plugins).

This bundle covers three clients of the same headless core (HTTP/SSE session server + LSP):
1. **TUI** — Rust, Ratatui
2. **VS Code extension** — TypeScript, extends the existing `vscode-navigator/`
3. **WebUI** — TypeScript (no stack chosen yet; suggestion below)

## About the design files
`Navigator.dc.html` is a **design reference built in HTML**. It shows the intended look and behavior. It is not production code to copy. Recreate it in each target environment with that environment's own patterns:
- TUI: Ratatui widgets, crossterm key handling.
- VS Code: native VS Code UI where it exists (TreeViews, QuickPick, decorations, status bar, InlineCompletionItemProvider), and the existing chat **webview** (`chatView.ts`, `chatProtocol.ts`) for the Cognition panel.
- WebUI: a TS SPA. Suggested: Vite + React (or Solid) + TanStack Query; consume the existing SSE event stream from `src/server/`.

Open the file in a browser (keep `support.js` next to it). Use the top bar to switch **surface** (TUI / VS Code / WebUI), **theme**, and **screen** (Session, Model picker, Command palette, Subagent view, Pipeline editor).

## Fidelity
**High-fidelity** for layout, hierarchy, copy, vocabulary and color roles. Pixel values are a guide. In the TUI everything maps to a character grid. In VS Code, defer to the user's VS Code theme for chrome and use Navigator colors only for Navigator-owned elements (remarks, wake, drift).

## Vocabulary (use exactly — from CONTEXT.md)
Driver, Navigator, Reflex, Instinct, Metacognition, Cognition, Suggestion, Remark, Wake, Verdict (useful / noise / dismissed), Delegation, Intent record, Live state, Trigger, Pipeline, Question, Branch, Action (silent / remark / wake cognition), Replay. Named agents on top of Cognition: **build** (all tools, edits ask permission) and **plan** (read-only, writes to the intent record). Subagents: `explore`, `review`.

## Screens

### Shared across surfaces
| Screen | What it shows |
| --- | --- |
| Session | Driver ↔ Cognition transcript, streamed. Tool calls collapsed into a list with status (✓ done, ⠹ running). Permission prompt for edit/write/bash and gated reads (allow once / deny; reusable origin approval only for eligible fetch). Config mutation stays driver-only; bash review is best effort, not confinement. Remark block (accent left rule) with Verdict actions. Wake block (red left rule) "metacognition → cognition · drift 0.86". |
| Model picker | **Session override** for the current agent (tabs: build, plan; active one marked). Chat models only, **grouped by gateway › provider, collapsible**, named `<gateway>/<id>`, with "used by". Variant = those offered by the model. Never writes `config.json`; footer says "session override · config.json unchanged" with **Back to config**. Layers and agent defaults are changed only in Settings. |
| Command palette | Grouped commands (Agent, Navigator, Session, Extensions) with key hints: switch agent, select model, open subagent, delegate, edit intent record, edit pipeline, replay, mark remark useful, new/open conversation, run skill, MCP servers. |
| Subagent view | Breadcrumb `parent › @explore`, status (running · elapsed), task text, spawned-by, tools, access (read-only), delivery mode (next-turn / steer / interrupt), model; tool-call list; findings so far (file:line links); note that the result returns as `subagent.done` on the parent's next turn; Steer / Stop. |
| Pipeline editor | See WebUI. TUI has a text version (`ctrl+l`). |

### TUI (Ratatui)
Layout, full terminal:
- Row 1 header: `◆ navigator`, cwd, branch + dirty count, active agent (`▌build` accent / `▌plan` blue), `cognition · <model> · <variant> · tokens/context`.
- Left column (~22 cols) **tab sidebar**: open sessions numbered 1…9, each with `agent · state`; subagents nested under their parent (`└ explore`, purple). Footer hints `alt+1…9 switch · alt+w close · alt+b hide bar`. Tabs live in the sidebar, not the top bar.
- Center: transcript (or pipeline / subagent view).
- Right column (~36 cols): Intent record + drift bar (`█████████░ 0.86`), Layers status dots, Agents, Changes (M/A), Skills · MCP · Plugins.
- Input line `❯` with `@file /command !shell` hint.
- Bottom key bar: `tab build/plan · ctrl+p palette · ctrl+l pipeline · ctrl+t subagents · /models · /config`.
- Overlays (palette, model picker) are centered bordered popups with an accent border; `esc` closes.

### VS Code extension
- Activity bar container **Navigator** (existing `media/navigator.svg`).
- Sidebar TreeView sections: Intent record (text + drift bar, "drift 0.86 · 3 remarks · 1 wake"), **Conversations · <workspace>** (title, relative time, message count; + new, search; selected row has accent left border — data from `listConversations`/`ConversationSummary`), Layers, Remarks (with verdict icon), Agents, Skills · MCP · Plugins.
- Editor: Remark rendered as end-of-line decoration (`◇ contradicts intent: public API`) + wavy accent underline on the range; hover/peek card with **Useful / Noise / Dismiss** (existing commands `navigator.markUseful/markNoise/dismiss`, Ctrl+Alt+Y/N/D). Reflex suggestion as native inline completion (ghost text, Tab).
- Right: Cognition webview — header `Cognition · <conversation title>` + history icon + build/plan segmented; message list; editor-context chip (`config.rs:42 · 3 files open`); composer. Subagent view replaces the message list with a breadcrumb back to the parent.
- Command palette and model picker = native **QuickPick** (`Navigator: …` commands; model picker is a 2-step QuickPick: model, then variant).
- Status bar item: `Navigator · drift 0.86 · click to set intent` (existing `navigator.setIntent`).

### WebUI
Grid: top bar / left sidebar 240px / main / right panel 360px.
- Top bar: logo, breadcrumb `<workspace> / <session>`, Agent segmented build/plan, ⌘K.
- Left sidebar: **Workspaces** (folder rows, caret, session count) → expanded workspace shows path · branch, "New session", and its sessions (title + "Today · 14 msgs · drift 0.86"). Sessions are always scoped to a workspace.
- Main: transcript as described in Session; composer with agent chip, **model · variant chip (opens model picker popover anchored above the composer)**, Delegate, Send.
- Right panel tabs: Intent (intent.md sections intent / plan / decisions + drift chart with wake threshold), Agents (build, plan, subagents, `.navai/agents/*.md`), Pipeline (summary), Extensions (Skills, MCP servers, JSON-RPC plugins with hooks), Config (layer/agent assignments, tool permissions, navigator timings; 0.6.0 proposal).
- **Pipeline editor page** (full width of main + right): header with `<ws>/.navigator/pipeline.toml`, trigger set tabs (Edit triggers | Conversation trigger), Revert / Replay / Save. Three columns:
  - Left 220px: trigger events (checkbox list: save, idle, diagnostic, conversation), pre-filters (`min_changed_lines 3`, `cooldown 45 s`; conversation: `30 s`), state prompt file + included context.
  - Center: **Questions** cards (name mono, answer type tag score/choice/yes-probability, prompt, guard `needs has_intent`) and **Branches** ordered rows (n, condition, guard, threshold marker on a 0–1 track, action pill colored silent/remark/wake).
  - Right 320px: **Replay** — previous vs this pipeline remark counts, and a per-trigger list `time · event · file · answers · prev → now action · driver verdict`.
- Command palette: centered modal with dim backdrop.

Default pipeline values come from `src/navigator/default_pipeline/` — keep them in sync, don't hard-code in UI.

## Interactions & state
- `surface`-independent client state: `agentMode: build|plan`, `screen`, `activeLayer`, `assign[layer] = {model, variant}`, `activeConversation`, `activeWorkspace`, `activeSession`, `tuiTabs[]`, `pipelineTrigger: edit|conversation`.
- Permission prompt: pending → allowed once / denied (cognition proposes an alternative). No reusable folder approvals; only eligible fetch keeps origin grants. Plan tools/access follow the agent policy, not a blanket UI exemption.
- Remark verdict: pending → useful / noise / dismissed, with undo; send `navigator/feedback`.
- Wake: shown in the transcript as a message whose author is `navigator` (chatProtocol `Author`).
- `/model` without a variant takes the new model's default variant. `/model` and `/variant` override only the current (session, agent), persisted with the session: switching away and back restores that agent's override, without editing `config.json`. Search ignores collapsed gateway/provider groups.
- Variants are model-offered catalog entries. OmniRoute `combo/*` entries declare `low`, `medium`, `high` client-side via `reasoning_effort`; imports merge and preserve manually declared variants. Agent modes are `primary`, `subagent`, `all`; `default_agent` defaults to `build`.
- Overlays close on backdrop click / `esc`.

## Design tokens — themes
Four themes, same role names. Implement as CSS custom properties (WebUI, webview) and as a `Theme` struct of `ratatui::style::Color::Rgb` (TUI).

| Role | Ayu Dark | Ayu Light | Nord Dark | Nord Light |
| --- | --- | --- | --- | --- |
| bg | #0b0e14 | #fcfcfc | #2e3440 | #eceff4 |
| panel | #0f131a | #f3f4f5 | #292e39 | #e5e9f0 |
| surface | #151a23 | #ffffff | #3b4252 | #ffffff |
| border | #232a36 | #dfe2e6 | #434c5e | #d0d7e3 |
| text | #bfbdb6 | #4a5058 | #d8dee9 | #2e3440 |
| muted | #7b8494 | #6e7781 | #8f9bb3 | #586174 |
| accent | #e6b450 | #ffaa33 | #88c0d0 | #5e81ac |
| accent text | #e6b450 | #a35f00 | #88c0d0 | #3f6591 |
| on accent | #0b0e14 | #1f2328 | #2e3440 | #eceff4 |
| green (ok, useful) | #aad94c | #5c8a00 | #a3be8c | #4f7a3c |
| red (wake, error, noise) | #f07178 | #d94f4f | #bf616a | #b0424d |
| blue (code refs, plan, links) | #59c2ff | #1a7bbf | #81a1c1 | #3f6591 |
| purple (subagents) | #d2a6ff | #8052b8 | #b48ead | #7d5a78 |
| orange (warnings, numbers) | #ff8f40 | #c25d00 | #d08770 | #a85a3f |

Tints: selected / active backgrounds = accent at 14–16% over transparent; remark block = accent 12% → transparent horizontal gradient; wake block = red 10%.

Type: UI `IBM Plex Sans` 13px (11px uppercase-ish section labels in muted, 12–12.5px secondary); code/TUI `JetBrains Mono` 12.5px, line-height 1.55–1.75. Weights 400/500/600 only.
Radii: 4–6px controls, 8px panels, 10–14px pills. Spacing is compact: 4/6/8/10/12/14/16/20/24px.
Icons: Phosphor (regular).

## Files
- `Navigator.dc.html` — the full interactive reference (all surfaces, themes, screens).
- `support.js` — runtime needed to open the reference in a browser.

## Suggested implementation order
1. Shared: theme tokens (Rust struct + CSS vars) and the client state model above; confirm server events cover everything (subagent status, permissions, verdicts, pipeline CRUD, replay).
2. TUI session screen + tab sidebar, then overlays (palette, model picker), subagent view, pipeline text view.
3. VS Code: conversations TreeView, QuickPicks, remark decorations; extend the chat webview with agent mode, model chip and subagent view.
4. WebUI: workspaces → sessions shell, session screen, model popover, palette, then the Pipeline editor page with Replay.
