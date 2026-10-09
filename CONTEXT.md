# WithNav

A pair-programming system in which the human keeps the keyboard and stays the author of the code,
while several models of different speeds watch, complete, judge and discuss. It exists to get the
speed of AI assistance without losing the developer's mental model of their own codebase.

## Roles

**Captain**:
The human writing the code. The Captain is the default author of every line.
_Avoid_: user (when authorship matters), pilot

**Nav**:
The Captain’s single point of contact, running the `build` and `plan` agents in Cognition.
WithNav is the whole assisting system; Nav guides, discusses and implements delegated work.
_Avoid_: copilot, assistant, agent (for the whole system)

## Layers

The layer, model and agent vocabulary below describes the implementation prepared for **v0.5.0**;
see [ADR 0011](docs/adr/0011-layers-models-and-agent-overrides.md). It does not describe which
version is installed on the Captain's machine.

**Layer**:
One of four cognitive roles: reflex, instinct, metacognition and cognition. The first three each
have one configured model at a time; cognition runs agents in parallel sessions. Configurable
flows are planned for 0.6.
_Avoid_: model tier, model alias

**Reflex · Completion**:
The fastest layer: completes what the Captain is typing, as ghost text, from a fill-in-the-middle
model, only in editor contexts.
_Avoid_: autocomplete layer, FIM layer

**Instinct · Triage**:
The decision layer: answers bounded, typed questions (choice, score, yes-probability) about the
current situation to sort, route, filter and gate; it never produces free text.
_Avoid_: classifier, router, Jev (Jev is one provider of this layer, not the layer)

**Metacognition · Beacon**:
The fast reflective layer: watches both the code being written and the conversation, judges whether
the code matches the intent, produces remarks, and can wake the cognition.
_Avoid_: observer, critic, flash (a flash model is its current engine, not its role)

**Cognition · Nav and the crew**:
The deliberate layer: the high-level conversation with the Captain about concepts, features, intent
and problems, with the full agentic toolset.
_Avoid_: main agent, chat, frontier (a frontier model is its engine, not its role)

## Feature labels

**Logbook · Activity history**: the recorded activity and decision history.

**Voyage · Session**: one session with its transcript, selected agent and model.

Safe Harbor · Checkpoint names the existing compaction checkpoint defined below.

## Outputs

**Suggestion**:
Ghost text offered by the reflex at the cursor; it becomes code only when the Captain accepts it.
_Avoid_: completion (ambiguous with chat completions)

**Beacon · Remark**:
A short, line-anchored Metacognition remark, signed by Nav and shown beside the code;
it never changes the code. The layer itself has no persona.
_Avoid_: hint, diagnostic, comment

**Delegation**:
The Captain explicitly asking the cognition to do a piece of work itself instead of guiding them
through it; without a delegation, the cognition assists and the Captain writes the code.
_Avoid_: autopilot, vibe mode

**Verdict**:
The Captain's rating of a remark: useful, noise, or dismissed.
_Avoid_: feedback, vote

## Crew

Each member has a name and glyph; colour alone never identifies a member.

- **Nav · The Navigator** (`build`, `plan`): the primary contact for implementation and planning.
- **Vigie · The Lookout** (`vigie`): read-only code, documentation and web scouting, with sources.
- **Lux · The Expert Advisor** (`lux`): read-only analysis and recommendations.
- **Mousse · The Lightweight Deckhand** (`mousse`): small local edits after approval.
- **Cartographe · The Cartographer** (`cartographe`): on-demand maps, writing Mermaid charts to `.withnav/charts/`.
- **Sondeur · The Deep Diver** (`sondeur`): on-demand root-cause work; tests and profilers after approval.

## Agents

The ownership and delivery decisions are in
[ADR 0009](docs/adr/0009-background-subagents-and-durable-delivery.md).
Model selection and editor context follow
[ADR 0011](docs/adr/0011-layers-models-and-agent-overrides.md).
An invalid nondefault model assignment falls back to the default agent's model with a visible
warning; an invalid default agent or unusable default model prevents startup.

