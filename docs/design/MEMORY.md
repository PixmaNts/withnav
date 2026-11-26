# Prompt for Claude Design: WithNav web UI, memory, goal watch and permission review

Design new screens for the WithNav web UI. WithNav is a pair-programming assistant. The human is
**the Captain**; the agent he talks to is **Nav**. Four layers assist:

- **Reflex**: ghost-text suggestions.
- **Instinct**: a fast model that makes typed decisions.
- **Metacognition**: one chat model that reviews and remarks.
- **Cognition**: the agent sessions, with subagents named Vigie, Lux, Mousse, Cartographe and
  Sondeur.

Use the existing WithNav design files as the visual source of truth: `WithNav Brand.dc.html`,
`WithNav.dc.html`, `WithNav Settings.dc.html` and `WithNav InstinctFlow.dc.html`. They share:

- the same top bar with **screen** and **theme** switchers;
- the theme tokens (Ayu Dark/Light, Nord Dark/Light);
- the fonts: Bricolage Grotesque for display, IBM Plex Sans for UI, JetBrains Mono for code and ids;
- Phosphor icons;
- the crew colors: Nav cyan, Lux yellow, Mousse coral, Vigie green, Cartographe violet, Sondeur
  periwinkle.

Deliver one file, **`WithNav Memory.dc.html`**, in the same format, with a screen switcher covering
the screens below in dark and light themes. Treat it as a design reference, not production code.
The web UI is React with TanStack Query and hash routes (`#/chat`, `#/settings`, `#/studio`). Add
`#/memory` and `#/projects`.

## Principles (they shape every screen)

- **The Captain stays in control, and he rarely rereads.** Memory must be trustworthy without
  constant review:
  - every remembered item shows **who said it**, as an authority tag: `captain said`,
    `captain accepted`, `observed` or `assumed`;
  - every item shows the **exact quote** it comes from, with a link to the source conversation;
  - every action has **undo**.
- **Nothing is ever overwritten.** An update or an invalidation is a new dated version, and the
  history is always one click away.
- **Memory never authorizes anything.** It informs Nav; it is never a permission.
- **Quiet by default.** Things that need the Captain are few and clearly separated from everything
  else.
- Use the WithNav vocabulary, in English: Captain, Nav, Remark, Goal, Criterion, Ledger, Decision,
  Scope, Group, Project, Session.

## 1. Memory page (`#/memory`)

A full page with the usual sidebar. Three areas.

**Filters and search (top).**
- Text search across title, body and quote.
- Scope chips: `global` · `group` · `project` · `session`.
- Kind chips: `decision` · `convention` · `preference` · `environment` · `pitfall`.
- Status: `active` (default) · `proposed` · `superseded` · `invalidated` · `expired`.
- Authority filter. "Pinned" toggle.

**Card list (left / center).** One row per item:
- title;
- scope badge, with the group or project name when relevant;
- kind icon and authority tag;
- current version number, date, and the session it came from;
- a small "seen in N sessions" counter.

Superseded items are dimmed and linked to their replacement.

**Card detail (right).**
- Current version: title, **Decision** sentence, **Why** (or "not stated"), the **quote** in a
  quote block with "open in session", category and horizon (task / feature / project).
