# Navai

A pair-programming system in which the human keeps the keyboard and stays the author of the code,
while several models of different speeds watch, complete, judge and discuss. It exists to get the
speed of AI assistance without losing the developer's mental model of their own codebase.

## Roles

**Driver**:
The human writing the code. The driver is the default author of every line.
_Avoid_: user (when authorship matters), pilot

**Navigator**:
The whole assisting system as the driver perceives it: a colleague watching over the shoulder who
completes, remarks and discusses, but does not take the keyboard.
_Avoid_: copilot, assistant, agent (for the whole system)

## Layers

The layer, model and agent vocabulary below describes the implementation prepared for **v0.5.0**;
see [ADR 0011](docs/adr/0011-layers-models-and-agent-overrides.md). It does not describe which
version is installed on the driver's machine.

**Layer**:
One of four cognitive roles: reflex, instinct, metacognition and cognition. The first three each
have one configured model at a time; cognition runs agents in parallel sessions. Configurable
flows are planned for 0.6.
_Avoid_: model tier, model alias

**Reflex**:
The fastest layer: completes what the driver is typing, as ghost text, from a fill-in-the-middle
model, only in editor contexts.
_Avoid_: autocomplete layer, FIM layer

**Instinct**:
The decision layer: answers bounded, typed questions (choice, score, yes-probability) about the
current situation to sort, route, filter and gate; it never produces free text.
_Avoid_: classifier, router, Jev (Jev is one provider of this layer, not the layer)

**Metacognition**:
The fast reflective layer: watches both the code being written and the conversation, judges whether
the code matches the intent, produces remarks, and can wake the cognition.
_Avoid_: observer, critic, flash (a flash model is its current engine, not its role)

**Cognition**:
The deliberate layer: the high-level conversation with the driver about concepts, features, intent
and problems, with the full agentic toolset.
_Avoid_: main agent, chat, frontier (a frontier model is its engine, not its role)

## Outputs

**Suggestion**:
Ghost text offered by the reflex at the cursor; it becomes code only when the driver accepts it.
_Avoid_: completion (ambiguous with chat completions)

**Remark**:
A short, line-anchored comment from the metacognition, shown discreetly next to the code; it never
changes the code.
_Avoid_: hint, diagnostic, comment

**Delegation**:
The driver explicitly asking the cognition to do a piece of work itself instead of guiding them
through it; without a delegation, the cognition assists and the driver writes the code.
_Avoid_: autopilot, vibe mode

**Verdict**:
The driver's rating of a remark: useful, noise, or dismissed.
_Avoid_: feedback, vote

## Agents

The ownership and delivery decisions are in
[ADR 0009](docs/adr/0009-background-subagents-and-durable-delivery.md).
Model selection and editor context follow
[ADR 0011](docs/adr/0011-layers-models-and-agent-overrides.md).
An invalid nondefault model assignment falls back to the default agent's model with a visible
warning; an invalid default agent or unusable default model prevents startup.

**Agent**:
The role cognition plays, such as `build`, `plan`, `explore`, `review` or a custom role: its prompt,
tools, access and optional assigned model and variant. A configured assignment takes precedence
over the model in its markdown header.
_Avoid_: model, profile, persona

**Default agent**:
The agent a new session starts with unless the driver chooses another: configured by
`default_agent`, or `build` when absent. It must have a usable model or Navai cannot start.
_Avoid_: default model, default alias

**Editor session**:
A session created by an editor client, adding the pair-programmer prompt, `intent` tool and
navigator conversation attachment to its agent. This context is independent of agent and model
selection and starts on the default agent like any other session.
_Avoid_: navigator profile, session profile

**Primary agent**:
The role a session runs as: `build` (implement, verify, report), `plan` (research, write markdown
plans, delegate) or a custom one. The driver selects it for the session and can switch between turns.
It sets what the model is told to do, not what it may execute.
_Avoid_: mode (a mode is where an agent definition may be used), persona

**Agent definition**:
A role described in a built-in or a markdown file: description, mode (primary, subagent or both),
role prompt, optional model, tool allow/deny map and access level. `build` and `plan` are reserved.
_Avoid_: profile, session type