**Agent**:
The role cognition plays, such as `build`, `plan`, `vigie`, `lux` or a custom role: its prompt,
tools, access and optional assigned model and variant. A configured assignment takes precedence
over the model in its markdown header.
_Avoid_: model, profile, persona

**Default agent**:
The agent a new session starts with unless the Captain chooses another: configured by
`default_agent`, or `build` when absent. It must have a usable model or WithNav cannot start.
_Avoid_: default model, default alias

**Editor session**:
A session created by an editor client, adding the pair-programmer prompt, `intent` tool and
navigator conversation attachment to its agent. This context is independent of agent and model
selection and starts on the default agent like any other session.
_Avoid_: navigator profile, session profile

**Primary agent**:
The role a session runs as: `build` (implement, verify, report), `plan` (research, write markdown
plans, delegate) or a custom one. The Captain selects it for the session and can switch between turns.
It sets what the model is told to do, not what it may execute.
_Avoid_: mode (a mode is where an agent definition may be used), persona

**Agent definition**:
A role described in a built-in or a markdown file: description, mode (primary, subagent or both),
role prompt, optional model, tool allow/deny map and access level. `build` and `plan` are reserved.
_Avoid_: profile, session type

**Subagent**:
An agent definition whose mode is `subagent` or `all`, meant to be given a task by another agent
(`vigie` and `lux` are built in and read-only). A cognition session may start one through
`task(agent, prompt)` from either the terminal or an editor root conversation. Children never
register with the conversation feed or receive the navigator's `intent` tool. Their results use
normal durable delivery, not navigator wakes; the root digest labels them as untrusted subagent text.
The subagent is a real, non-promptable child session. It has an independent transcript, an
intersected snapshot of the parent's policy, a durable parent-child link, and bounded terminal
result. `subagent.spawned` and `subagent.done` are replayable events. Results enter the parent's
transcript once, as bounded untrusted user messages at a turn boundary. `steer` delivery can use
a safe boundary within its running turn. Completion wakes an idle parent, but cannot answer its
pending interaction. Child permissions are answered by the Captain
through the parent's API, with the interaction and grants remaining on the child. Parent stop and
delete cascade; restart stops unfinished children without replaying their tools.
_Avoid_: worker, child agent

**Completion notice**:
Watchdog doubts also use the durable outbox to ask the parent model for a decision, independently
of terminal completion; their notice ids are distinct from the child's id.
A durable outbox entry containing the child's id, outcome, agent and result, independent of the
child's continued existence. `delivered` means its notice event (`subagent.done` or
`subagent.watchdog`) was committed to the parent stream. `consumed` means its result message and
durable `message.received` event were committed atomically into the parent's transcript and stream.
These are separate recovery boundaries. Several notices are consumed in order and never use the
navigator's latest-wins inbox. Deleting the parent removes its notices.
_Avoid_: tool result (the `task` tool already returned the child id), wake message

**Safe boundary**:
The point before an agent's next provider request, after every call in the previous tool batch has
its result and with no pending ask or permission. Injected guidance and eligible results enter
here, before budget estimation and automatic compaction. Never between a tool call and its result.
_Avoid_: step event (observation is not an injection boundary)

**Steer**:
Parent guidance injected into a running child's transcript at its next safe boundary, without
restarting the turn. A parked interaction or watchdog doubt keeps it queued. Guidance is queued
in memory and can be lost in a crash. `Parent` provenance prevents guidance
from replacing the Captain's instruction for compaction or the plan guard. A child result whose
delivery mode is `steer` uses the same boundary on the parent, with untrusted subagent provenance.
_Avoid_: Captain message, permission answer