- **Version timeline**, vertical. Each entry shows its change (`created` · `updated` ·
  `invalidated` · `restored` · `rescoped`), who made it (the Captain, or "proposed by Nav,
  accepted by the Captain"), the date, the session, and the reason. Select two versions to see a
  diff.
- **Relations:** `supersedes` / `superseded by`, `conflicts with`, `scoped`, each a link.
- **Seen by:** the sessions and turns this item was sent to Nav in, with the reason it was picked
  (scope, match, pinned).
- **Actions:** Edit (creates a new version and asks for a reason), Invalidate (asks for a reason),
  Restore, Change scope, Pin, Copy id. Each action shows an undo toast.

Mock data, about 15 items across all scopes, for example:
- "Inspect OmniRoute and agent configs over Tailscale only" (environment, global);
- "Kyber projects use conventional commits with a scope" (convention, group `kyber`);
- "Passive goal notes stay off until the shadow results show they are useful" (decision, project
  WithNav);
- "Permission `requested` is judged against the conversation, not the command" (decision, project
  WithNav, version 2, superseding a "fixed threshold 0.8" version);
- "Prefers tmux sessions per role for long work" (preference, global);
- "`cargo test` needs `--features test-support`" (pitfall, observed, project WithNav);
- one `assumed` item, dimmed;
- one `proposed` item, expiring in 3 days.

## 2. "Needs you" (in the Memory page and as a sidebar badge)

A short, capped list, never a backlog, of the only cases that ask the Captain:
- a proposed item with **group or global** scope;
- a new item that would **supersede** or **conflict with** an existing one: show both side by
  side;
- an agent proposal the Captain only answered "ok" to (`captain accepted`): confirm the wording;
- any item extracted automatically from the conversation: it stays **proposed** until the Captain
  accepts its exact wording and scope. Items the Captain writes himself (`/remember`) are active at
  once.

Each row has one-click **Accept** · **Edit then accept** · **Reject**, and shows its expiry ("expires
in 9 days"). Include an empty state that feels calm.

## 3. Projects and groups (`#/projects`)

- **Groups** in the left column, e.g. `kyber`, `infra`, `perso`, plus "No group". Each shows its
  member count and the items scoped to it.
- **A project** is a git repository: show its name, its identity (the repository path), its
  **worktrees** listed under it, its sessions (recent first, with the agent and status), and its
  groups.
- Actions:
  - add or remove a project from a group;
  - attach or detach a session to a project;
  - create a group;
  - rename.
  Membership changes are kept as history; show a small "history" disclosure.
- **The new-project question:** a dialog shown the first time a session starts in an unknown
  repository: "Which group does `<repo>` belong to?". It offers the existing groups, a new group,
  or no group, and a note: "Items scoped to the group are shared with its projects; global items
  are shared everywhere."

## 4. Session view additions (`#/chat`)

Extend the existing session screen (transcript, composer, right panel). Do not redesign it.

- **Right panel, new sections**, in the existing style:
  - **Goal**: objective (up to 3 lines, expandable), revision, status (`active` / `paused`), and
    the criteria. Each criterion carries its last check status: `supported` (green, with the
    evidence on hover), `contradicted` (red), or `not checked` (muted). Show "last checked 2 min
    ago".
  - **Ledger**: turns, files changed (count and the last 3), repeated failures (count and the worst
    one), open questions.
  - **Memory**: the items sent to Nav this turn, titles with authority tags, and "why picked" on
    hover; a link to the Memory page.
- **Transcript inline lines**, small and quiet:
  - `Remembered: "<title>" · project WithNav · undo`, for an item the Captain wrote with
    `/remember`;
  - `Proposed to remember: "<title>" · review`, for an item extracted from the conversation;
  - a **goal note** (an automatic observation, not a Captain message), rendered like a flow note,
    e.g. "Goal check (revision 3): criterion 2 'Windows CI passes' is contradicted — `cargo test`
    exited 101."
- **Command palette** entries: `/goal`, `/remember`, `/forget`, `/memory`.

## 5. Permission card with a parallel metacognition review

Today a permission card asks the Captain: allow once, always, or reject. New behavior, for calls
that may be destructive and are not covered by the Captain's request: the Captain is asked **and**
the metacognition model reviews the call **at the same time**. The first decisive answer wins.

Design every state of the card:

1. **Asked and reviewing.** The usual buttons stay active. A slim "Metacognition reviewing…" line
   with a spinner. The Captain can answer at any time.
2. **Review allowed, Captain away.** The card closes as "Allowed by Metacognition", with the
   reasoning (2–3 sentences, collapsible) and the time. The transcript keeps a compact record.
3. **Review says wait.** The card stays open with a **Waiting for you** badge and the review's
   reasoning shown **above the buttons** to guide the Captain, e.g. "Deletes files in another
   tool's config directory; the Captain asked to fix warnings, not to remove these files."
4. **Captain answered first.** The review line becomes "Review cancelled".
5. **Review timed out or failed.** A neutral "No review; your call" line.

For a **compound command** (`a && b; c`), show the line split into its commands, as an
explanation only: the call always runs or is refused as a whole. Show destinations and
redirections, and the consequence in one line (e.g. "publishes a comment on GitHub", "deletes 3
files outside the project"). Highlight the part that caused the question. Keep the full line
visible.

More rules for this card:
- The metacognition can only **allow once** or **wait**, never "always".
- The Captain has a **Hold for me** action next to the usual buttons. If the call has already
  started when he answers, the card says "already started" and never implies it was undone.
- The reasoning is a short conclusion: the request it relied on and the consequence. Never a long
  chain of thought.

Fixed rules shown in small print on protected calls (credentials, WithNav's own config):
"Protected: only you can answer."

## 6. Notifications

When a card is **Waiting for you** and the Captain is not looking:
- a browser notification "Nav is waiting for you in `<session>`: `<short call>`";
- a badge on the session in the sidebar;
- a global badge in the top bar listing every waiting session across workspaces;
- a mute toggle per session.
- One notification per blocked turn, never one per review. A notification closes in every client
  once the card is answered anywhere.

Design the notification itself and the in-app list.

## States to include

Empty states (no memory yet, no goal, no groups), loading, an error (server unreachable), dark and
light themes, and a narrow-width layout (≥ 900 px; the right panel collapses).

## Copy

Short, plain, English. No marketing language. Dates are relative, with the exact time on hover.
Ids in JetBrains Mono.
