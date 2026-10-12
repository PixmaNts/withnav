# Permissions

Nav can read files, change them and run commands. WithNav puts a gate in front of the calls that
can do harm, so you stay in control.

> The gate guards against drift. It is **not a sandbox**. `bash` is not confined to your workspace,
> and detection of sensitive files inside a shell command is best effort. Treat approving a
> command as running it yourself.

## What is gated

| Tool | Gate |
|---|---|
| `write`, `edit`, `bash` | Every call is reviewed before it runs. |
| `fetch` | Asks first. |
| `read` outside the project, or of credentials | Reviewed like `bash`. |
| `grep`, `glob` outside the project | Refused. Use `read` or `bash` for that. |
| `read`, `grep`, `glob` inside the project | Run without asking. |

## How a call is decided

1. **Fixed checks** look at the call: redirections, commands that change things, the real
   destination of a path. They only ever make a call stricter.
2. **Instinct** answers a few typed questions about the call: its effect, its scope, whether it
   can be undone, whether it publishes anything, how sensitive it is and whether it fits your
   task.
   - A confident, read-only call runs on Instinct's answer alone.
   - A low-risk change inside the project that you can recover from (for example with git) also
     runs. Builds, tests and commits are typical.
3. **Metacognition** reviews the rest, with no tools, and either approves or does not. It sees your
   recent messages, recent work, and the script a command runs.
4. **You** are asked about anything that is not approved: a rejection, an unsure answer, a malformed
   answer, a timeout, a call too large to review, or no reviewer. The prompt shows the reason and
   the command, or the file path and the start of the content.

Review has a deadline. Set `permissions.review_timeout_secs` in `config.json` to an integer from 5
to 300 (default 40). It applies to new sessions and is inherited by their children. If the review
times out, a read that Instinct already judged safe keeps its safe fallback. A change asks you.

Some things always ask you, however confident the reviewers are:

- Changes to WithNav's own configuration, credentials and keys.
- `git push` that deletes or forces, in any spelling.
- Shell syntax that hides what a command does, such as `cd` before the real command, a relative
  `PATH` entry or redirections to expanded paths.
- Destruction, shared publication, and changes outside the project.

To turn the automatic review off and be asked about every `bash`, `write` and `edit` call, set
`permissions.automatic_review` to `false`. An invalid value fails closed.

## Answering a prompt

| Answer | Meaning |
|---|---|
| **Allow once** | Run this call. |
| **Deny** | Do not run it. You can add a message that the model sees. |
| **Allow for this folder / site** | Only offered for `fetch`. The approval is saved per workspace in `~/.local/share/withnav/grants.json` and applies to later sessions there. Delete the workspace's entry in that file to revoke it. |
| **Allow WithNav config for this session** | Offered for edits to WithNav's own config. Reviews still run, and credentials still ask. |
| **Allow this skill's scripts on this machine** | Offered for a user skill's script. It saves the skill's location and each script's SHA-256 in `skill-grants.json` in the config directory. Reviews still run, and changes ask again. |

In the terminal these are the keys `y`, `a` and `n`. In VS Code they are buttons on a card that
shows the full reason and command.

For `bash`, `write` and `edit` there is no reusable "allow folder": every call is reviewed. Your
Allow once on an edit or write also counts for the same file until your next message; it never
covers credentials or protected configuration.

## Bypass

**Bypass** lets one session skip ordinary permission prompts.

- Turn it on with `/bypass on` (and `/bypass off`), `alt+p` in the terminal, the **WithNav: Toggle
  Bypass Permissions** command in VS Code, the **Bypass** button in the browser, or
  `nav run --yes`.
- With it on, `write`, `edit`, `bash` and `fetch` calls that would have asked run once instead,
  after the usual reviews.
- Requests for WithNav's own configuration, credentials and keys **still ask you**. `nav run
  --yes` stops with exit code 3 on one.
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

There are three levels of control, from the strongest: **normal** (reviews and prompts),
**bypass** (ordinary prompts skipped for one session; protected requests still ask) and **yolo**
(nothing is checked).

- A tool call runs without an approval prompt, without the Instinct and Metacognition review,
  without the plan role's check or an agent's write scope, without any `tool.pre_call` flow, and
  without the protected-configuration, credentials and scratch-folder rules. File tools may reach
  outside the workspace, and `grep` and `glob` may start in a credentials folder (the credential
  files in it are still left out of the results). No review request is sent at all.
- Each call records `approved_by: yolo` in its `tool.pre_call` and `tool.post_call` events.
- Still enforced: which tools a session or agent has, malformed or missing arguments, and
  cancellation. A refused call still says `approved_by: yolo`.
- `YOLO` shows in the terminal header and sidebar, as a badge in the browser and in VS Code, and
  `nav doctor` prints `permissions: yolo (no permission control)`.
- Changing the setting applies to new sessions, not to open ones.

## The toolchain trust rule

A program you installed yourself, such as `cargo` from rustup, is trusted like a system one. So
`cargo test` does not go to review every time.

A program qualifies when all of these hold:

- You or root own it.
- Nobody else can write it, or any directory above it.
- It is outside the workspace.

The trade-off: any program you own that meets these rules is trusted by ownership. An agent that
can write one there, with the shell or a build script, can have it run as a known tool. To limit
that:

- Edits through the `write` and `edit` tools to `~/.cargo/bin` and `~/.rustup/toolchains` (or
  `$CARGO_HOME` and `$RUSTUP_HOME`) always ask you.
- Replacing the resolved executable, or the rustup proxy, invalidates an approval for a command. A
  toolchain update behind an unchanged proxy does not.

## `plan` and the guard

For the `plan` agent, Instinct also checks that each gated call fits a planning role. A plain
`.md` write is exempt; everything else is cleared by that check or asked to you. The exemption
holds only for a regular Markdown file, reached without a symbolic link, in a folder that already
exists.

## Scratch folders

The agent may write temporary files, without asking, in two folders:

- A private one: `$XDG_RUNTIME_DIR/withnav`, or `~/.cache/withnav/scratch` if that is not usable.
- A shared one: `<temp dir>/withnav`, usually `/tmp/withnav`.

Each must be a real directory owned by you with mode 0700. A folder that is replaced, made public
or reached through a symlink loses the exemption. Change the list with
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