**Interrupt**:
Cancel only the current turn, settle its outstanding tool results, record interrupted `turn.end`,
renew the turn cancellation token and start fresh with the new input. The session and child link
stay live. An explicit parent instruction may cancel a child's pending interaction; a delivered
child result never cancels the parent's human interaction, instead waiting for the next turn.
_Avoid_: stop (stop tears down the session)

**Watchdog**:
Root conversations have no default step cap. Their watchdog uses the same bounded observable
evidence every 300 seconds (`agent.watchdog_interval_secs`; 0 disables), bound to the judged turn.
`active` continues; `stuck` stops before the next provider request once every tool result is in.
`doubt`, including two consecutive judge failures, gets one tool-free metacognition review with a
20-second deadline. Continue resumes with the remaining steps; stop settles the batch and stops; hesitation, invalid
output, timeout or an unavailable reviewer pauses at that boundary and asks the Captain to Continue
(`alt+c`) or Stop. Permission and ask waits are not judged or answered by this mechanism.
At the settled boundary, a pause snapshots the transcript, publishes a typed yield with its turn
identity and unique incarnation/hold token, and ends the turn as yielded. Owner Continue consumes
that token once; late answers cannot release a newer pause. Resolution, expiry and Captain-message
supersession are sequenced notices. Restart expires the hold, retains the checkpoint and waits
for fresh Captain input. Navigator wakes and child results cannot release a hold or renew a parked
budget. A real Captain message replaces the paused turn and records its interruption.
Recovery installs a separate fresh-Captain gate even after publishing expiry; pending child
completions and autonomous inbox messages stay held until HTTP or CLI Captain input clears it.
Only a consumed instruction checkpointed atomically with its gate-release event clears recovery's
gate. Durable `message.received` is admission, not proof of transcript consumption. A crash before
that checkpoint keeps the gate closed on the next recovery. Owner Continue on a budget-parked
child claims accepted interrupt replacements exactly once before requesting another model step.
Supersession names the old turn in the pause-resolution event, never a second terminal event.
Accepted deferred model/compact operations receive correlated cancellation at a watchdog pause.
Without instinct, or with supervision disabled, the session announces a renewable 500-step fallback
cap instead. Only explicit owner Continue or fresh Captain input renews it. An explicit
`WITHNAV_MAX_STEPS` keeps the existing budget parking behaviour; subagents keep their 50-step default.

The instinct's separate progress check for a running background child, every 300 seconds by
default (`subagents.watchdog_interval_secs`; 0 disables). It reads a bounded goal, time since
message/tool activity, last tool call and result, and step count, never reasoning. `active` does
nothing; `stuck` terminalizes the child with reason `watchdog` and delivers its partial output
marked "stopped by watchdog". `doubt` holds the child before its next provider request, after the
whole tool batch, and wakes its parent with "continue or stop?" plus that evidence. Two consecutive
unavailable, timed-out or invalid answers also yield doubt; one is silent. Permission-, budget-
and doubt-parked children are not checked, and missed intervals are discarded.

Only the owning root cognition can use `decide_subagent(child_id, decision, note?)` to continue
or stop a watchdog doubt. This cannot approve a tool or renew a step budget. The Captain's existing
Stop and owner Continue endpoints still work. `subagent.watchdog` is a durable parent-stream
verdict with `child_session_id`, `verdict`, `reason` and `checked_at`; evidence is untrusted data.
Recovery stops doubts like any other unfinished child; watchdog state is not restored.
_Avoid_: role guard (that checks role conformance, not progress), permission request

## Models and commands

These selection terms follow the decision in
[ADR 0011](docs/adr/0011-layers-models-and-agent-overrides.md).

**Model**:
A catalog identity written `gateway/upstream-id`, such as `omniroute/combo/gpt-6-astra`;
the gateway is the configured endpoint and the remaining id is sent to it verbatim. Catalog
groups distinguish the gateway from providers within it, such as `omniroute` → `cc` or `combo`.
_Avoid_: alias, nickname, preset

