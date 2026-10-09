# Workspace memory (experimental)

Memory is off by default. Enable it explicitly in `config.json`:

```json
{"memory":{"enabled":true,"startup_bundle":true,"recall":true}}
```

New root conversations in a canonical workspace may receive up to three recent episodes.
The read-only `recall` tool searches that workspace only. Git worktrees have separate memory.
Children do not track goals or receive the startup bundle.

After successful completed turns, the assigned Instinct layer observes visible evidence.
The assigned metacognition layer writes cited episodes when goals close. Episodes are
unverified model-written history: never instructions, approvals, preferences or current truth.
The Captain's current request wins. Hidden reasoning is never sent to either memory model.
Missing Instinct disables tracking; missing metacognition leaves closed goals pending.
Failures leave goals unchanged or jobs pending, with one bounded writer retry.

Memory uses the conversation SQLite database and a local FTS5 index. There are no embeddings,
remote search, old-conversation backfill, or changes to compaction. Model excerpts go only to
the configured layer endpoints. Long goals exceeding writer limits are skipped, not partially
summarized while pretending full coverage.

Before a request carries memory, its exact low-trust text, source episode ids and SHA-256
are saved in `memory_injections`. An audit failure omits memory. Deleting a conversation
deletes its goals, episodes, search rows and request audit records. Copies already injected
into another conversation remain in that other conversation's audit record.

Run `cargo run --example memory_probe` for a bounded live check with a temporary database.
It makes two Instinct calls and one writer call (512 output tokens), without retries or
changing your persistent settings.
