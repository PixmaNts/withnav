# InstinctFlow: a driver's guide
## 1. What a flow is
A **flow** is an acyclic set of **nodes** listening to one **trigger**. An **event**
arrives with a **payload**; the flow's `when` expression decides whether to start.
Each node declares its **inputs**. A **decision** asks the instinct a typed question;
an **expression** computes without a model; an **action** proposes something for
Navai to do. A node's active **port** carries the run along its wires. A **constant**
belongs to the definition; a **variable** exists only for this run. A **fallback**
replaces the run's proposed actions on failure. The Studio edits the definition,
Save writes it, and **reload** changes what runs. A **workspace flow** requires
explicit **trust**; a **global flow** comes from your own configuration.
```text
event + payload → trigger / when → decision → expression → action → then → action
                                      yes/no     true/false
                  failure anywhere → fallback (not the earlier proposed actions)
```
## 2. Quick start: match the message `test`
This example logs an exact message match. It needs no decision and therefore makes
no instinct call. It does not answer the message or change a permission.
1. Open the Studio from your workspace in the WebUI. Its route is
   `#/studio?ws=<absolute-workspace>&flow=<name>&loc=workspace`.
2. Choose **New flow**, name it `message-test`, choose **This workspace**, and set
   **Event it listens to** to `message.received`. Create writes an initial flow
   containing a `log` action; it does not reload it.
3. Select the trigger card. In its inputs, select `event.text`: the chip declares
   the alias `text`. **Declare the input first, then use its alias in `when`.**
   Set `when` to `text == "test"`. Do not write `event.text == "test"` there.
4. Select the `log` action and set its `text` parameter to `Matched test`.
   The trigger's event port must be wired to this action.
5. If the trust banner appears, inspect the workspace flows, then choose
   **Trust this workspace's flows**. Trust allows declared context reads, not
   tool permissions. Trust itself does not reload.
6. **Save**, then **Reload**. Check the reload result; `saved · not reloaded`
   means the file is newer than the running definition.
7. Click the sample payload in the bottom bar and replace it with:
   ```json
   {"command_id":"sample","author":"driver","text":"test"}
   ```
8. Choose **Run sample**. The path proposes `log`; application says **not applied**.
   This is correct: samples never apply actions. Change `text` to `Test` or
   `test\n` and run again: the path says **skipped: when is false**.
