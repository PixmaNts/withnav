# Background subagents are owned child sessions; results are delivered durably

The cognition needs to delegate work without blocking its conversation with the driver.
The driver must still see the work, stop it and authorize its effects.
A result must not disappear when a client disconnects or a process dies.
These decisions describe the background subagents shipped in Navai 0.4.0.
They build on the separate role and permission rules of ADR 0008.

## Decision

- **A subagent runs as a real child session, in the background only.** Root cognition starts
  it with `task`. It has its own transcript and a durable parent-child link.
  The driver can read it, stop it and continue a parked child, but cannot prompt or resume it.
  Children cannot start children. The default limit is four active children per parent.
  Starting and parked children count too. `subagents.max_concurrent` can change the limit.
  Inline tool loops were rejected: they hide the child's lifecycle and block the parent.
  Independent writable conversations were rejected: they lose ownership and allow nested fan-out.
- **Tool policy is an intersection, not an override.** The child gets only tools allowed by
  its declaration, the parent's declaration and both access ceilings.
  A read-only child or parent cannot grant `write`, `edit` or `bash`.
  A `plan` parent imposes the same no-write ceiling on its children.
  `ask` and `task` are never available to children. Root steering and decision tools stay root-only.
  The dispatch registry enforces the frozen policy, not just the schema sent to the model.
  Prompt-only restrictions and child overrides were rejected: neither prevents a forbidden call.
- **Admission and teardown have separate locks.** The lifecycle gate stays held across runtime
  joins. A short admission gate protects reservations and lifecycle claims.
  The lock order is lifecycle, admission, then the registry. No I/O or joins await under admission.
  Admission has two phases. First reserve capacity and capture the parent's incarnation.
  Prepare outside the gates. Then revalidate that incarnation before publication.
  Child-scoped operation ownership covers publication, failed-start cleanup and watchdog teardown.
  Stop and delete inspect ownership and claim their transition together under admission.
  They wait outside admission and retry if another operation owns the child.
  Releasing the lifecycle gate before joins was tried and rejected: concurrent stop, resume and
  delete could observe incomplete teardown and violate the single-runtime invariant.
- **Completion is a durable outbox notice.** The terminal child state and notice commit in one
  transaction. Only its winner releases capacity. A watchdog stop claims its reason, cancels and
  joins execution, then commits completion. Competing natural outcomes are retained during a
  provisional claim and finalized if it rolls back.
  The parent's own sequencer drains the outbox. `delivered` means the notice event was committed
  to that stream. `consumed` means its text entered the parent's transcript.
  These are separate recovery points. Consumption, transcript and the durable `message.received`
  event commit together before live emission. A later batch failure retains the committed prefix.
  Notices and text survive child deletion. Restart drains pending notices without replaying tools.
  Best-effort SSE delivery was rejected: an offline parent or a closing stream would lose results.
- **Results are untrusted user messages with subagent provenance.** The parent reads bounded JSON
  containing child identity, outcome and result. It is data, not a new driver instruction.
  Compaction and the plan role guard still use the latest driver message.
  Live and replayed `message.received` events identify `source: "subagent"`.
  Queued driver input precedes automatic result wakes and eligible result injection.
  Navigator's latest-wins inbox was rejected: coalescing it would discard independent results.
  Driver provenance was rejected: a result could replace the task the parent is meant to serve.
- **Child permissions go to the driver through the parent.** Requests and resolutions are mirrored
  on the parent stream. The child keeps its pending interaction and grants.
  Owner-scoped answers use the child's typed permission channel.
  A result never answers a pending human interaction.
  Parent-model approval was rejected: delegated text must not authorize effects.
- **Steer and interrupt respect complete tool batches.** Steer waits for the safe boundary before
  the next provider request, after every call has its result. It does not restart the turn.
  Interrupt cancels the current turn, settles its calls and starts a replacement turn.
  Its token is separate from session stop. Replacement operations are claimed once, in order.
  Parent guidance has `Parent` provenance and an in-memory queue. A crash can lose a steer.
  Child results remain in the durable outbox. Their delivery can be next-turn, steer or interrupt.
  Result interrupts wait behind human interactions. An explicit parent interrupt can cancel one.
  Mid-batch injection was rejected: it can separate a call from its result.
  Persisting every steer was not chosen; volatile guidance is distinct from durable completion.
- **The watchdog asks the instinct about progress, not role conformance.** Every 300 seconds by
  default, Jev reads bounded observable activity, never reasoning. Zero disables the checks.
  `active` does nothing. `stuck` stops execution and delivers partial output marked
  "stopped by watchdog". `doubt` holds the next safe boundary and wakes the parent with evidence.
  Two consecutive unavailable, timed-out or invalid answers mean doubt; one is silent.
  Permission-, budget- and doubt-parked children are skipped. Missed ticks do not pile up.
  Verdicts belong to a runtime generation. Stale verdicts cannot stop a replacement turn.
  The owning parent model answers doubts with `decide_subagent`; driver stop and continue remain.
  The decision cannot approve permissions or renew a budget. Watchdog state is not restored.
  A timeout-as-stop rule was rejected: an unavailable judge is not proof of stuck execution.
- **Recovery must distinguish dead owners from other live processes.** Each service holds an
  instance lock file beside the canonical database, in `<database>.instances/`.
  Recovery holds a stale owner's lock while terminalizing its unfinished children.
  It leaves live owners alone and is conservative if ownership cannot be checked.
  Unconditional recovery was rejected: several servers can share one database.
  Locks under the configurable data directory were rejected: different directories can still
  refer to the same database.
- **SQLite write transactions acquire the write lock first.** Writer and outbox transactions use
  `BEGIN IMMEDIATE`. A real concurrent-delivery hang exposed a deferred read-then-write race.
  The upgrade failed with a locked database, poisoned the writer and ended the parent runtime.
  Deferred writes were rejected: taking the lock after reading cannot safely upgrade that snapshot.

## Consequences

- Child execution never takes the lifecycle gate. Teardown still owns the runtime until it exits.
- Completion publication and transcript consumption are distinct, durable and idempotent.
- The driver remains the authority for effects. Delegation does not expand permission.
- The event catalog has 22 kinds. Live result text reuses `message.received`.
- Shared writable workspaces are not sandboxes. Concurrent children can still edit the same files.
