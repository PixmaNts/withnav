# Permissions are rules in the database, judged by a flow

Date: 2026-10-13

Status: Accepted — implemented. Supersedes [ADR 0010](0010-per-call-permission-review.md).
Amends [ADR 0008](0008-primary-agents-and-resumable-tool-authorization.md) (the role guard) and
[ADR 0013](0013-instinct-flow-engine-crate.md) (what a flow may decide). Tracked in issue #11.

The vocabulary is in [CONTEXT.md](../../CONTEXT.md).

## Context

ADR 0010 reviews every `bash`, `write` and `edit` call, and some reads, one by one: deterministic
checks, then Instinct, then metacognition, then the Captain. It made WithNav tiring to use and
expensive to maintain:

- The same safe command is asked again and again: folder approvals are reduced to "once" for
  `bash`, `write`, `edit`, `read`, `grep` and `glob`, and only `fetch` keeps reusable grants.
- On 2026-10-10, ten of eleven real metacognition reviews timed out (45–54 KB requests, up to
  40.4 s). Every timeout became a question to the Captain. When it did answer, metacognition
  escalated about one call in two, often for nothing.
- On 2026-10-12, `src/agent/permission_gate.rs`, its shell analysis and `src/agent/permissions.rs`
  held about 7 000 lines, and 203 commit messages since 2026-09-01 mentioned permissions, the
  gate, floors or sensitivity. Most of that work closed theoretical bypasses: symlink races,
  replaced binaries, shell expansion corner cases.

## Threat model

| Threat | Stance |
|---|---|
| The model acting against the Captain | Not defended against. The model works for the Captain. |
| An honest but destructive mistake (`rm -rf`, force push, overwrite outside the project) | Judged by Instinct; the Captain decides when Instinct objects. |
| A tool, MCP server, skill or flow the Captain installed doing what it is meant to do | The Captain's responsibility. Validated once, then trusted. |
| **Prompt injection**: instructions hidden in a skill, a fetched page, an MCP result, a repository file | **The main threat.** Untrusted text never authorises anything (see "What counts as the Captain's request"). |
| Local tricks: a fake `cargo` in `PATH`, symlink races, shell expansion games | Out of scope. They need an attacker who already controls the machine or the repository. WithNav is not a sandbox. |

## Decision

### Three control levels

1. **Yolo**: `"yolo": true` in `~/.config/withnav/config.json`. Nothing is checked: no rule, no
   flow, no Instinct request, no fixed floor, no role restriction beyond the tools an agent has.
   Every call records `approved_by: yolo`, and every client shows that yolo is on. Only the
   Captain's settings file turns it on; it is not a session toggle.
2. **Bypass**: the existing per-session switch (`/bypass on`, `nav run --yes`). `ask` rules and the
   flow are skipped and the call runs; a written `deny` rule and the fixed floor still apply.
3. **Normal**: rules, then the permission flow, then the Captain.

Yolo can only be turned on by the Captain: without yolo, the settings file is behind the fixed
floor. With yolo on, nothing stops the agent from turning it off, which is harmless.

### Rules live in the database

Rules are rows in a new table of `~/.local/share/withnav/withnav.db`, not a list in
`config.json`:

- **Shape**: `action` (tool name, `mcp__<server>__<tool>`, `external_directory`, `doom_loop`),
  `resource` (a pattern with `*`), `effect` (`allow`, `ask`, `deny`), `scope` (`global`, `project`
  with the workspace path, or `agent` with the agent name), origin (`always` answer, manual,
  flow), creation and revocation time.
- **Matching** uses SQLite `GLOB`, so one query returns every rule that matches a call, however
  many rules exist.
- **The most precise rule wins**: the one with the most fixed characters in `action` and
  `resource`. On a tie, the narrower scope wins (agent, then project, then global), then the
  stricter effect (`deny`, then `ask`, then `allow`).
- **No rule means `ask`.** WithNav ships with no default rule: on a new project the Captain
  answers often at first, and each "Always" makes the list of questions shorter.