9. Choose **Clear run** to remove the run highlighting and recorded values.
The complete definition, also usable in the TOML view:
```toml
[flow]
version = 1
name = "message-test"
on = "message.received"
enabled = true
blocking = false
start = ["log"]
when = 'text == "test"'
[flow.inputs]
text = "event.text"

[[node]]
id = "log"
type = "action"
action = "log"
[node.params]
text = "Matched test"
```
`==` compares the whole string exactly, including case and whitespace; it is not
a contains test. Expression strings use **double quotes**. The surrounding single
quotes above are TOML's way of keeping those double quotes intact. There is no
`in [...]`: write `text == "test" || text == "hello"`. Flow inputs are local to
`when`; every node that reads text must declare its own `text = "event.text"`.
## 3. Studio tour and gestures
| Where | What to do there |
| --- | --- |
| Left: flow list | Select by trigger, name and location. See blocking/async and enabled status; create a flow; edit the selected flow's constants. An enabled toggle saves the flag, but still needs reload. |
| Top: flow header | Check location, path, unsaved/not-reloaded status, validation errors and health. Set blocking and its budget; Save, Discard or Reload. |
| Centre: Canvas | Add a decision, expression or action from the toolbar; arrange cards and connect ports. |
| Right: inspector | Select the trigger or a node to edit its fields, declared inputs, instructions, parameters and ports. Inspect rendered state and run detail. |
| Centre: TOML | Edit the same definition as text. Errors show locations; invalid text leaves the last valid canvas visible and locked. |
| Bottom: run bar | Edit sample JSON, Run sample, choose a record, View run, Replay, Clear run. Inspect path, proposed actions, failure, fallback and application status. |
| Health | Header/list counters for the last 24 hours of live runs: runs, failures, fallbacks applied, cost. Samples and replay do not count. |
| Gesture | Result |
| --- | --- |
| Click a card | Select its node and open its inspector. |
| Drag a card | Move it; positions are saved as layout, not behaviour. |
| Drag an output port to another card's input | Add a wire. One port can feed several nodes. Cycles and invalid connections are refused. |
| Click a wire, then **Delete** | Remove the selected wire. Backspace also works with canvas focus. |
| Click a card, then **Delete** | Remove that node and its wires. The synthetic trigger cannot be removed. Check inputs referring to the removed node. |
| Drag canvas background / scroll | Pan / zoom the React Flow canvas. |
| Escape in New flow | Close the creation dialog. |
| **Clear run** | Leave run mode: remove highlighted path, displayed run values and replay detail; keep the definition and stored records. |
Run mode is an overlay, not another definition and not a live subscription. Dimmed
nodes mean not reached in the displayed run, not disabled. Editing does not turn a
sample into a live run. Clear it before interpreting the canvas as your draft.
Built-ins are **read-only**: no card movement, wires, deletion or field editing.
To customise one, put a same-name definition (and any sibling template it uses)
in your workspace or global flows directory. Do not try to Save the built-in.
Text saves preserve comments; canvas saves rewrite canonical TOML and may lose
comments. Invalid TOML can be explicitly saved for later repair, but cannot run;
an invalid canvas cannot be saved. Samples can test valid unsaved edits without
silently saving or reloading them.
## 4. Nodes
### Decision: give the instinct evidence
Each decision has one `instructions` string and its own `[node.inputs]`. The
instructions describe the question; the inputs are the evidence sent as state.
**A decision needs inputs.** With no inputs, the default state is empty; the
provider rejects that request with HTTP 400 and the run reports `provider_error`.
Adding text to instructions does not supply state. Select at least a meaningful
payload or host input and inspect **State · inputs sent with the question**.
| Type | Configuration | Answer and active port |
| --- | --- | --- |
| `noul` | `instructions`, `threshold` (finite 0–1 number or `"constant.NAME"`) | Yes probability; `yes` when answer ≥ threshold, otherwise `no`. |
| `choice` | `instructions`, `[node.criteria]`: 2–16 named options, each with nonempty instructions | Option string; the matching criteria key is the port. |
| `score` | `instructions`, `levels`: 2–10 distinct labels, weakest first; `at_least`: exact label | Fractional expected index; `at_least` when ≥ that label's zero-based index, otherwise `below`. Not rounded to a level. |
```toml
[[node]]
id = "requested"
type = "noul"
instructions = "Does this message explicitly request a code change?"
threshold = 0.75
[node.inputs]
text = "event.text"
[node.next]
yes = ["audit"]
no = []
```
Destinations must exist; an empty port exits that chain. Labels are display text,
not names to reference. Ready decisions with identical rendered state and client
identity can share one call; cost belongs to the batch, not separately to each node.
Default state sorts aliases and renders `alias = <JSON value>` lines. Optional
`state_template = "state.md"` uses a relative sibling file with `{{alias}}`
placeholders, declared inputs only. No absolute paths, `..`, symlinks or includes.
Template literal braces escape as `{{{{` / `}}}}`. Studio carries these files
through saves; it does not edit templates. Images are currently refused.
### Expression: deterministic condition and variables
An expression has `condition` and ports `true` / `false`. `[node.assign]` declares
variables, evaluated in lexicographic name order before the condition; later
assignments can read earlier ones. Variables are immutable after the node completes
and start afresh on each run. They are not session settings or persistent memory.
```toml
[[node]]
id = "gate"
type = "expression"
condition = "is_set(p) && p >= THRESHOLD"
[node.inputs]
p = "answer.requested"
agent = "session.agent"
[node.assign]
THRESHOLD = 'if(agent == "plan", 0.9, 0.6)'
[node.next]
true = ["audit"]
false = []
```
### Action: catalogue parameters, not arbitrary code
An action has `action`, `[node.params]` (even if empty) and optional `then` wires,
whose destinations must be actions. Parameters must match the catalogue's types.
Use a literal, or `{ ref = "alias" }` for a typed declared input. Text parameters
also accept `{alias}` interpolation; `{{` / `}}` escape literal braces. Absent
values render as `—`. Placeholders do not evaluate expressions; instructions do
not interpolate. An action can only do what the catalogue offers (section 7).
```toml
[[node]]
id = "audit"
type = "action"
action = "log"
[node.inputs]
p = "answer.requested"
[node.params]
text = "Requested confidence: {p}"
[node.next]
then = ["another_action"]
```
## 5. Inputs and references
Declare `alias = "reference"` in `[flow.inputs]`, `[node.inputs]` or
`[fallback.inputs]` as appropriate. Expressions and parameters use **aliases**,
not dotted references. A declaration in one place does not declare it elsewhere.
| Reference | Value |
| --- | --- |
| `event.<field>` | One exact payload field offered by this trigger (section 7); no arbitrary nested traversal. |
| Host name, e.g. `session.agent` | Context collected by Navai, only when declared and needed. |
| `constant.MIN_YES` | Scalar from `[constants]`: string, finite number, integer or boolean. |
| `answer.requested` | Upstream decision answer: noul number, choice string, score number. |
| `answer.kind.confidence` | Upstream choice/score confidence. |
| `answer.kind.probabilities.option` | Choice probability for an option; score probability keys are zero-based index strings. |
| `result.gate` | Upstream expression's boolean condition result. |
| `variable.gate.THRESHOLD` | Variable qualified by the expression node that wrote it. |
Host inputs in the Navai catalogue (all transported as text; scalar types remain
available to expressions):
| Host input | Semantic type | Context / availability |
| --- | --- | --- |
| `session.id` | string | Session identity; all triggers. |
| `session.agent` | string | Current agent; all triggers. |
| `session.model` | string | Sanitised canonical model identity; all triggers. |
| `session.workspace` | string | Session workspace; all triggers. |
| `conversation.last_driver` | string | Latest driver request; all triggers. |
| `conversation.digest` | string | Conversation digest; all triggers. |
| `conversation.last_turns` | array | Recent turns; all triggers. |
| `intent.record` | string | Existing intent record; all triggers. |
| `workspace.root` | string | Workspace root; all triggers. |
| `workspace.branch` | string | Git branch; all triggers. |
| `workspace.dirty_files` | array | Changed files; all triggers. |
| `editor.open_files` | array | Editor context; save, idle, diagnostics, conversation only. |
| `editor.cursor` | object | Editor context; same editor triggers. |
| `editor.selection` | string | Editor context; same editor triggers. |
Offered does not mean present: unavailable session/editor/git context is **absent**.
Objects and arrays render as canonical JSON text; this does not enable indexing or
JSON traversal inside expressions. There are no inputs for credentials, arbitrary
files or provider destinations.
Output references must name **strict upstream ancestors**, not the node itself,
siblings or downstream nodes. The inspector disables non-upstream answers. An
upstream node not visited on this path produces absent, not zero, false or empty
text. A node starts on **any** active incoming wire (OR, not a join), then waits
only for the upstream outputs it declares to finish or become unreachable. Each
node runs at most once; independent chains can run concurrently.
Use `is_set(p) && p > 0.5`, or `if(is_set(p), p, 0.0)`, to handle absence safely.
Other operations on absent values fail the run. Decisions show `<absent>` in state;
cut inputs show `[cut]`. Fallback cannot read graph outputs after a failure.
## 6. Expression language reference
The same bounded language is used by `when`, `condition` and assignment values.
`when` and `condition` must produce a boolean.
| Syntax | Meaning |
| --- | --- |
| `true`, `false`, numbers, `"text"` | Boolean, finite numeric and string literals. |
| `+ - * / %`, unary `-` | Numeric arithmetic only. |
| `== != < <= > >=` | Comparisons, including string equality/order; compatible types required. |
| `! && ||` | Boolean NOT, AND, OR; AND/OR short-circuit. |
| `( … )` | Grouping; use parentheses to make intent clear. |
| `is_set(alias)` | Whether a declared input is present; argument must be an identifier. |
| `if(boolean, value, value)` | Lazy selection; both branch types must agree. |
No `in [...]`, ternary `?:`, string concatenation, exponentiation, regex, tuples,
indexing, dotted identifiers, arbitrary functions, semicolons or assignment
operators. Assignment belongs only in `[node.assign]`. Type mismatch, division
by zero or a nonfinite result fails, rather than being coerced.
Per expression: at most 4096 UTF-8 bytes, 128 lexical tokens, 32 operators,
16 nested parentheses, AST depth 32 and 128 AST nodes. A node may declare at most
16 inputs; an expression may declare at most 16 assignments.
## 7. Navai triggers and actions
Payload fields below are required unless marked `?`. `s` = string, `i` = integer,
`o` = object, `a` = array. Use `event.<field>` when declaring an input.
Unknown payload fields are rejected.

