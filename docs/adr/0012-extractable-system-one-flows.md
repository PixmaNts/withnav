# Flows deepen the Rust evaluator and keep host authority in Navai

Date: 2026-10-05

Status: Accepted as direction — planned for 0.6, not implemented or shipped

The navigator's decision pipeline asks one batch of questions and takes the first matching branch.
The driver wants staged questions where an earlier answer selects the next, edited as a graph or
as text, replacing the remarks pipeline progressively. We extend the existing Rust evaluator and
keep every effect under Navai's authority. The vocabulary is in [CONTEXT.md](../../CONTEXT.md).

## Decision

- **Deepen the Rust pipeline; no second runtime.** The evaluator keeps its existing System One
  clients, condition language, snapshot hashing and replay. Embedding the Node `jev-flow` engine,
  or running it as a process, was rejected: it adds a runtime and a JSON format Navai does not need,
  its documented limits do not give a whole-run elapsed budget, and it can execute webhooks, an
  authority that would sit beside Navai's permissions. It stays a design reference only. Published
  Rust Jev clients were compared as references; they parse answers permissively, so the evaluator
  validates every answer against the request that produced it.
- **Staged and acyclic, not a workflow engine.** A flow is a directed graph of question steps with no
  loops. Persistent variables, subflows, action webhooks and a plugin framework are out of scope.
  Loops would make cost and termination depend on model answers.
- **The evaluator proposes; the host applies.** It receives an immutable validated definition,
  bounded inputs, an injected decision client, cancellation and explicit finite budgets, and returns a
  proposed outcome with a trace. Navai alone collects context, holds credentials, owns activation
  generations and applies outcomes through existing remark and wake delivery. An outcome is never
  permission to run a tool, start a session, or answer a pending ask or permission request. Workspace
  trust does not change that.
- **Extractable by proof, not by packaging.** The evaluator must run in a headless test with no LSP,
  session or store code and a fake client. "No HTTP" means the evaluator opens no connections itself;
  an injected client may be the real HTTP adapter. A separate binary, crate or interchange format is
  not decided.
- **Observation has no effect.** Evaluating real events for comparison never publishes a remark,
  wakes the cognition, or changes cooldowns, deduplication, baselines or in-flight ownership.
  Replaying recorded answers calls no model; a fresh evaluation does and is accounted.
- **One producer per scope.** The old pipeline stays the producer until the driver explicitly
  activates a flow version; activation fences late results of the earlier producer, and rollback to
  the old pipeline stays available throughout 0.6. A flow failure silences the event with a
  diagnostic; it never falls back or rolls back by itself.

## Consequences

The flat pipeline format cannot express staging, so the definition format is extended. The
baseline is one node of the new form, which keeps the existing behaviour comparable. The Rust crates
surveyed and any new dependency are not adopted by this decision. Settings and the studio ride on
the same snapshot and compare-and-swap rules but are specified separately in the 0.6 work plan.

This records the direction chosen before implementation. The module layout, schema, storage and
budget numbers are still to be decided and are not claimed here.