- **History is kept**: a rule is revoked, never deleted. A second table records every decision:
  the call, the matching rule, the flow's verdict, the Captain's answer, the session. It measures
  how many questions a day of work costs and shows which answers could become rules.
- **Managed with `nav permissions list | add | revoke | history`.** A web screen comes later.

`grants.json`, `skill-grants.json` and `flows-trust.json` are replaced by tables of the same
database. Their content is not imported: the only user starts again from an empty table.

Settings JSON is read into a sorted map, so an ordered rule list in `config.json` could not keep
its order; this is one more reason the rules are rows.

### Bash: commands and arity

A command line is split into its commands (pipes, `&&`, `||`, `;`, subshells), and each command is
checked on its own. "Always" saves the command's prefix followed by ` *`, using an **arity table**
that says how many words identify a command: `git: 2`, `git config: 3`, `cargo: 2`,
`npm run: 3`, `docker compose: 3`, `rm: 1`, `cat: 1`. Options (words starting with `-`) are not
counted. An unknown program is identified by its first word. Answering "Always" to
`git push origin feature` saves `bash` / `git push *`, and that rule also matches a bare
`git push`. Commands are normalised before matching (leading assignments, redirections and repeated
spaces removed), and a line that cannot be parsed is checked as one opaque command, never as none.

### Questions: Once, Always, Reject

- **Once** runs this call.
- **Always** runs it and saves an `allow` rule. Its scope is shown on the button and depends on
  the tool: project for `bash`, file tools and `fetch`; global for MCP tools, skills and flows. The
  Captain can change the scope later with `nav permissions`. Pending calls of the same turn that
  the new rule now allows run too.
- **Reject** does not run the call. The Captain may add a message, which the model reads as the
  reason. Every other pending question of the same turn is rejected with it.

### Decision order

For each tool call:

1. **Yolo**: run.
2. **Fixed floor** (below): handled as described there.
3. **Reads inside the project** (`read`, `grep`, `glob` within readable roots): run. No rule, no
   flow.
4. **Rules**: a `deny` stops the call. Nothing later can change it, not the flow, not a skill.
5. **Bypass**: run.
6. **Permission flow** with the rule's result (`allow`, `ask`, or no rule). It answers `allow`,
   `ask_captain` or `deny`.
7. **Captain**: Once, Always or Reject.

### The permission flow

The judgement is a flow on the `tool.pre_call` trigger (ADR 0013), not Rust code. A built-in flow
ships with WithNav; its questions, thresholds and prompts change in the Studio and take effect at
the next reload, without recompiling.

The built-in flow starts with three questions:

- Rule `ask` or no rule:
  - "Did the Captain, or the validated skill in use, ask for this action?" Yes ≥ 0.6: `allow`.
  - Otherwise, "Is it a read, or a recoverable change inside the project?" Yes ≥ 0.7: `allow`.
  - Otherwise: `ask_captain`.
- Rule `allow`:
  - "Is this exact call destructive, irreversible, does it publish something, or touch
    credentials?" Yes ≥ 0.5: `ask_captain` (a `git push --force origin main` under a saved
    `git push *`).
  - Otherwise: `allow`.

The thresholds are flow constants. The decision table is what tunes them.

What the flow receives, in addition to today's `tool.pre_call` payload: the rule's result and the
rule that matched; for `bash`, the commands of the line with their arity prefix; for file tools,
whether the target is inside the project; the validated skill in use, with its instructions; and
the Captain's messages of the session within a byte budget, not only the last one.

**What counts as the Captain's request**: the Captain's own messages, and the instructions of a
skill the Captain validated and that is in use in this turn. Never tool output, fetched pages, MCP
results, repository files or the model's own text. A validated skill can therefore open an `ask`
rule (a skill that says "push the branch" pushes without a question), never a `deny`.

**Changes to ADR 0013**:

- A flow may now **allow** a call: the `no_objection` action becomes `allow`. When several flows
  run on the same call, the strictest answer wins: `deny`, then `ask_captain`, then `allow`.
- Any flow may allow, workspace flows included. The documentation warns that a workspace flow can
  open permissions, which is why every flow must be validated (below).