Action groups: **common** = `wake`, `inject_note`, `log`; **editor** = common plus
`remark`; **tool** = common plus `no_objection`, `deny`, `ask_driver`.

| Trigger | Payload fields | Can block | Allowed actions |
| --- | --- | --- | --- |
| `tool.pre_call` | `tool:s`, `args:o`, `call_id:s` | yes | tool |
| `message.received` | `command_id:s`, `author:s`, `text:s`, `selected_skill?:o` | yes | common |
| `turn.end` | `turn_id:s`, `text:s`, `status:s` | no | common |
| `save` | `uri:s`, `language_id:s`, `version:i`, `diff:s`; editor extras below | no | editor |
| `idle` | `uri:s`, `language_id:s`, `version:i`, `diff:s`, `idle_ms:i`; editor extras | no | editor |
| `diagnostics` | `uri:s`, `language_id:s`, `version:i`, `diagnostics:a`, `diff:s`; editor extras | no | editor |
| `conversation` | `turn_id:s`, `text:s`, `uri?:s`; editor extras | no | editor |

Every editor trigger also offers these **optional** payload fields, used by the
built-ins and migrated definitions (not host-input names):

| Type | Exact optional fields |
| --- | --- |
| string | `template_file`, `template_language`, `template_intent`, `template_diff`, `template_context`, `template_diagnostics`, `template_live_state`, `template_conversation` |
| integer | `changed_lines`, `since_last_remark_ms`, `since_last_wake_ms` |
| boolean | `called_for_help`, `has_errors`, `has_intent`, `has_conversation` |

