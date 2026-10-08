# instinct-flow

An independent Cargo library and CLI. It validates immutable decision graphs, calls an
explicit decision-client seam, and returns proposed actions and a private JSON capture.
It never applies actions, reads Navai settings, resolves credentials or stores captures.

## Host API

1. Supply a `Catalogue` describing triggers, host inputs, actions and ceilings.
2. `Definition::parse(text)` and `validate(&catalogue, &capabilities, &templates)` produce
   a `CompiledDefinition`; templates are already-loaded sibling file contents.
3. Implement object-safe `InputSource` and `DecisionClient` (or use `SystemOneClient`).
   Input collection must refer to a stable start-of-run snapshot even when lazy.
   `InputValue { value, cut, redacted }` retains real values for live execution; a redacted
   value is withheld from the capture and replaced by `<redacted>` in retained states.
4. `run` takes an absolute deadline and cancellation token and returns `RunOutcome`.
   Failed runs discard all ordinary actions. The host decides whether the event is still
   current before applying fallback proposals.
5. Combine only blocking outcomes with `combine_verdicts`, then apply selected actions
   through the host's `ActionExecutor`. Capture application results in `RunRecord`.
6. `replay` uses captured context/questions/answers only and returns no outcome if a newly
   reached decision cannot be compared honestly. It does not invoke any trait callbacks.
   Version 1 records optionally add `NodeRecord.selected_inputs` (alias → reference and
   captured value/cut/redacted). Changed references or constants invalidate decision reuse;
    missing or redacted host evidence invalidates every consumer, including expressions/actions.
    Hosts list removed event evidence in `RunRecord.redacted_event_paths` (for example,
    `event.text` or `event.args`); replay rejects consumers of those references, including
    flow-level `when`, without treating replacement text as original evidence. Older records
    may omit this field; unredacted event fields remain comparable.
   Legacy records without selected inputs require the original definition digest for reuse.

Input byte ceilings are allocated equally across distinct declared non-upstream references
(including flow and fallback inputs), independently of completion order. Text may be cut;
typed parameters and expressions fail with `limit` when truncation would be required.
State and action rendering enforce ceilings during construction, without cutting templates.

The shared System One wire DTOs live in `instinct_flow::protocol`; the root exports the
catalogue, definition, client, engine and record DTOs. `Node` contains common fields and
`NodeKind`; JSON/TOML flatten the kind into the normative `type` discriminant.

## CLI

```sh
instinct-flow validate flows --catalogue catalogue.json
INSTINCT_API_KEY=... instinct-flow run flows/example.toml --payload event.json \
  --catalogue catalogue.json --endpoint https://provider.example/systemone --model model-id
instinct-flow replay flows/example.toml --runs runs.jsonl --catalogue catalogue.json
```

CLI host context is absent. Run payload files contain the trigger payload object, not a
wrapped Event. Output is JSON; cost remains unknown without external pricing evidence.
See `NOTES.md` for conservative resolutions and host filesystem responsibilities.
