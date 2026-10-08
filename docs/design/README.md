> **Rebrand, Oct 2026.** Navai is now **WithNav** (withnav.dev). The main agent is **Nav**; the crew is **Vigie** (The Lookout, read-only scouting, was `explore`/`librarian`), **Lux** (The Expert Advisor, was `review`/`oracle`) and **Mousse** (The Lightweight Deckhand, new, small model for local edits). Extended crew, on demand: **Cartographe** (The Cartographer, maps modules and data flows) and **Sondeur** (The Deep Diver, root causes). The user is **the Captain**. Paths: `.withnav/`, `~/.config/withnav/`, CLI `nav`, env `WITHNAV_*`. Layers (reflex, instinct, cognition, metacognition) keep their technical names. Full identity: `WithNav Brand.dc.html`. Logo files: `logo/` (wordmarks are outlined paths, no font needed).

# Handoff: WithNav — TUI, VS Code extension, WebUI

> **Three design files in this bundle.**
> - `WithNav.dc.html` — sessions, model picker, palette, subagent view, pipeline editor (this README).
> - `WithNav Settings.dc.html` — Settings, TUI, WebUI and VS Code (see **SETTINGS.md**).
> - `WithNav InstinctFlow.dc.html` — the InstinctFlow editor, WebUI (see **INSTINCTFLOW.md**).
>
> **Vocabulary update:** no model *aliases*. Reflex, Instinct and Metacognition are **layers** with exactly one model each (fim, systemone, chat). The Cognition runs **agents** (build, plan, vigie, lux, mousse, cartographe, sondeur), each with its own model and variant, and has a `default_agent`. Models are named `<gateway>/<id>` and listed grouped by gateway, then provider. Storage and permissions: see SETTINGS_AND_FLOWS.md.

## Overview
WithNav is the pair-programming system built on `rust-agent` (repo `PixmaNts/Navai`). The human (the Captain) keeps the keyboard; four layers assist: **Reflex** (ghost-text suggestions), **Instinct** (typed decisions), **Metacognition** (line-anchored Remarks, can Wake Nav) and **Cognition** (the agentic chat, with build/plan agents, subagents, tools, skills, MCP, plugins).

This bundle covers three clients of the same headless core (HTTP/SSE session server + LSP):
1. **TUI** — Rust, Ratatui
2. **VS Code extension** — TypeScript, extends the existing `vscode-withnav/`
3. **WebUI** — TypeScript (no stack chosen yet; suggestion below)

## About the design files
`WithNav.dc.html` is a **design reference built in HTML**. It shows the intended look and behavior. It is not production code to copy. Recreate it in each target environment with that environment's own patterns:
- TUI: Ratatui widgets, crossterm key handling.
- VS Code: native VS Code UI where it exists (TreeViews, QuickPick, decorations, status bar, InlineCompletionItemProvider), and the existing chat **webview** (`chatView.ts`, `chatProtocol.ts`) for the Nav panel.
- WebUI: a TS SPA. Suggested: Vite + React (or Solid) + TanStack Query; consume the existing SSE event stream from `src/server/`.

Open the file in a browser (keep `support.js` next to it). Use the top bar to switch **surface** (TUI / VS Code / WebUI), **theme**, and **screen** (Session, Model picker, Command palette, Subagent view, Pipeline editor).

## Fidelity
**High-fidelity** for layout, hierarchy, copy, vocabulary and color roles. Pixel values are a guide. In the TUI everything maps to a character grid. In VS Code, defer to the user's VS Code theme for chrome and use WithNav colors only for WithNav-owned elements (remarks, wake, drift).

## Vocabulary (use exactly — from CONTEXT.md)
Captain, Nav, Reflex, Instinct, Metacognition, Cognition, Suggestion, Remark, Wake, Verdict (useful / noise / dismissed), Delegation, Intent record, Live state, Trigger, Pipeline, Question, Branch, Action (silent / remark / wake Nav), Replay. Named agents on top of Cognition: **build** (all tools, edits ask permission) and **plan** (read-only, writes to the intent record). Subagents: `vigie`, `lux`.

