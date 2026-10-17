# withnav-sketch

Compile-checked prototype of the WithNav v1 target design (`../pipeline.md`, `../types.md`,
`../data-flow.md`). Sketch fidelity: no I/O, no HTTP, no SQLite. Standalone crate, not in the root workspace.

```
cd doc-v1/sketch && cargo test
```

## What it proves
- Stages run serially in plan order; `Halt` short-circuits; one `StageObservation` per run reaches `Telemetry`.
- `tool.prepare` rewrites happen before authorization; the decision sees the rewritten call.
- Fixed authorization skeleton (yolo, written deny, floor, doom loop, free read, bypass, review slot, rule).
- A written deny is final; plugin reviewers return `Restriction` (no `Allow`), so they only escalate.
- `ExecutionPermit` cannot be built outside `engine::authorize` (compile_fail doctest).
- `tool.result` patches change the output text only; ids and status are untouched.
- A plan is an `Arc` snapshot per turn; a swap applies to the next turn.
- `api::v1::Event` keeps today's wire names; an unknown kind decodes to `Unknown`.

| Module | Future crate |
| --- | --- |
| `core` | `withnav-core` |
| `api` | `withnav-api` |
| `engine::{pipeline, authorize, turn}` | `withnav-engine` |
| `fakes` | test support in each crate |