**Subagent**:
An agent definition whose mode is `subagent` or `all`, meant to be given a task by another agent
(`explore` and `review` are built in and read-only). A cognition session may start one through
`task(agent, prompt)` from either the terminal or an editor root conversation. Children never
register with the conversation feed or receive the navigator's `intent` tool. Their results use
normal durable delivery, not navigator wakes; the root digest labels them as untrusted subagent text.
The subagent is a real, non-promptable child session. It has an independent transcript, an
intersected snapshot of the parent's policy, a durable parent-child link, and bounded terminal
result. `subagent.spawned` and `subagent.done` are replayable events. Results enter the parent's
transcript once, as bounded untrusted user messages at a turn boundary. `steer` delivery can use
a safe boundary within its running turn. Completion wakes an idle parent, but cannot answer its
pending interaction. Child permissions are answered by the driver
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
from replacing the driver's instruction for compaction or the plan guard. A child result whose
delivery mode is `steer` uses the same boundary on the parent, with untrusted subagent provenance.
_Avoid_: driver message, permission answer

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
output, timeout or an unavailable reviewer pauses at that boundary and asks the driver to Continue
(`alt+c`) or Stop. Permission and ask waits are not judged or answered by this mechanism.
At the settled boundary, a pause snapshots the transcript, publishes a typed yield with its turn
identity and unique incarnation/hold token, and ends the turn as yielded. Owner Continue consumes
that token once; late answers cannot release a newer pause. Resolution, expiry and driver-message
supersession are sequenced notices. Restart expires the hold, retains the checkpoint and waits
for fresh driver input. Navigator wakes and child results cannot release a hold or renew a parked
budget. A real driver message replaces the paused turn and records its interruption.
Recovery installs a separate fresh-driver gate even after publishing expiry; pending child
completions and autonomous inbox messages stay held until HTTP or CLI driver input clears it.
Only a consumed instruction checkpointed atomically with its gate-release event clears recovery's
gate. Durable `message.received` is admission, not proof of transcript consumption. A crash before
that checkpoint keeps the gate closed on the next recovery. Owner Continue on a budget-parked
child claims accepted interrupt replacements exactly once before requesting another model step.
Supersession names the old turn in the pause-resolution event, never a second terminal event.
Accepted deferred model/compact operations receive correlated cancellation at a watchdog pause.
Without instinct, or with supervision disabled, the session announces a renewable 500-step fallback
cap instead. Only explicit owner Continue or fresh driver input renews it. An explicit
`NAVAI_MAX_STEPS` keeps the existing budget parking behaviour; subagents keep their 50-step default.

The instinct's separate progress check for a running background child, every 300 seconds by
default (`subagents.watchdog_interval_secs`; 0 disables). It reads a bounded goal, time since
message/tool activity, last tool call and result, and step count, never reasoning. `active` does
nothing; `stuck` terminalizes the child with reason `watchdog` and delivers its partial output
marked "stopped by watchdog". `doubt` holds the child before its next provider request, after the
whole tool batch, and wakes its parent with "continue or stop?" plus that evidence. Two consecutive
unavailable, timed-out or invalid answers also yield doubt; one is silent. Permission-, budget-
and doubt-parked children are not checked, and missed intervals are discarded.

Only the owning root cognition can use `decide_subagent(child_id, decision, note?)` to continue
or stop a watchdog doubt. This cannot approve a tool or renew a step budget. The driver's existing
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
_Avoid_: tool, action (an action is what a pipeline branch decides)

## Permissions

**Driver request**:
The driver's own latest human-ingress message. Explicitly requested effects establish intent,
not instructions to the reviewers about their verdict. Tool arguments, assistant text and parent
delegations remain untrusted evidence. Approval still requires understood, recoverable effects;
protected configuration always requires a typed driver answer.

Navai's explicitly configured config directory is a protected root: native writes/edits ask the
driver before any model review, including symlinks. Scratch never overlaps this root.
Bash calls literally naming it ask too, but indirect bash effects cannot be fully confined.

**Scratch directory**:
Two independently checked roots default to private `$XDG_RUNTIME_DIR/navai` (fallback
`~/.cache/navai/scratch`) then shared `<temp_dir>/navai`. `permissions.scratch_dirs` is an ordered
absolute-path list; `[]` disables both. Native scratch approval binds the root inode through dispatch.
These private owned roots are available across projects and children.
Scratch changes count as project-local during review. The ordered absolute-path list is snapshotted
and inherited by children; `[]` disables it. It must be a real directory owned by the current user, mode 0700.
Outside-project file changes receive review like bash effects; approval never extends writable roots.

**Permission review**:
An advisory per-call classification by the instinct, followed for risky or outside-project
mutations by synchronous metacognition review. Only explicit approval runs those calls;
uncertainty, malformed answers and deadlines escalate to the driver. Not a sandbox.