## Screens

### Shared across surfaces
| Screen | What it shows |
| --- | --- |
| Session | Captain ↔ Nav transcript, streamed. Tool calls collapsed into a list with status (✓ done, ⠹ running). Permission prompt for edit/write/bash (allow once / always for folder / deny; blocked in plan mode). Remark block (accent left rule) with Verdict actions. Wake block (red left rule) "metacognition → nav · drift 0.86". |
| Model picker | **Session override** for the current agent (tabs: build, plan; active one marked). Chat models only, **grouped by gateway › provider, collapsible**, named `<gateway>/<id>`, with "used by". Variant = those offered by the model. Never writes `config.json`; footer says "session override · config.json unchanged" with **Back to config**. Layers and agent defaults are changed only in Settings. |
| Command palette | Grouped commands (Agent, WithNav, Session, Extensions) with key hints: switch agent, select model, open subagent, delegate, edit intent record, edit pipeline, replay, mark remark useful, new/open conversation, run skill, MCP servers. |
| Subagent view | Breadcrumb `parent › @vigie`, status (running · elapsed), task text, spawned-by, tools, access (read-only), delivery mode (next-turn / steer / interrupt), model; tool-call list; findings so far (file:line links); note that the result returns as `subagent.done` on the parent's next turn; Steer / Stop. |
| Pipeline editor | See WebUI. TUI has a text version (`ctrl+l`). |

### TUI (Ratatui)
Layout, full terminal:
- Row 1 header: `◆ withnav`, cwd, branch + dirty count, active agent (`▌build` accent / `▌plan` blue), `nav · <model> · <variant> · tokens/context`.
- Left column (~22 cols) **tab sidebar**: open sessions numbered 1…9, each with `agent · state`; subagents nested under their parent (`└ vigie`, turquoise). Footer hints `alt+1…9 switch · alt+w close · alt+b hide bar`. Tabs live in the sidebar, not the top bar.
- Center: transcript (or pipeline / subagent view).
- Right column (~36 cols): Intent record + drift bar (`█████████░ 0.86`), Layers status dots, Agents, Changes (M/A), Skills · MCP · Plugins.
- Input line `❯` with `@file /command !shell` hint.
- Bottom key bar: `tab build/plan · ctrl+p palette · ctrl+l pipeline · ctrl+t subagents · /models · /config`.
- Overlays (palette, model picker) are centered bordered popups with an accent border; `esc` closes.

### VS Code extension
- Activity bar container **WithNav** (`media/withnav.svg`, from `logo/withnav-mark-mono.svg`).
- Sidebar TreeView sections: Intent record (text + drift bar, "drift 0.86 · 3 remarks · 1 wake"), **Conversations · <workspace>** (title, relative time, message count; + new, search; selected row has accent left border — data from `listConversations`/`ConversationSummary`), Layers, Remarks (with verdict icon), Agents, Skills · MCP · Plugins.
- Editor: Remark rendered as end-of-line decoration (`◇ contradicts intent: public API`) + wavy accent underline on the range; hover/peek card with **Useful / Noise / Dismiss** (existing commands `withnav.markUseful/markNoise/dismiss`, Ctrl+Alt+Y/N/D). Reflex suggestion as native inline completion (ghost text, Tab).
- Right: Nav webview — header `Nav · <conversation title>` + history icon + build/plan segmented; message list; editor-context chip (`config.rs:42 · 3 files open`); composer. Subagent view replaces the message list with a breadcrumb back to the parent.
- Command palette and model picker = native **QuickPick** (`WithNav: …` commands; model picker is a 2-step QuickPick: model, then variant).
- Status bar item: `Nav · drift 0.86 · click to set intent` (existing `withnav.setIntent`).

