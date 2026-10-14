# Permissions

Nav can read files, change them and run commands. WithNav decides each call with your rules and
asks you when they do not cover it, so you stay in control.

> This guards against drift. It is **not a sandbox**. `bash` is not confined to your workspace,
> and detection of sensitive files inside a shell command is best effort. Treat approving a
> command as running it yourself.

## How a call is decided

Every tool call goes through the same steps, in this order:

1. **Yolo** (if you turned it on): the call runs, nothing is checked.
2. **Fixed checks**: a call that touches credentials (keys, `auth.json`, `.env` files) always asks
   you. A write to WithNav's own `config.json`, database or global flows folder runs only if you
   explicitly asked for it, or explicitly accepted the agent's proposal to do it; otherwise it asks
   you. A command that merely names one of these files (`sqlite3 …/withnav.db "SELECT …"`, a script
   given the path) is judged by the same question, which also lets a pure read run: a query that
   reads your rules runs, one that deletes them asks.
3. **Reads inside the project** (`read`, `grep`, `glob`) run without asking. So do **writes to a
   temporary folder**: a `write` or `edit` whose file is in the system temp directory (such as
   `/tmp`) or a scratch folder, unless a rule of yours denies or asks about it. A `bash` command
   that writes there is still judged by the permission flow: anything inside WithNav's scratch
   folders (`/tmp/withnav`, `$XDG_RUNTIME_DIR/withnav`, or your `permissions.scratch_dirs`),
   deleting included, is recoverable; in the rest of the temp directory, creating a new file is
   recoverable, but deleting or overwriting an existing one is not, since other programs keep
   their files there.