Message authors are `driver`, `subagent` or `system`; `selected_skill` is
`{name: string, origin: "driver"}`. Turn status is `completed`, `interrupted` or
`failed`. Diagnostics entries carry `message`, numeric LSP `severity` and `range`.
Only root-session events emit flows in v1; editor events use attached root context.
`message.received` runs after admission checks, before transcript consumption;
`turn.end` is asynchronous after the turn settles. Tool preflight rejection is
final and is not offered to flows.

All listed parameters are required strings; `no_objection` has an empty map.

| Action | Parameters | Kind | What Navai does |
| --- | --- | --- | --- |
| `no_objection` | `{}` | verdict, priority 0 | Continue normal tool permission review, unchanged; not permission to run. |
| `ask_driver` | `note` | verdict, priority 1 | Hold the call for a driver-only permission request, skipping model reviews. |
| `deny` | `note` | verdict, priority 2 | Do not run the tool; return the note as the denied tool result. |
| `remark` | `prompt` | effect | Ask metacognition for an editor remark using inline rendered text, not a prompt-file lookup. |
| `wake` | `note` | effect | Bring the note to cognition's attention; wake delivery rules/cooldown still apply. |
| `inject_note` | `text` | effect | Deliver a flow-labelled note at a safe boundary. |
| `log` | `text` | effect | Annotate the trace only. |

Verdicts are only for **blocking `tool.pre_call` flows**, including fallbacks.
At most one verdict may be reachable per run. `wake` / `inject_note` have typed
non-driver provenance (`source: flow`, flow name, run id), remain untrusted data,
and never update the driver request, choose a skill, answer an ask or generate
recursive `message.received` events. No action runs arbitrary code, grants tool
permission, switches agent/model or starts a session.

## 8. Blocking, budget and fallback

Blocking holds the event while the flow runs. `tool.pre_call` can veto or ask;
blocking `message.received` only waits for routing effects, not permission.
Other triggers must be asynchronous. Async effects apply when the run finishes;
they cannot change an already-admitted event.

Set `blocking = true` and **required** `budget_ms = 1..2000`; omit `budget_ms`
when non-blocking. The budget is one deadline covering input collection, waiting
for call capacity and all chains, not a new allowance per decision. Async runs
have a 5-second host deadline; each System One call has at most 2 seconds.

Failure discards **all** proposed graph actions, even if another chain finished.
The matching fallback replaces them:

| Key | Used for |
| --- | --- |
| `on_failure` | General failure; expression, input, cancelled and limit failures. |
| `on_timeout` | Exhausted deadline. |
| `on_provider_error` | Failed provider call, including rejected empty state. |
| `on_invalid_answer` | Missing/wrong/out-of-range answer or malformed probability set. |

A specific key overrides `on_failure`. Each value is `{ action, params, then? }`;
inline `then` chains have depth at most 4 and still obey trigger/action validation.
Fallback inputs can read event, host and constants only. `failure_kind` and
`failure_message` are predefined aliases; do not redeclare them.