- A failed run (timeout, provider error, invalid answer) falls back to **`apply_rule`**: the rule's
  result applies, `allow` runs and `ask` asks. An Instinct outage does not turn into a stream of
  questions. This replaces "a blocking flow asks the Captain" as the default fallback. Each
  flow's `apply_rule` is resolved to `allow` or `ask_captain` before the strictest answer is
  chosen, and no flow verdict at all also applies the rule.

### Metacognition leaves the decision

Metacognition no longer reviews tool calls. `permissions.automatic_review` and
`permissions.review_timeout_secs` are removed. A metacognition decision node, usable at the end of
a flow with its own longer budget, is planned in a separate issue; it is not delivered with this
change. Its later role is to watch the conversation for drift and injection, not to review calls
one by one.

### The fixed floor

The only checks written in Rust, skipped only by yolo:

| Target | Read | Write |
|---|---|---|
| `config.json`, `withnav.db` and its `-wal` / `-shm` files, the global flows directory | free | only when the permission flow's protected check answers that the Captain explicitly asked for it or accepted the agent's proposal to do it (threshold 0.9); a skill cannot satisfy it, a failed or skipped check asks, and "Always" never covers it |
| Credentials: `auth.json`, `~/.ssh/id_*`, `.env` and the existing sensitive-name list | asks | asks |

Bypass does not skip the floor. For bash the floor looks at literal paths and redirections of each
command, and at WithNav's own commands that change rules (`nav permissions add`); it is best effort.

### Validated skills and flows

A skill or a flow must be validated once by the Captain before it runs. The validation is bound to
a hash of its content; a changed file must be validated again. The built-in flow needs none.
Skills and flows share one validation table in the database, which replaces `skill-grants.json`
and `flows-trust.json`. A future skill checker reviews a new or changed skill before the Captain
validates it; it is a separate issue.

### Agents and the role guard

The role guard (`agent/guard.rs`, the `role_conformance` question, the `plan` Markdown exception)
is removed. An agent's limits are rules with `agent` scope that the Captain adds, for example `plan`
with `write` / `*.md` / `allow` and `write` / `*` / `ask`. WithNav seeds none, consistent with "no
default rule": without them, `plan`'s writes are judged like any other call and its prompt still
tells it to write only Markdown. This amends ADR 0008: the role still does not remove tools, and
its limits are now rules like any other.

### What is removed

- The per-call review: Instinct questions written in Rust, routing, evidence building, the
  metacognition reviewer and its settings.
- The shell risk analysis, replaced by command splitting and the arity table.
- Folder grants and the "folder becomes once" downgrade; fetch origin grants become rules.
- Target re-resolution before dispatch, no-follow binding of reviewed targets, the executable
  identity checks, the toolchain directory floor, the changed scratch root floor, Captain
  precedents and config session grants.
- The tests dedicated to these, replaced by a smaller suite for the rules, the flow and the floor.

Kept: permission requests and answers on the wire, the HTTP answer route and the three clients'
cards (simplified to Once / Always / Reject with a message), `approved_by` on every call (rule,
flow, Captain, bypass, yolo), and tool availability per agent.

### Delivery order

1. Yolo mode, alone, on top of the current system.
2. This model, in steps: rules and the database, Once / Always / Reject, bash splitting and arity,
   the permission flow, then the removals.