4. **Your rules**: a `deny` rule stops the call, whatever comes after.
5. **Bypass** (if on for this session): the call runs.
6. **The permission flow**: Instinct judges whether you, or a skill you validated, asked for the
   call (including a parent agent's delegation to a subagent), whether it only reads, or whether
   it is a recoverable change: files in the project, its scratch folders or the temp folder, or
   building, testing, formatting or linting the project. It can let the call run, or ask you.
   Under an **allow** rule it asks only whether this exact call is dangerous (destructive,
   irreversible, publishing or touching credentials); a dangerous call still runs if you asked for
   it, and asks you otherwise. So an **Always** never makes a dangerous call run unasked, and
   never stops one you explicitly asked for.

The permission flow reads all your messages and the last four messages of the conversation, yours
and the agent's, up to your last message. A short answer such as "yes, go" therefore accepts the
proposal the agent just made, and counts as your request, including for WithNav's own
configuration. A message from the agent alone never counts, whatever it says you approved. What
the agent wrote after your last message (reading a file, reasoning, announcing the command it is
about to run) is not read at all: if you ask it to "apply" a file, a command found in that file
is not your request until you have seen it and said yes.
7. **You**: Once, Always or Reject.

There are no default rules: on a new project you answer more often at first, and each **Always**
makes the list of questions shorter.

A `bash` line is split into its commands (pipes, `&&`, `;`, subshells) and each is checked on its
own; the strictest answer wins. A line WithNav cannot parse matches only a `deny` rule on `*`, and
**Always** is never offered for it. MCP tools are decided like any
other tool: with no matching rule, the default is ask. The permission flow may still allow the
call, for example when the Captain asked for it. Without a flow that answers, WithNav asks.

## Rules

A rule says what happens to a tool call: an **action** (the tool name, such as `bash`, `write`,
`fetch` or `mcp__github__create_issue`, or `external_directory` for files outside the project), a
**resource** pattern where `*` matches anything, an **effect** (`allow`, `ask` or `deny`) and a
**scope** (one project, every project, or one agent). The most precise rule wins; on a tie the
narrower scope wins, then the stricter effect. A rule is revoked, never deleted.

```sh
nav permissions list [--all] [--project <dir> | --global | --agent <id>] [--json]
nav permissions add <action> <resource> <allow|ask|deny> [--project <dir> | --global | --agent <id>]
nav permissions revoke <id>
nav permissions history [--limit <n>] [--json]
```

`add` defaults to the current project. Examples:

```sh
nav permissions add bash 'git status *' allow          # this project
nav permissions add bash 'git push --force *' deny --global
nav permissions add write '*.md' allow --agent plan     # plan may write Markdown
nav permissions add write '*' ask --agent plan
```

`history` shows every decision: the call, the matching rule, the flow's verdict and the scores
behind it (for example `permission-default requested 0.04 read_only 0.01 recoverable 0.05`), your
answer and
the outcome (`allowed`, `denied`, `pending`, `cancelled`, `expired`). Arguments are stored with
secrets redacted and size-limited. `--json` adds `flow_trace`: the run id of each flow run, which
finds the full run, its inputs and the exact text sent to Instinct, in
`~/.local/share/withnav/flows/runs/<workspace hash>/runs.jsonl`. A question also shows these scores
in its reason.

Rules and history live in `~/.local/share/withnav/withnav.db`. If it cannot open, the session starts
with **Permission rules unavailable** and the error. Every call is judged without a rule: the floor
still applies and the permission flow may allow it, otherwise you are asked. Bypass cannot answer
and Always is unavailable; decisions cannot be recorded. **Written deny rules are invisible in
this mode.** Fix the database and start a new session to restore them. Yolo remains unchecked.

## Answering a prompt

| Answer | Meaning |
|---|---|
| **Once** | Run this call. |
| **Always** | Save the `allow` rule shown on the button, then run the call. Calls of the same turn that the new rule covers run too. Not offered for credentials, WithNav's own configuration, a command WithNav cannot split, or a command that deletes or changes files, processes or history. |
| **Reject** | Do not run it. You can add a message that the model reads; every other waiting call of the turn is rejected with it. |
| **Validate this skill** | Offered the first time a skill (or a changed version of it) is loaded. **Always** keeps the validation for this exact content. |

In the terminal these are the keys `o`, `a` and `r`. In VS Code and the browser they are buttons on
a card that shows the reason and the call.

What **Always** saves:

| Call | Rule saved | Scope |
|---|---|---|
| `bash cargo test -p withnav` | `bash` / `cargo test *` (the command's prefix, which also matches a bare `cargo test`) | this project |
| `bash rm -v notes.txt`, `bash echo x > f` | nothing: **Always** is not offered for a command that deletes, moves, overwrites or kills (see below) | |
| `write src/main.rs` | `write` / the exact file path | this project |
| a file outside the project | the file rule, plus `external_directory` / its folder | this project |
| `fetch https://docs.rs/serde` | `fetch` / `https://docs.rs/*` | this project |
| a skill rule question | `skill` / the exact skill name | every project |
| an MCP tool or another tool not listed above | the tool name / `*` | every project |

**Always** is never offered for a shell command that deletes, moves or overwrites files, changes
permissions, kills processes or rewrites history: `rm`, `rmdir`, `mv`, `cp`, `dd`, `shred`,
`truncate`, `chmod`, `chown`, `ln`, `tee`, `install`, `sed -i`, `perl -i`, `sqlite3` without
`-readonly`, `git push`, `git reset`, `git clean`, `git checkout`, `git restore`, `git rebase`,
`git branch -D`, `git tag -d`, `kill`, `killall`, `pkill`, and any command that writes to a file
with `>` or `>>`. A rule such as `rm *` would allow every future deletion. Answer **Once**, or
write a narrower rule yourself with `nav permissions add`. Reads keep their prefix rule
(`sqlite3 -readonly *` saves `sqlite3 *`, `git status` saves `git status *`).

On a skill-validation question, **Always** stores a validation of that exact content (its hash),
not a rule.

Change or remove a saved rule with `nav permissions`. A subagent's requests are answered in its
parent's conversation.

A subagent receives your genuine messages and, separately, the task and guidance its parent agent
gave it. Delegation may request ordinary actions; only your own words can request changes to
WithNav's configuration or database.

## Validated skills and flows

A skill or a flow runs only after you validate it once. The validation is bound to a hash of its
content: a changed file asks again.

- **Skills**: the first time a skill (or a new version of it) is loaded, Nav asks you. **Once**
  loads it for this turn, **Always** keeps the validation, **Reject** does not load it. A validated
  skill still obeys your rules: `nav permissions add skill <name> deny` refuses it.
- **Flows**: a global or workspace flow does not run until you validate it, in the Studio
  (**Validate this flow version**) or with `nav permissions validate flow <name>
  [--location global|workspace]`. The built-in flows need no validation.

```sh
nav permissions validations [--json]
nav permissions revoke-validation <id>
```

> **A validated flow can allow tool calls.** That includes a workspace flow, which comes with the
> repository you opened. Read a flow before you validate it, as you would read a script before
> running it. A validated skill can also ask for an action, and the permission flow may then allow
> it without asking you; it can never lift a `deny` rule.

## Bypass

**Bypass** lets one session skip ordinary permission prompts.

- Turn it on with `/bypass on` (and `/bypass off`), `alt+p` in the terminal, the **WithNav: Toggle
  Bypass Permissions** command in VS Code, the **Bypass** button in the browser, or
  `nav run --yes`.
- With it on, a call that a rule `ask` or the permission flow would have asked about runs once
  instead.
- A `deny` rule still denies. Requests for WithNav's own configuration, credentials and keys
  **still ask you**. `nav run --yes` stops with exit code 3 on one.
- It is off for every new or resumed session and is never saved. A session's subagents follow it.
- A clear warning shows while it is on: `⚠ bypass` in the terminal, a badge in VS Code and the
  browser.
- It is not a sandbox: `bash` keeps its access to your machine.

## Yolo

> **Warning: yolo turns every permission check off.** The agent can change or delete anything your
> account can reach, read your credentials and keys, push, publish and run any command, with no
> prompt, no rule and no `deny`. A prompt injected into a web page, a file or a tool result can
> make it do so. Use it only in a disposable machine or container you can throw away.

Set `"yolo": true` in `config.json` (see [Configuration](configuration.md)). It is off by default,
applies to new sessions, and their subagents inherit it.

There are three levels of control, from the strongest: **normal** (rules, the permission flow and
prompts), **bypass** (ordinary prompts skipped for one session; `deny` rules and protected requests
still apply) and **yolo** (nothing is checked).

- A tool call runs without a prompt, without rules, without the permission flow or any
  `tool.pre_call` flow, without an agent's write scope, and without the protected-configuration,
  credentials and scratch-folder checks. File tools may reach outside the workspace, and `grep` and
  `glob` may start in a credentials folder (the credential files in it are still left out of the
  results). No Instinct request is sent at all.
- Each call records `approved_by: yolo` in its `tool.pre_call` and `tool.post_call` events and
  in the decision history.
- Still enforced: which tools a session or agent has, malformed or missing arguments, and
  cancellation. A refused call still says `approved_by: yolo`.
- `YOLO` shows in the terminal header and sidebar, as a badge in the browser and in VS Code, and
  `nav doctor` prints `permissions: yolo (no permission control)`.
- Changing the setting applies to new sessions, not to open ones.

## Agents

An agent's limits are rules with `agent` scope that you add, for example `plan` with `write` /
`*.md` / `allow` and `write` / `*` / `ask`. WithNav seeds none.

## Scratch folders

The agent may use two folders for temporary files; they count as part of the project:

- A private one: `$XDG_RUNTIME_DIR/withnav`, or `~/.cache/withnav/scratch` if that is not usable.
- A shared one: `<temp dir>/withnav`, usually `/tmp/withnav`.

Each must be a real directory owned by you with mode 0700. A folder that is replaced, made public
or reached through a symlink is no longer written to. Change the list with
`permissions.scratch_dirs` (absolute paths; `[]` turns it off).

## Long turns and the watchdog

A conversation has no step limit by default. Instead, every 300 seconds
(`agent.watchdog_interval_secs`; `0` turns it off), Instinct checks that the turn is making
progress.

- **Active:** the turn continues.
- **Stuck:** the turn stops after its current tools finish, keeping the work done.
- **In doubt:** Metacognition decides. If it hesitates too, the turn pauses and asks you.
  `alt+c` in the terminal (or the Continue button) continues it. A new message replaces the paused
  turn.

If the watchdog is off, or Instinct is unavailable, a visible 500-step cap applies instead, and
only you can renew it. `WITHNAV_MAX_STEPS` sets an explicit cap. Subagents keep their own 50-step
budget.