```toml
[fallback]
on_failure = { action = "log", params = { text = "Failed: {failure_kind}: {failure_message}" } }
```

Without a matching fallback, a blocking tool flow defaults to `ask_driver` with
`Flow failed: {failure_message}`. Blocking message flows have **no verdict default**:
the message continues after failure/deadline, with host outage handling. Async
flows have **no default fallback**; failure with none leaves **not applied**.
Cancellation of a no-longer-current event applies no live effect or fallback.

Several enabled flows on one event run with independent budgets. Blocking verdicts
combine as **deny > ask driver > no objection**. Tied notes are retained in flow-name
order; effects accumulate in flow-name / topological order, preserving `then`.
A flow's no objection never skips normal permission review. **Bypass never answers
a flow's `ask_driver`**, including follow-ups, and never defeats a flow's denial.
An approved flow-origin call is still rechecked for preflight, binding and hard floor.

Limits include 64 nodes/flow, at most 16 decisions reachable in a run, 64 loaded
flows/workspace, 16 KiB per collected input, 64 KiB total inputs/run and rendered
state, and 256 KiB definition/provider response. Cut text is marked, not hidden;
total-input/state overflow fails with `limit`. Effects already applied by the host
cannot be rolled back; failed or uncertain application is never retried automatically.

## 9. Files, trust, reload and TOML

| Location | Path / precedence |
| --- | --- |
| Workspace flow | `<workspace>/.navai/flows/<name>.toml`; trusted workspace wins by name. |
| Global flow | `~/.config/navai/flows/<name>.toml` (or configured config directory); wins over built-in. |
| Built-in | Shipped in Navai: `navigator-default-save`, `navigator-default-idle`, `navigator-default-conversation`; read-only. |
| Trust | `<config_dir>/flows-trust.json`, private driver-granted canonical-workspace trust, not tracked in the workspace. |
| Run records | `<data_dir>/flows/runs/<sha256-canonical-workspace>/runs.jsonl`, private captures. |

Untrusted workspace flows stay listed and editable, but cannot run, sample,
replay or shadow globals. Inspect files before granting trust. Missing/malformed
or insecure trust storage fails closed. Revoking trust stops new workspace runs
immediately; runs already in progress finish. Trust grants no model destination
or tool permission. Definitions/templates must not use symlinks.

**Save is not reload**, including enabled toggles and edits outside the Studio.
No definition-file watching: a `git pull` alone does not change what runs. Reload
via the Studio, `/flows reload`, or restart. All eligible definitions, including
disabled and shadowed files, are validated. Any invalid file prevents replacing
the whole running set; the previous set stays. Startup failure uses validated
built-ins only. In-flight runs keep their old immutable definition/templates.

A successful reload changes this process immediately and signals other Navai
processes to reload **on their next event**. It is not simultaneous application
across processes: inspect the propagation message and diagnostics in each process.
An unsaved draft is not part of reload.

Format essentials: one UTF-8 file per flow, version `1`; name must match filename
stem and `[a-z][a-z0-9-]{0,63}`. Node ids, aliases, constants, variables and choice
ports use `[A-Za-z_][A-Za-z0-9_]{0,63}`, case-sensitive. `trigger` is reserved as
the synthetic card id. `enabled` defaults true, `blocking` false, absent `when`
true; input/next maps default empty. `start` is required and nonempty; nodes are
required. Unknown fields, unreachable nodes, cycles, wrong ports, non-upstream
references and conflicting reachable verdicts are errors. `[layout]` is optional.

### Complete guard-mutations definition

This is the engine fixture, including its fallback choices and layout. It asks
about the most consequential effect, then whether changes were requested or how
far a destructive change could reach. Its timeout deliberately takes no objection
(normal review still applies). Choose that trade-off consciously for your own flow.