Native `read` within readable roots remains ungated, except sensitive targets. Native `grep` and
`glob` stay confined to readable roots and skip sensitive entries before opening them, with a count.
Outside single-file reads use the same per-call classification as bash. Approval binds one canonical target,
re-resolved before dispatch and opened without following replacement links. No root is added.
Credential/key targets deterministically force risky review and then a typed driver decision even
if both models approve. A risky read-only bash call also needs review. Reading ordinary Navai
configuration follows normal read review; `auth.json` is sensitive.
Bash is not confined: its sensitive-file detection is best effort (literal canonical operands and
recognized expansions), not enforcement against arbitrary shell programs. Only native file tools
enforce the deterministic sensitive-path policy. In-root directory walks still reopen descendants
by path; replacement links mid-walk need a future descriptor-relative traversal fix.

**Permission request**:
A held tool call awaiting a typed driver decision. Bash, write and edit offer allow once or deny;
fetch may also offer an origin grant. Chat text never approves a call. A pending request expires
on restart. Reviewer rejection escalates to the driver rather than finally denying execution.
_Avoid_: confirmation, approval prompt

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
the driver and kept under version control.
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
is delivered once nothing is pending. Driver text, unlike a wake, still cancels a permission request.
_Avoid_: mute, queue

**Trigger**:
An event (save, idle, diagnostic change, conversation turn) that makes the instinct evaluate whether
a layer should act.
_Avoid_: hook, event (too generic)

## Decision pipeline

**Pipeline**:
The editable chain that turns a trigger into an action: the questions the instinct asks, and the
branches that read the answers. Today it is one flat batch of questions followed by ordered
branches; a staged, conditional version is a flow (below).
_Avoid_: workflow, graph, rules

**Question**:
One typed ask put to the instinct about the current state, answered as a choice, a score or a
yes-probability.
_Avoid_: check, probe, query

**Branch**:
A condition over the answers, with a threshold, that selects an action when it holds; branches are
tried in order and the first that holds wins.
_Avoid_: rule, route (route is itself a question name)

**Action**:
What a branch decides: stay silent, ask the metacognition for a remark, or wake the cognition.
_Avoid_: outcome, effect

**Replay**:
Running a recorded history of triggers through a different pipeline to see which actions it would
have chosen, compared against the driver's verdicts.
_Avoid_: backtest, simulation

## Flows

Planned for 0.6, not shipped; see [ADR 0012](docs/adr/0012-extractable-system-one-flows.md).

**Flow**:
A staged set of instinct questions where an earlier answer can choose which question runs next, ending in an action. It has no loops. The existing pipeline is the flat, single-batch case.
_Avoid_: workflow, automation, subagent (a subagent is a child session, not a flow)

**Flow definition**:
The validated, immutable text of a flow, with the inputs each step may read. Graph and text are two views of it.
_Avoid_: flow file (the file is where it is stored), script

**Flow run**:
One evaluation of a flow definition for one event, within explicit call, input and time limits; it ends in a proposed action and a trace.
_Avoid_: execution, job

**Draft**:
A saved flow definition that is not running. Editing or saving it never changes what is active.
_Avoid_: pending, unsaved (a draft is saved)

**Active version**:
The flow definition currently producing actions for one workspace. Activation is explicit; the previous active version is kept for rollback.
_Avoid_: deployed, published, live

**Observation**:
Evaluating a flow on real events, possibly calling the instinct, without applying any action or changing any production state.
_Avoid_: shadow mode, dry run

**Replay** (flows):
Re-running recorded answers through a flow definition without calling any model. It is comparable only when the recorded questions match the ones the definition would ask.
_Avoid_: re-evaluation (that calls the model again)

**Workspace trust**:
One explicit driver decision that lets a workspace's own flow definitions read the context they declare. It activates nothing and grants no tool permission or model destination.
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

**Checkpoint**:
The summary that stands for the start of a conversation once it was compacted: `{through_message,
summary, generation}` plus the tool outputs the prompt shows as elision markers. The full
transcript is never rewritten: `sessions.transcript` keeps every message, and the checkpoint is a
boundary the request builder applies. The prompt is then the fresh system components, the summary
as labelled conversation data (never as instructions), and the messages after the boundary. The
system components, the latest user request, the unfinished turn with any pending `ask` and the last
two completed turns are always kept verbatim, and a cut never splits a tool call from its results.
`/compact [focus]` makes one by asking the session's own model and variant for a summary.
_Avoid_: snapshot (that is the context gauge), history rewrite
