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

**Reflex**:
The fastest layer: completes what the driver is typing, as ghost text, from a fill-in-the-middle
model.
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

**Primary agent**:
The role a session runs as: `build` (implement, verify, report), `plan` (research, write markdown
plans, delegate) or a custom one. The driver selects it for the session and can switch between turns.
It sets what the model is told to do, not what it may execute.
_Avoid_: mode (a mode is where an agent definition may be used), persona

**Agent definition**:
A role described in a built-in or a markdown file: description, mode (primary, subagent or both),
role prompt, optional model, tool allow/deny map and access level. `build` and `plan` are reserved.
_Avoid_: profile (a profile is the kind of session, such as the navigator)

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

**Trigger**:
An event (save, idle, diagnostic change, conversation turn) that makes the instinct evaluate whether
a layer should act.
_Avoid_: hook, event (too generic)

## Decision pipeline

**Pipeline**:
The editable chain that turns a trigger into an action: the questions the instinct asks, and the
branches that read the answers.
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