### WebUI
Grid: top bar / left sidebar 240px / main / right panel 360px.
- Top bar: logo, breadcrumb `<workspace> / <session>`, Agent segmented build/plan, ⌘K.
- Left sidebar: **Workspaces** (folder rows, caret, session count) → expanded workspace shows path · branch, "New session", and its sessions (title + "Today · 14 msgs · drift 0.86"). Sessions are always scoped to a workspace.
- Main: transcript as described in Session; composer with agent chip, **model · variant chip (opens model picker popover anchored above the composer)**, Delegate, Send.
- Right panel tabs: Intent (intent.md sections intent / plan / decisions + drift chart with wake threshold), Agents (build, plan, subagents, `.withnav/agents/*.md`), Pipeline (summary), Extensions (Skills, MCP servers, JSON-RPC plugins with hooks), Config (model aliases, tool permissions allow/ask, pipeline timings).
- **Pipeline editor page** (full width of main + right): header with `<ws>/.withnav/pipeline.toml`, trigger set tabs (Edit triggers | Conversation trigger), Revert / Replay / Save. Three columns:
  - Left 220px: trigger events (checkbox list: save, idle, diagnostic, conversation), pre-filters (`min_changed_lines 3`, `cooldown 45 s`; conversation: `30 s`), state prompt file + included context.
  - Center: **Questions** cards (name mono, answer type tag score/choice/yes-probability, prompt, guard `needs has_intent`) and **Branches** ordered rows (n, condition, guard, threshold marker on a 0–1 track, action pill colored silent/remark/wake).
  - Right 320px: **Replay** — previous vs this pipeline remark counts, and a per-trigger list `time · event · file · answers · prev → now action · Captain verdict`.
- Command palette: centered modal with dim backdrop.

Default pipeline values come from `src/navigator/default_pipeline.rs` — keep them in sync, don't hard-code in UI.

## Interactions & state
- `surface`-independent client state: `agentMode: build|plan`, `screen`, `activeLayer`, `assign[layer] = {model, variant}`, `activeConversation`, `activeWorkspace`, `activeSession`, `tuiTabs[]`, `pipelineTrigger: edit|conversation`.
- Permission prompt: pending → allowed once / always (folder) / denied (Nav proposes an alternative). In plan mode edits never prompt; they are added to the plan.
- Remark verdict: pending → useful / noise / dismissed, with undo; send `withnav/feedback`.
- Wake: shown in the transcript as a message whose author is `nav` (chatProtocol `Author`).
- Selecting a model keeps the current variant if the model supports thinking, else variant = none.
- Overlays close on backdrop click / `esc`.

## Design tokens — themes
Eight themes (Ayu, Nord, Tide, Moss — each dark and light), same role names. The theme switcher is a family picker plus a dark/light toggle. Implement as CSS custom properties (WebUI, webview) and as a `Theme` struct of `ratatui::style::Color::Rgb` (TUI).

