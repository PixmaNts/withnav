# Agents

Cognition runs **agents**. An agent is a role: a prompt, a set of tools, an access level and,
optionally, a model and a variant. The agent you talk to is the **primary agent**. Others are
**subagents** that a primary agent hands a task to.

| Agent | Kind | What it does |
|---|---|---|
| `build` | primary | The default. Implements, verifies and reports. |
| `plan` | primary | Researches and writes Markdown plans. Gated calls are checked by Instinct; see [Permissions](permissions.md). |
| `vigie` | subagent | Read-only scouting of code, documentation and the web, with sources. |
| `lux` | subagent | Read-only analysis and expert advice. |
| `mousse` | subagent | Small local edits, after your approval. |
| `cartographe` | subagent, on demand | Writes Mermaid charts to `.withnav/charts/`. |
| `sondeur` | subagent, on demand | Root-cause work. Tests and profilers run after your approval. |

You switch the primary agent between turns (`/agent <id>`, or `tab` in the terminal client). The
agent sets what the model is told to do. It does not set what it may run; [permissions](permissions.md)
do that.

## Tools

Agents work through these tools: `read`, `write`, `edit`, `bash`, `grep`, `glob`, `fetch`, `ask`
and `task` (hand work to a subagent), plus `skill` when [skills](rules-and-skills.md) are found.
`write`, `edit`, `bash` and `fetch` go through the permission gate.

## Assign models to agents

In `config.json`, under `agents`:

```json
{
  "default_agent": "build",
  "agents": {
    "build": { "model": "omniroute/cc/claude-opus-5-5" },
    "vigie": { "model": "omniroute/openai/gpt-6-luna", "variant": "medium" }
  }
}
```

The rules:

- `default_agent` is the agent a new session starts with. It is `build` when absent, and it must
  be a primary agent.
- The default agent **must** have a usable chat model. Without one, WithNav does not start.
- Other agents are optional. An unassigned primary agent (such as `plan`) uses the default agent's
  model. An unassigned subagent uses its parent's effective model and variant, including any local
  override.
- An assignment whose model is unknown or unusable falls back to the default agent's model and
  logs a warning that names the agent and the model.
- An unknown or non-primary `default_agent`, or a default agent without a usable chat model, stops
  server and language-server startup. The error names the field, the model and the valid agents.
- An assignment in `config.json` wins over a `model:` in an agent definition's header.
- Adding an assignment does not create an agent definition.
- `variant` follows the same rules as for [layers](layers.md#assign-a-model).

To use the crew, call on `vigie` (scouting), `lux` (advice) or `mousse` (small edits). The others
are available on demand.

The old agent ids `explore`, `librarian`, `review`, `oracle` and `explorer` still resolve, to
`vigie`, `vigie`, `lux`, `lux` and `mousse`, with a deprecation warning. They are planned for
removal in 0.8.

## Custom agents

Define an agent in a Markdown file in one of these folders. Later ones replace earlier ones with
the same id:

1. `~/.config/opencode/agents/*.md` (only read; shared with OpenCode)
2. `~/.config/withnav/agents/*.md`
3. `<project>/.withnav/agents/*.md`

A file is a YAML header between `---` lines, then a Markdown body that becomes the role prompt:

```markdown
---
description: Reviews diffs for missing tests
mode: subagent
model: omniroute/openai/gpt-6-astra
tools:
  write: false
  edit: false
access: read-only
---
You review a diff and list the changes that have no test.
```

| Header key | Meaning |
|---|---|
| `description` | What the agent is for. |
| `mode` | `primary`, `subagent` or `all`. `all` when absent. |
| `model` | Optional model. An assignment in `config.json` wins. |
| `tools` | A map of tool name to `true` or `false`. |
| `access` | `read-only` or `full`. `full` when absent. |

`build` and `plan` are reserved. A file with one of those names never replaces the built-in and is
reported. A malformed header rejects that file only. Other keys are kept but have no effect.

## Subagents

An agent can start a subagent with the `task` tool. A subagent is a real child session with its
own transcript. In the terminal client and in VS Code you can open a subagent's read-only
transcript while the parent chat stays open. A subagent's permission requests appear in the
**parent** conversation, and you answer them there. Its result comes back to the parent once, as
text labelled untrusted.

A session runs up to `subagents.max_concurrent` subagents at once (default 4). If a subagent runs
out of steps (50 by default), you can continue it (`alt+c` in the terminal, a button in VS Code).