**Model override**:
A durable model or variant choice for one session and one agent, restored on reopening or switching
back to that agent. Without an override or assignment, a primary agent uses the default agent's
model; an unassigned subagent uses its parent's effective model, including overrides.
_Avoid_: global model change, session-wide model

**Variant**:
A model's named request settings, such as reasoning effort (`low`, `high`), selected per session
and agent. The choice is `inherit` (use the applicable default), `off` (no variant overrides), or
a named variant; changing model without a variant takes the new model's default.
_Avoid_: thinking level, mode, profile

**Session command**:
A `/name` the server runs on a conversation: `/help`, `/model`, `/new`, `/stop`, `/context`,
`/variant`, `/compact`, `/agent`. A command has a result, shown in the conversation. While a permission request is pending, `/stop`
runs (and cancels the request) and every other command is refused. While an `ask` is pending, every
command is refused, `/stop` included; the stop endpoint (`POST /v1/sessions/{id}/stop`) still works. `//text` sends the literal text `/text`. The
terminal client also has its own commands (`/sessions`, `/theme`, `/quit`). `/model <gateway/id>`
switches the current agent's model in the open conversation, after the turn if busy; on an empty
tab it sets the starting override. `/new [agent]` starts fresh on that agent's assignment, without
the previous session's overrides.
_Avoid_: tool, action (an action is what a flow proposes)

## Permissions

**Captain request**:
Permission review uses the last three verified human-ingress messages (with command ids), a task
anchor and retained newer constraints; the role guard still uses only the latest message. Questions,
ok and praise do not revoke a task. Stop, completion, inspect-only and explicit new-task markers
replace the anchor. Intent exceeding the bounded budget is incomplete and asks rather than
dropping a constraint. Human words establish goals, not instructions about reviewers' verdicts.
Tool arguments, assistant text and parent
delegations remain untrusted evidence. Approval still requires understood, recoverable effects;
protected configuration requires a typed Captain answer or a live config session grant.

WithNav's explicitly configured config directory is a protected root: native writes/edits ask the
Captain before any model review, including symlinks. Scratch never overlaps this root.
Bash calls literally naming it ask too, but indirect bash effects cannot be fully confined.