| Role | Ayu Dark | Ayu Light | Nord Dark | Nord Light | Tide Dark | Tide Light | Moss Dark | Moss Light |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| bg | #0b0e14 | #fcfcfc | #2e3440 | #eceff4 | #061115 | #f2f8f8 | #0b100c | #f7f5ee |
| panel | #0f131a | #f3f4f5 | #292e39 | #e5e9f0 | #0a171b | #e9f2f3 | #101511 | #efede3 |
| surface | #151a23 | #ffffff | #3b4252 | #ffffff | #101e23 | #fbfefe | #161d17 | #fdfcf7 |
| border | #232a36 | #dfe2e6 | #434c5e | #d0d7e3 | #213135 | #cddbdc | #262e27 | #d7d5c7 |
| text | #bfbdb6 | #4a5058 | #d8dee9 | #2e3440 | #d5e1e1 | #1f3137 | #e6e1cf | #293027 |
| muted | #7b8494 | #6e7781 | #8f9bb3 | #586174 | #87999e | #53676c | #909487 | #5f665a |
| accent | #e6b450 | #ffaa33 | #88c0d0 | #5e81ac | #5ecbcb | #1d9999 | #75d78d | #429c5a |
| accent text | #e6b450 | #a35f00 | #88c0d0 | #3f6591 | #5ecbcb | #00696f | #75d78d | #197037 |
| on accent | #0b0e14 | #1f2328 | #2e3440 | #eceff4 | #061115 | #081e24 | #0b100c | #141d16 |
| green (ok, useful) | #aad94c | #5c8a00 | #a3be8c | #4f7a3c | #81d584 | #2e7c35 | #c8df58 | #577600 |
| red (wake, error, noise) | #f07178 | #d94f4f | #bf616a | #b0424d | #ee6570 | #c53443 | #ea6a64 | #c13c3b |
| blue (code refs, plan, links) | #59c2ff | #1a7bbf | #81a1c1 | #3f6591 | #6cb2ec | #0f68a2 | #6eb9dc | #136b94 |
| purple (subagents) | #d2a6ff | #8052b8 | #b48ead | #7d5a78 | #b59be6 | #6f4fa1 | #be99da | #784d96 |
| orange (warnings, numbers) | #ff8f40 | #c25d00 | #d08770 | #a85a3f | #f69370 | #c1552c | #ebb25f | #ae6700 |
| logo needle (Captain) | = `text` in each theme |
| logo rings (Nav) | = `la` in each theme |

Tints: selected / active backgrounds = accent at 14–16% over transparent; remark block = accent 12% → transparent horizontal gradient; wake block = red 10%.

Type: UI `IBM Plex Sans` 13px (11px uppercase-ish section labels in muted, 12–12.5px secondary); code/TUI `JetBrains Mono` 12.5px, line-height 1.55–1.75. Weights 400/500/600 only.
Radii: 4–6px controls, 8px panels, 10–14px pills. Spacing is compact: 4/6/8/10/12/14/16/20/24px.
Icons: Phosphor (regular).

## Logo
Logo **A · Compass beacon** (see `WithNav Brand.dc.html` › Logo): the needle is the Captain's course, in `text`; the rings above are Nav's light, in `la`. Wordmark "WithNav" in Bricolage Grotesque 600, −0.02em. At 20 px and below use the two-ring variant (`withnav-favicon.svg`).

In the UI, inline the mark SVG and fill it with `--tx` (needle) and `--la` (rings) so it follows the theme. TUI keeps a text mark: `◆` in `la` + `withnav` in `text`.

- `logo/withnav-*.svg` — mark (dark, light, mono), favicon (adapts to color scheme), app icons, wordmarks (`withnav-wordmark-*`, symbol + name) and name-only `withnav-wordtype-*`, all outlined to paths. `logo/agents/` — Nav, Vigie, Lux, Mousse, Cartographe, Sondeur and Captain glyphs, dark/light/mono.

## Files
- `WithNav.dc.html` — sessions, pickers, palette, subagent view, pipeline editor (all surfaces, themes).
- `WithNav Settings.dc.html` — Settings, TUI, WebUI and VS Code.
- `WithNav InstinctFlow.dc.html` — InstinctFlow editor, WebUI.
- `SETTINGS.md`, `INSTINCTFLOW.md` — specs for the two pages above.
- `WithNav Brand.dc.html` — brand book: positioning, crew, three logo directions, palette, type, icons, landing, interface, microcopy, metaphor rules (FR/EN). Uses `WithNavGlyph.dc.html`.
- `logo/` — SVG exports.
- `support.js` — runtime needed to open the reference in a browser.

## Suggested implementation order
1. Shared: theme tokens (Rust struct + CSS vars) and the client state model above; confirm server events cover everything (subagent status, permissions, verdicts, pipeline CRUD, replay).
2. TUI session screen + tab sidebar, then overlays (palette, model picker), subagent view, pipeline text view.
3. VS Code: conversations TreeView, QuickPicks, remark decorations; extend the chat webview with agent mode, model chip and subagent view.
4. WebUI: workspaces → sessions shell, session screen, model popover, palette, then the Pipeline editor page with Replay.