3. Windows support (#8) continues on the new model, without porting the removed checks.

## Considered options

- **Rules in `config.json`** as an ordered list: rejected. The file grows without bound, rules
  cannot be queried, and an object form would lose its order.
- **Last matching rule wins**: needs an ordered list, which the database does not have.
- **Strictest rule wins**: makes exceptions impossible (`ask *` with `allow git status *` would
  always ask).
- **Instinct can only make a call stricter**: keeps every `ask` as a question even when the Captain
  asked for the action three messages earlier.
- **Keep metacognition as the reviewer when Instinct is unsure**: it timed out ten times in eleven
  and escalated half of its answers; it returns later as an optional flow node.
- **Only global flows may allow**: rejected for simplicity; every flow is validated instead.

## Consequences

- The number of questions is driven by the Captain's rules, not by the gate's caution, and
  decreases over the life of a project.
- An Instinct outage leaves the rules in force instead of asking about every call.
- A validated workspace flow or skill can open permissions. That is the price of trusting what the
  Captain validated, and it is documented.
- Bash remains unconfined and nothing claims otherwise.

## Implementation notes

Decisions taken while implementing this ADR that refine it:

- **Protected writes.** A write to `config.json`, `withnav.db` or the global flows directory runs
  automatically only when the protected check compiled into the binary answers that the Captain
  asked for it, or explicitly accepted an agent proposal to do it. Otherwise the Captain is asked and can approve once. A flow on disk,
  even a copy of the built-in one, can make that answer stricter but never grant it. The check is
  asked once per call and its answer is passed to the flows.
- **Permission database.** If the database cannot open, the session starts in degraded mode and
  shows the error. Calls are judged as having no rule: the fixed floor and permission flow still
  apply, and the flow may allow. Bypass cannot answer, Always is not offered, and decisions cannot
  be saved. This deliberately permits flow-approved actions while written deny rules are invisible.
  Yolo remains unchecked.
- **Skills.** Validation proves which content the Captain trusted; it does not override rules.
  Loading a validated skill is decided with action `skill` and the skill's name: a `deny` rule
  refuses it. With no matching rule it gets an effective allow, but the permission flow still runs
  and can ask or deny; `approved_by: validation` is recorded when that default applies. An `ask`
  rule can also be allowed by the flow. A skill the Captain selects counts as the Captain's request.
- **Subagents.** A child's permission flow sees its parent's Captain messages, captured when the
  child is created and refreshed when the parent steers it. The delegated prompt and the steering
  text are supplied separately as `conversation.delegation`, with attachments removed and a 16 KiB
  whole-message budget. The ordinary permission flow may treat delegation as a request; the
  protected check never reads delegation.
- **Temporary writes.** A native `write` or `edit` whose resolved file is inside the system
  temporary directory (`std::env::temp_dir()`, plus `/tmp` on Unix) or a scratch directory runs
  without the flow, recorded as `approved_by: rule`, next to free reads. The fixed floor, a written
  `deny` and a written `ask` still apply before it, and a project that lives in the temporary
  directory is judged as a project. Bash is left to the flow, whose state says that creating or
  changing files in the project, its scratch directories or the temp directory is recoverable.
- **No Always for destructive commands.** "Always" saves a prefix rule, so it is not offered for a
  shell command that deletes, moves or overwrites files, changes permissions, kills processes or
  rewrites history, nor for one that writes through a `>` or `>>` redirection to a file. The list
  is one constant, `permission::shell::NO_ALWAYS`; `sqlite3 -readonly` stays a read.
- **Recent exchanges.** The permission flow and the protected check both receive
  `conversation.recent`: the last 4 messages between the Captain and the agent, in order, each
  labelled `captain` or `agent`, Captain text stripped of attachments like the Captain's messages,
  whole messages within 8 KiB with a cut flag. A short answer such as "yes go" is read with the
  agent proposal it answers, so accepting a proposal counts as a request, protected writes
  included. The agent's message alone never counts, whatever it says the Captain approved.
- **Answers to `ask`.** The Captain's answer to an `ask` counts as the Captain's words, labelled
  with the assistant's question, after editor selections and skill blocks attached to it are
  removed.
- **Opaque shell commands.** A command the splitter cannot parse matches only a `deny` rule whose
  resource is exactly `*`. It never matches an `allow` or `ask` rule, and "Always" is never offered
  for it.
- **Not decided.** `task`, `steer_subagent`, `decide_subagent` and `intent` act only on the session,
  its children and the intent record; they are not decided. Each child's own calls are.
- **Validation scope.** Validation gates what runs on real events. Studio sample runs, replays and
  the standalone `instinct-flow` command evaluate a definition without applying actions, and are
  not gated by validation.
- **Bash.** The floor's checks on bash are best effort: literal path words, redirection targets and
  output options. Variables and globs are not expanded.