```toml
[flow]
version = 1
name = "guard-mutations"
on = "tool.pre_call"
enabled = true
blocking = true
budget_ms = 800
start = ["effect"]
when = 'tool == "bash" || tool == "write" || tool == "edit" || tool == "fetch"'
[flow.inputs]
tool = "event.tool"

[constants]
MIN_YES = 0.75

[fallback]
on_failure = { action = "ask_driver", params = { note = "Flow failed: {failure_message}" } }
on_timeout = { action = "no_objection", params = {} }
on_provider_error = { action = "ask_driver", params = { note = "Instinct unavailable" } }
on_invalid_answer = { action = "ask_driver", params = { note = "Invalid instinct answer" } }

[[node]]
id = "effect"
type = "choice"
instructions = "What will this call do? Choose its most consequential effect."
[node.inputs]
tool = "event.tool"
args = "event.args"
agent = "session.agent"
[node.criteria]
read_only = "Only reads files or prints information."
local_change = "Changes files inside the workspace."
network = "Reaches a host outside the machine."
destructive = "Deletes, force-pushes or rewrites history."
[node.next]
read_only = ["continue"]
local_change = ["asked_for"]
network = ["trusted_host"]
destructive = ["blast_radius"]

[[node]]
id = "asked_for"
type = "noul"
instructions = "Did the driver or the intent record ask for this exact change?"
threshold = "constant.MIN_YES"
[node.inputs]
last_driver = "conversation.last_driver"
intent = "intent.record"
args = "event.args"
[node.next]
yes = ["continue"]
no = ["plan_overreach"]

[[node]]
id = "trusted_host"
type = "noul"
instructions = "Is this a read-only request to an identifiable public documentation host?"
threshold = 0.9
[node.inputs]
args = "event.args"
[node.next]
yes = ["continue"]
no = ["ask"]

[[node]]
id = "blast_radius"
type = "score"
instructions = "How far could this reach if it goes wrong? Levels are weakest first."
levels = ["one file", "one folder", "whole repo", "outside the repo"]
at_least = "whole repo"
[node.inputs]
args = "event.args"
root = "workspace.root"
dirty = "workspace.dirty_files"
[node.next]
at_least = ["deny"]
below = ["ask"]

[[node]]
id = "plan_overreach"
type = "expression"
condition = 'asked_for < THRESHOLD'
[node.inputs]
asked_for = "answer.asked_for"
agent = "session.agent"
[node.assign]
# The plan agent should not change code: demand a clearer request from it.
THRESHOLD = 'if(agent == "plan", 0.9, 0.6)'
[node.next]
true = ["deny"]
false = ["ask"]

[[node]]
id = "continue"
type = "action"
action = "no_objection"
[node.params]

[[node]]
id = "ask"
type = "action"
action = "ask_driver"
[node.inputs]
effect = "answer.effect"
asked_for = "answer.asked_for"
[node.params]
note = "Review {effect}; requested confidence: {asked_for}."

[[node]]
id = "deny"
type = "action"
action = "deny"
[node.params]
note = "Change not requested, or destructive reach too broad. Propose it to the driver."
[node.next]
then = ["audit"]

[[node]]
id = "audit"
type = "action"
action = "log"
[node.params]
text = "guard-mutations proposed denial"

[layout]
trigger = { x = 16, y = 230 }
effect = { x = 226, y = 200 }
asked_for = { x = 476, y = 30 }
trusted_host = { x = 476, y = 250 }
blast_radius = { x = 476, y = 450 }
plan_overreach = { x = 726, y = 120 }
continue = { x = 966, y = 30 }
ask = { x = 966, y = 260 }
deny = { x = 966, y = 470 }
audit = { x = 1180, y = 470 }
```

## 10. Testing and records

- **Run sample** uses the current valid draft or saved definition and editable
  catalogue payload. On the real API it makes real instinct calls when decisions
  are reached, but **never applies actions**. Host context can be absent: Studio
  samples currently send no session id. A **mock API** banner means in-memory
  files and invented answers instead; nothing is saved to disk.
- **View run** reads the original record, including live application results;
  it neither calls the model nor runs the definition.
- **Replay** uses captured inputs and answers, never live context or model calls,
  and never applies actions. It is comparable only if selected inputs, cut flags,
  rendered state, question and model/variant match. Changed expressions/actions
  can take a new path using unchanged decisions. A newly reached uncaptured
  decision or changed question yields `missing_capture` / `question_changed`,
  not an invented answer. Noncomparable replay has no new outcome.
- **Run records** separate status (`success`, `skipped`, `failure`), mode
  (`live`, `sample`, `replay`), proposed actions, fallback and application
  (`not_applied`, `applied`, `failed`, `uncertain`). Inspect all of them: success
  means evaluation succeeded, not that an action ran. Batch usage/cost may be
  unknown; shared batch cost must not be counted once per node.
- **Health** covers 24 hours of production runs only. Repeated applied fallbacks
  indicate a budget, provider, input or definition problem worth fixing.