**Config session grant**:
Only the protected-config request offers `allow_config_session` ("allow WithNav config for this
session"). A typed Captain answer shares an in-memory grant between the root actor and its live
subagents; future children inherit that same memory. It is never saved to `grants.json` and a new,
resumed or restarted root asks again. It skips only the Captain-only hold: instinct and
metacognition review still run, and evidence records `approved_by: config_session_grant`.
Credential/key targets, `auth.json`, changed scratch roots, and changes to `permissions` or
`automatic_permission_review` in the resulting `config.json` still ask. Malformed settings JSON
fails closed; bash naming `config.json` always asks because its resulting settings cannot be parsed.

**Scratch directory**:
Two independently checked roots default to private `$XDG_RUNTIME_DIR/withnav` (fallback
`~/.cache/withnav/scratch`) then shared `<temp_dir>/withnav`. `permissions.scratch_dirs` is an ordered
absolute-path list; `[]` disables both. Native scratch approval binds the root inode through dispatch.
These private owned roots are available across projects and children.
Scratch changes count as project-local during review. The ordered absolute-path list is snapshotted
and inherited by children; `[]` disables it. It must be a real directory owned by the current user, mode 0700.
Outside-project file changes receive review like bash effects; approval never extends writable roots.

**Permission review**:
An advisory per-call Instinct classification in one batched request: `call_effect`
(`read_only` / `local_change` / `remote_change` / `unsure`), `call_scope`, `reversibility`
(`none_needed` / `recoverable` / `destructive` / `unsure`), `publication`
(`none` / `own_work` / `shared` / `unsure`), plus probability questions `sensitivity` and
`task_fit` using the active Captain task and newer constraints. Confidence ≥ 0.7 is required only on deciding
answers: `read_only` for reads; `local_change`, `recoverable` and `inside` for local changes;
`remote_change`, `recoverable` and `own_work` for publication of the task's own work.
Secondary answers need no confidence, but cannot contradict a read with `outside`, or any
route with `destructive` or `shared`. Sensitivity must stay < 0.2 and task fit ≥ 0.6.
Git pushes require bounded
literal-command host facts confirming the current non-default branch and no force/deletion.
Host facts include the current/default branches, dirty state and literal git path tracking.
Missing git, a timeout or an ambiguous push supplies no authorization fact.

Destruction and shared publication at any confidence, sensitive or off-task effects, and low
confidence or unsure deciding answers go to synchronous metacognition review. Every Run path
still passes the host shell, executable identity, overwrite recovery and removal/move floors.
Its separate 64 KiB budget carries Instinct
answers/probabilities, git facts, three bounded untrusted assistant messages and ten structured
untrusted tool calls with arguments, outcome, exit code, output head and approval source;
Instinct keeps 16 KiB. Truncation is marked. On timeout, eligible floor-free calls retry once
with a fresh cancel token and a five-second deadline; outages never authorize a call.
Typed Captain allow-once answers become in-memory signature-and-scope precedents shared by live
children, never restored from `permission.resolved` events on resume. SQLite event JSON is writable
by ambient bash and cannot authenticate an approval; every resumed session starts with no precedents.
A matching live precedent replaces only
metacognition unsure/timeout escalation, never an explicit reject or a host floor, and records
`approved_by: captain_precedent` plus the earlier request. Bypass/config grants never create one.
New human turns conservatively invalidate precedents; every typed Captain deny clears all session
precedents, even when the denied call has no signature. Signatures
use the trusted resolver's validated canonical executable, subcommand and sorted flags. Push
signatures retain the confirmed target and the full effective push URL list, including Git URL
rewrites; unavailable URLs prevent a signature and precedent reuse. Python JSON exemptions require
`python3 -I -S` to exclude workspace/PYTHONPATH imports and startup hooks.
PR edits and execution/path overrides are not covered. A new conversation starts empty.
Otherwise only explicit approval runs reviewed calls; force pushes, default-branch pushes, releases and
remote deletions still require the Captain. Protected configuration, credentials and changed
scratch roots keep their existing Captain floors, and plan role conformance is unchanged.
Evidence records every answer, thresholds and route (`instinct_run` / `review` / `captain`).
Malformed answers, unavailable reviewers and deadlines escalate to the Captain. Not a sandbox.

Script execution always retains the opaque-script floor and goes to metacognition review,
never the Instinct-only fast path. Bounded script content is untrusted evidence, not authorization;
it makes the effects visible so the reviewer can return fewer `unsure` decisions. Direct executable
scripts attach only with a trusted canonical shebang interpreter (the executable identity resolver:
root-owned, not group/other writable, outside workspace/writable roots), without `env` indirection.
Evidence records that interpreter identity. No-follow reads reject multiply linked files as well
as symlinks, sensitive targets and protected configuration. Complete evidence binds the canonical
path, original spelling, file/parent identities and SHA-256. Validation is repeated before dispatch
and immediately before bash spawn; any mismatch or inability to revalidate goes straight to the
Captain with `script changed since it was reviewed`, even under bypass, without re-reading to
decide escalation. No snapshot copy is executed: a path-to-spawn race remains, and bash is advisory,
not a sandbox.

Native `read` within readable roots remains ungated, except sensitive targets. Native `grep` and
`glob` stay confined to readable roots and skip sensitive entries before opening them, with a count.
Outside single-file reads use the same per-call classification as bash. Approval binds one canonical target,
re-resolved before dispatch and opened without following replacement links. No root is added.
Credential/key targets deterministically force risky review and then a typed Captain decision even
if both models approve. A risky read-only bash call also needs review. Reading ordinary WithNav
configuration follows normal read review; `auth.json` is sensitive.
Bash is not confined: its sensitive-file detection is best effort (literal canonical operands and
recognized expansions), not enforcement against arbitrary shell programs. Only native file tools
enforce the deterministic sensitive-path policy. In-root directory walks still reopen descendants
by path; replacement links mid-walk need a future descriptor-relative traversal fix.

**Permission request**:
A held tool call awaiting a typed Captain decision. Bash, write and edit offer allow once or deny;
eligible protected-config calls also offer a config session grant.
fetch may also offer an origin grant. Chat text never approves a call. A pending request expires
on restart. Reviewer rejection escalates to the Captain rather than finally denying execution.
_Avoid_: confirmation, approval prompt

**Bypass**:
A per-session setting, off by default and never saved as a preference, that answers ordinary
permission requests for the Captain. When on, a call that would have asked runs once instead,
after the usual reviews: their verdicts stay in its evidence and the result is marked
`approved_by: bypass`. Bypass never creates a config session grant. Without that separate typed
grant, WithNav's protected configuration still asks; credential/key targets and a changed
scratch root always ask the Captain (the request says `requires_captain`); validation errors, tool
policy, read-only agents and outside searches are refused as before, and a question from `ask` is
never answered. Turning it on also answers an eligible request already waiting. A root session
sets it (`POST /v1/sessions/{id}/bypass`, `/bypass on|off`, `nav run --yes`) and its live
subagents follow every change; a new subagent starts with the parent's current value. A resumed
session starts off, and its history says so. HTTP changes require a configured bearer token and
an authenticated client, including slash commands; servers without a token allow only state reads.
The in-process `nav run --yes` path does not require HTTP authentication.
Not a sandbox: bash keeps its ambient access.
_Avoid_: auto-approve, yolo, allow all

**Folder grant**:
Historical bash/file folder approvals remain readable on disk but never authorize execution.
Only fetch origin grants bypass later requests. Root sessions save those per workspace; children
keep them in memory. Approval never expands writable roots.
_Avoid_: whitelist, permission rule

**Role guard**:
Plan exceptions require instinct role conformance in the same batched request as safety review.
Plain Markdown exempts only the role question, never permission review. Conformance is recorded
in result metadata independently of safety approval. Advisory, not a sandbox.
_Avoid_: filter, sandbox, policy

**Pending interaction**:
What a yielded actor waits for: the answer to an `ask`, or the decision on a permission request (with
the held call and the calls behind it). While a permission request is pending, session commands are
refused, except `/stop`, which runs and cancels the request. While an `ask` is pending, all session
commands are refused, `/stop` included, until it is answered. The stop endpoint
(`POST /v1/sessions/{id}/stop`) works in both cases.

## Shared context

**Shared context**:
What every layer knows about the work in progress, so that fast layers can be relevant, not just
fast. It has two parts: the intent record and the live state.
_Avoid_: memory, context window

**Intent record**:
The durable part of the shared context: current intent, plan and decisions, readable and editable by
the Captain and kept under version control.
_Avoid_: plan file, notes

**Live state**:
The ephemeral part of the shared context: recent edits, compiler diagnostics, a running summary of
the conversation. It is not persisted.
_Avoid_: session state, cache

**Wake**:
The metacognition bringing something to the cognition's attention, turning a fast observation into a
deliberate discussion.
_Avoid_: escalate, trigger (a trigger is an instinct input)

**Wake hold**:
The rule that, while a conversation waits for an answer to an `ask` or for a permission decision, a
wake from the navigator is kept back instead of delivered. Only the latest held wake is kept, and it
is delivered once nothing is pending. Captain text, unlike a wake, still cancels a permission request.
_Avoid_: mute, queue

**Trigger**:
The kind of event a flow listens to (save, idle, diagnostic change, conversation turn, an agent-loop
hook such as a tool call about to run). Each flow has exactly one trigger; the arrival of a matching
event starts a flow run.
_Avoid_: hook (the agent-loop point it comes from), input (what a node reads)

## Editor flows

The legacy decision pipeline was replaced by the built-in save, idle and conversation flows
on 2026-10-08. They are enabled by default; without an enabled flow for a trigger nothing runs.
Pipeline, Branch and Question describe the historical `pipeline.toml` format only.

**Action**: what a flow proposes, such as a remark, a wake or silence.

**Replay**: offline evaluation of captured decisions through flows, comparing their actions with
recorded ones without asking a model or applying effects. Missing provenance is not comparable.
See Replay (flows) below.

## Flows

Implemented for editor triggers; see [ADR 0013](docs/adr/0013-instinct-flow-engine-crate.md)
and `.scratch/instinct-flow/spec.md`.

**InstinctFlow**:
The feature: flows run by the instinct layer, edited in the studio.
_Avoid_: Jev flows

**Studio**:
The editor where the Captain draws, tests and saves flows, and asks the engine to reload them.
_Avoid_: flow editor (fine in prose, not as a name), canvas (the canvas is one part of it)

**Flow**:
An acyclic graph of nodes started by one trigger: decisions ask the instinct, expressions compute,
actions end the chains. A node runs at most once per flow run, as soon as any wire into it is
active; one output may feed several nodes, so a run can follow several branches at once. The
built-in editor flows preserve the historical single-batch decisions.
_Avoid_: workflow, automation, subagent (a subagent is a child session, not a flow)

**Event**:
Something that arrives and may start a flow run: what happened, and its payload.
_Avoid_: trigger (the trigger is the kind of event a flow listens to)

**Payload**:
The data carried by one event, such as the tool name and arguments of a tool call about to run.
It is one source of inputs.
_Avoid_: input (a payload is a source of inputs, not an input)

**Input**:
One named piece of data a node reads: a payload field, session or workspace context, a constant,
or the answer of a node upstream of it. A node may read only what it declares, and only answers
of nodes it is downstream of. An answer from a node the run did not pass through is absent, never
a default value. In the end an input is text or an image.
_Avoid_: state (state is the set of inputs a decision sends), context (too broad)

**Node**:
One step of a flow, drawn as a card on the canvas: a decision, an expression or an action. It
reads its inputs and, except for actions, produces one output.
_Avoid_: card (the drawing, not the step), block, step

**Port**:
A named exit of a node, such as `yes` and `no` of a noul or one per option of a choice. The
active port chooses which wires carry the run on.
_Avoid_: branch (a branch is the old pipeline's ordered condition), edge

**Decision**:
A node that asks the instinct one typed question about its inputs: noul (probability of yes,
against a threshold), choice (one option among named criteria) or score (an ordered level, against
a minimum). Its answer is both control (it activates one port) and data (later nodes may read it).
_Avoid_: question (one decision asks one question), classifier

**Expression**:
A node computing a deterministic condition over its inputs, without calling any model and
without side effects; it activates `true` or `false`.
_Avoid_: filter, rule, function

**Constant**:
A named value of a flow, such as `MIN_CONFIDENCE = 0.6`, read as an input by its nodes.
_Avoid_: magic number, parameter

**Variable**:
A named value computed during one flow run by an expression (`THRESHOLD = asked_for > 0.5 ? 0.5 :
0.75`) and read by the nodes after it. It starts afresh on every run and is never kept between
runs, sessions or workspaces, so a run depends only on its event.
_Avoid_: state, memory, setting

**Workspace flow** / **Global flow**:
Where a flow definition lives: in the workspace, read only once the workspace is trusted, or in the
Captain's own configuration, for every workspace. A workspace flow replaces a global flow of the
same name. The Captain chooses where when creating the flow.
_Avoid_: project flow, user flow

**Action** (flows):
What a chain ends with: something WithNav does, chosen from the closed list WithNav offers. A flow
can make WithNav more careful (deny a tool call, ask the Captain) but never less: an action that does
not object leaves the normal permission review in place. Actions never run arbitrary code.
_Avoid_: outcome, effect, allow (say "no objection")

**Fallback**:
What a flow does when its run fails: the whole run's actions are replaced by it, even if some
branches had already concluded. One general fallback (on failure) may be specialised per kind of
failure (on timeout, on provider error, on invalid answer). The failure message is always kept in
the trace. Without a fallback, a blocking flow asks the Captain and an asynchronous flow does
nothing.
_Avoid_: on timeout (a timeout is one kind of failure), default action

**Reload**:
The flow engine reading the flow files again. Saving a flow only writes its file; what runs
changes at the next reload (an explicit command, or a restart). A reload in which any file is
invalid changes nothing and reports the errors.
_Avoid_: activate, deploy, publish

**Flow definition**:
The definition of a flow with the inputs each node may read, stored as one file. Graph and text are
two views of it; the positions of the cards are kept in the file but do not change what it does.
_Avoid_: flow file (the file is where it is stored), script

**Flow run**:
One evaluation of a flow definition for one event, within explicit call, input and time limits; it ends in proposed actions and a trace.
_Avoid_: execution, job

**Observation**:
Evaluating a flow on real events, possibly calling the instinct, without applying any action or changing any production state.
_Avoid_: shadow mode, dry run

**Replay** (flows):
Re-running recorded answers through a flow definition without calling any model. It is comparable only when the rendered requests, model and variant, and selected inputs match what the definition would send now.
_Avoid_: re-evaluation (that calls the model again)

**Workspace trust**:
One explicit Captain decision that lets a workspace's own flow definitions read the context they declare. It activates nothing and grants no tool permission or model destination.
_Avoid_: permission, approval

## Context budget

**Context budget**:
How much of a model's context window the next request may fill, and how full it is now. The
window W comes from the saved resolution; the output reserve R is the `models.json`
`max_output_tokens`, else the smallest of 8192, a quarter of W and the output ceiling the provider
reports; the margin M is 5% of W (at least 4096); the input budget is `min(W − R, input limit or W) − M`.
A window that leaves no input budget is refused at session creation. The fill is an estimate of the
next request (UTF-8 bytes / 4 plus a fixed framing per message), split into system, rules, skills
catalog, tools and messages, and it is shown as an estimate (`~42k`). It is never the provider's count
of the last request (`last_usage`), and never the session's cumulative usage.
_Avoid_: token limit, quota

**Safe Harbor · Checkpoint**:
The summary that stands for the start of a conversation once it was compacted: `{through_message,
summary, generation}` plus the tool outputs the prompt shows as elision markers. The full
transcript is never rewritten: `sessions.transcript` keeps every message, and the checkpoint is a
boundary the request builder applies. The prompt is then the fresh system components, the summary
as labelled conversation data (never as instructions), and the messages after the boundary. The
system components, the latest user request, the unfinished turn with any pending `ask` and the last
two completed turns are always kept verbatim, and a cut never splits a tool call from its results.
`/compact [focus]` asks the assigned metacognition model and variant for a summary, falling
back to the session model on provider errors (never on cancellation). Model switches may then
use the destination. Each writer budgets against its own window; the installed checkpoint
still has to fit the session or destination. Summaries roll over message-boundary chunks,
keeping tool exchanges together, with the previous summary as untrusted JSON data.
`config.json` settings `compaction.writer` (`metacognition`, default, or `session`) and
`compaction.chunk_tokens` (64000, capped at half the writer input budget) are exposed by
`/v1/settings`. They apply to newly prepared sessions. Each failed pass is retried once;
failed or empty summaries never replace a checkpoint. `context.compacted` names the writer
and records successful passes.
_Avoid_: snapshot (that is the context gauge), history rewrite
