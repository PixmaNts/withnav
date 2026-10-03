# Primary agents set the role; effects are authorized separately, through resumable interactions

A session runs as one **primary agent**: `build` (implement, verify, report) or `plan` (research, write
markdown plans, delegate), or a custom one defined in a markdown file. The agent is a role: a prompt, an
optional model, a tool allow/deny map and an access level. It says what the agent is for. It does not
say what the agent may execute.

## Decision

- **Role intent and execution permission are two things.** Choosing `plan` changes what the model is
  told to do; it does not remove tools, and choosing `build` does not grant anything. Both agents keep
  the full toolset (`plan` is told to write only markdown and never change code). Hiding a tool to
  enforce a role would make the role a sandbox that it cannot be: a model that has `bash` can do anything
  the prompt forbids.
- **Effects are authorized where they happen.** Every `write`, `edit`, `bash` and `fetch` goes through a
  permission request answered by the driver (allow once, allow for a folder, deny), as a typed
  interaction that suspends the turn and resumes it: the same yield-and-resume path the `ask` tool uses.
  The approved call runs once, before another model request, and its real result enters the transcript;
  a denial enters it as a denied result. Approval text is never a tool result, and nothing the model or a
  chat message says can grant. This part of the work comes after the agents themselves.
- **Jev only evaluates the exceptions of `plan`.** The fast decision layer is asked, within a short
  deadline, whether a `write`, `edit` or `bash` of the `plan` agent conforms to its role (a write of a
  `*.md` file is exempt). Only a clear "conforms" lets the call run without the driver; anything else,
  including a timeout or no answer, becomes a permission request. It is advisory, never a block and never
  a sandbox.
- **Selection is a session property.** The session stores the primary agent (`sessions.primary_agent`,
  `NULL` meaning `build`); `/agent <id>` switches it between turns, with no pending `ask`, stored before
  it is reported, and published as a durable `session.agent_changed`. The role prompt is composed with the
  profile's instructions and is never written into the transcript, so a switch keeps the conversation and
  the model as they are. A stored agent whose definition has vanished is an error on resume, never a
  silent `build`.
- **Definitions are files.** Built-ins, then `~/.config/opencode/agents/` (read only), then
  `~/.config/navai/agents/`, then the project's `.navai/agents/`; `build` and `plan` are reserved, so a
  file cannot replace them (OpenCode's own `build.md` is a subagent and must not). Unsupported
  permission fields are reported, never applied.

## Consequences

- A role cannot be bypassed by prompt text, and a permission cannot be obtained by persuasion: the two
  are enforced in different places and fail separately.
- Until the permission gate lands, `build` and `plan` run effects without asking, as the tools always
  have. The `plan` role then rests on its prompt alone.
- The event catalog grows by one kind (`session.agent_changed`), and later by the permission events.
