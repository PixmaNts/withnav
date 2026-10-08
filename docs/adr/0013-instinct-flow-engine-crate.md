# InstinctFlow is a graph engine in its own crate, reloaded explicitly

Date: 2026-10-07

Status: Accepted — not implemented. Supersedes [ADR 0012](0012-extractable-system-one-flows.md).

Naming status: the human role is now called the Captain and the product WithNav;
see [ADR 0014](0014-rebrand-withnav.md). Original decision wording is retained.

The 0.6 flow work followed ADR 0012: the existing pipeline deepened into staged batches of questions
with ordered branches on one path, three actions, drafts and activation in a private database, and
a separate vanilla JS Studio. The driver rejected it: a flow is a canvas of cards, one decision per
card, whose outputs feed any number of later cards, and the engine should later run as a standalone
application (events pushed over an API, actions that run code). The specification is
`.scratch/instinct-flow/spec.md`; the vocabulary is in [CONTEXT.md](../../CONTEXT.md).

## Decision

- **A flow is an acyclic graph of single-question nodes.** Decision (`noul`, `choice`, `score`),
  expression and action nodes joined by wires from named ports. A node runs once, as soon as any
  wire into it is active; branches run in parallel; an answer from a node the run did not pass
  through is absent, not defaulted. Batched questions with first-match branches were rejected:
  they hide the graph the driver reasons with. Batching survives as an engine optimisation for
  decisions that send the same state.
- **The engine is a separate crate, `crates/instinct-flow`, in a Cargo workspace.** ADR 0012 relied
  on proving isolation inside one crate; the rejected stack's proof was a text search of imports,
  which aliases and macros bypass. A crate makes the compiler enforce it, and is what a standalone
  application and the `instinct-flow` CLI build on. The crate holds the System One protocol and an
  HTTP client built from explicit parameters; credentials, model choice and the instinct layer stay
  in Navai, which hands the engine a ready client.
- **Three seams, supplied by the host.** A catalogue of triggers (payload schema, can block, sample),
  inputs and actions; a decision client; an action executor. The engine proposes, the host applies,
  all at the end of a run. Navai's catalogue is closed: no action runs code, and none can make
  Navai less careful than its permission review. Code-running actions belong to the future
  standalone host.
- **Failure replaces the whole run by a fallback.** A run whose instinct call times out, fails or
  answers invalidly discards every proposed action, including concluded branches, and applies the
  flow's fallback; without one a blocking flow asks the driver. Partial results were rejected: a
  flow whose remaining branches did not run did not decide.
- **Flows are files, run after an explicit reload.** `.navai/flows/*.toml` (after workspace trust)
  and `~/.config/navai/flows/*.toml`, the workspace one replacing a global one of the same name.
  Saving writes the file; a reload, all or nothing, makes it run. This drops ADR 0012's drafts,
  activation generations and hash-pinned versions: version control already keeps history, and the
  driver chose a simple save-then-reload model.
- **The remark pipeline becomes a built-in flow and its engine is removed**, once replaying the
  recorded decision log through it gives identical actions. ADR 0012's rollback to the old pipeline
  is dropped.

## Consequences

Agent-loop hooks (`tool.pre_call`, `message.received`) can now wait on flows, with a whole-run
budget capped at 2 s. The React WebUI gains its first large dependency, React Flow, for the canvas.
Observation of draft flows on real events, persistent variables, sub-flows and the actions that
change agents or start subagents are deferred.
