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
   you. A write to WithNav's own `config.json`, database or global flows folder runs only if your
   own messages explicitly ask for it; otherwise it asks you.
3. **Reads inside the project** (`read`, `grep`, `glob`) run without asking.
4. **Your rules**: a `deny` rule stops the call, whatever comes after.
5. **Bypass** (if on for this session): the call runs.
6. **The permission flow**: Instinct judges whether you, or a skill you validated, asked for the
   call, or whether it is a read or a recoverable change inside the project. It can let the call
   run, or ask you.
7. **You**: Once, Always or Reject.

There are no default rules: on a new project you answer more often at first, and each **Always**
makes the list of questions shorter. Manage rules with `nav permissions list | add | revoke |
history`.

## Answering a prompt

| Answer | Meaning |
|---|---|
| **Once** | Run this call. |
| **Always** | Save the rule shown on the button (for example `Always (project): bash git push *`), then run the call. Calls of the same turn that the new rule covers run too. Not offered for credentials, WithNav's own configuration, or a command WithNav cannot split. |
| **Reject** | Do not run it. You can add a message that the model reads; every other waiting call of the turn is rejected with it. |
| **Validate this skill** | Offered the first time a skill (or a changed version of it) is loaded. **Always** keeps the validation for this exact content. |

In the terminal these are the keys `o`, `a` and `r`. In VS Code and the browser they are buttons on
a card that shows the reason and the call.

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

> **Unsafe.** Yolo turns permission control off. The agent can change or delete anything your
> account can reach, read your credentials and run any command, with no prompt and no review.
> Use it only in a disposable machine or container.

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
- Each call records `approved_by: yolo` in its `tool.pre_call` and `tool.post_call` events.
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