Captures contain payloads, code, rendered state and notes: treat them as private.
Directories are 0700, records 0600; retention is at most 7 days, 1000 records and
32 MiB per workspace. Redacted/missing evidence makes replay noncomparable.

### Command line

Export Navai's current host catalogue first; the standalone CLI does not infer it:

```sh
navai flows catalogue > catalogue.json
instinct-flow validate .navai/flows --catalogue catalogue.json
instinct-flow run .navai/flows/message-test.toml --payload event.json \
  --catalogue catalogue.json --endpoint https://your-provider.example/systemone \
  --model your-systemone-model
instinct-flow replay .navai/flows/message-test.toml --runs runs.jsonl \
  --catalogue catalogue.json
navai-lsp replay --flows --log navai.jsonl
```

For `run`, `event.json` contains just the payload object shown in quick start.
Supply the API key through `INSTINCT_API_KEY`, not a command-line literal.
The endpoint is the full `/systemone` URL. The CLI reads neither Navai's model
configuration nor `auth.json`, and its host inputs are absent. It prints a JSON
run record with proposed actions and no application; it is not a live host.
`validate` checks `.toml` files in the directory and sibling templates without
model calls. `replay` prints a replay result for each JSONL record.

`navai-lsp replay --flows` replays the **embedded editor flows** against legacy
`navai.jsonl` captures, preserving question/state provenance, without model calls.
It is not a path option for arbitrary workspace flows. `--json` produces a
machine-readable report; `--flows` cannot combine with `--pipeline` or `--reask`.

## 11. Troubleshooting

| Symptom | Cause | Fix |
| --- | --- | --- |
| `provider_error`, HTTP 400, decision has no inputs | Empty state; instructions alone are not evidence. | Select meaningful inputs on that decision, inspect rendered state, Save and Reload for live runs. |
| `skipped: when is false` | Payload does not satisfy exact comparison. | Inspect text, case and whitespace; edit sample JSON or correct the condition. |
| Unknown name in `when` | Alias not declared in `[flow.inputs]`, or dotted reference used as expression name. | Select the input chip first; bind `text = "event.text"`, then use `text`. Node inputs do not declare flow inputs. |
| `in [...]` / single-quoted expression string fails | Unsupported expression syntax. | Use double-quoted string literals and `||`: `text == "test" || text == "hello"`. |
| Workspace flow cannot sample/replay or does not run | Workspace is untrusted. | Inspect definitions; grant trust explicitly, then reload. Global/built-in flows remain available. |
| Saved but live behaviour is unchanged | Save did not reload, or another process has not seen the signal yet. | Reload, inspect errors/propagation; other processes reload on their next event. |
| Reload fails after editing a different flow | Replacement is all-or-nothing, including eligible disabled/shadowed definitions. | Fix every reported error; reload again. The previous set remains running. |
| Sample/replay says `not applied` | Intentional test behaviour. | Inspect proposed actions/path; use View run on a live record to inspect real application. |
| Async live failure says `not applied`, fallback none | Async runs have no default fallback. | Fix the recorded failure or declare an allowed effect fallback. |
| Wake suppressed: `wake cooldown` (120 s) | Wakes share the session cooldown; repeated wake is held back. | Wait 120 seconds; inspect application detail. Do not assume a proposed wake was delivered. |
| `instinct unavailable` notice | First provider error/timeout since startup or a successful call. Further outage notices are suppressed. | Check instinct assignment, System One endpoint/key and deadline; one successful call resets the notice. |
| Missing upstream input / expression failure | Node was not visited on this path, or host context unavailable. | Use `is_set(alias)` with lazy `&&` or `if`; read only strict upstream outputs. |
| Cards dimmed or inspector shows old values | A recorded/sample run overlay is still displayed. | Choose Clear run; it does not delete records or edits. |
| Cannot delete wire/card | Canvas not focused, built-in read-only, or invalid TOML locks editing. | Select it on an editable canvas and press Delete; fix TOML or create a same-name override for a built-in. |
| Save conflict / stale revision | Another edit changed the file on disk. | Preserve your draft separately, then Reopen the file; reopening loses unsaved edits. |
| Replay is not comparable | Request changed or required capture is absent/redacted. | Inspect per-node replay status; obtain a new sample/live capture rather than guessing an answer. |

Next: create `message-test`, declare `text`, and run the exact sample before adding
a decision.
