# Configuration

WithNav is configured with plain JSON files and Markdown files. Two JSON files do most of the work:

- **`models.json`** is the **catalogue**. It says which gateways exist and which models they
  serve. It is a list of what is available, not a list of choices.
- **`config.json`** makes the **assignments**. It says which model each layer and each agent
  uses, plus a few settings.

Keeping them apart lets you add a model to the catalogue without changing what WithNav uses.

| Page | What it covers |
|---|---|
| [Gateways and models](models.md) | `models.json`, API keys, a full example. |
| [Layers](layers.md) | Choosing the models for Reflex, Instinct and Metacognition. |
| [Agents](agents.md) | Choosing models for `build`, `plan` and the crew; custom agents. |
| [Editor context and session overrides](session-overrides.md) | What `/model`, `/variant`, `/agent` and `/new` do. |
| [Rules and skills](rules-and-skills.md) | `AGENTS.md`, `rules.md` and `SKILL.md` files. |
| [Import a catalogue from OpenCode](catalogue-import.md) | Build `models.json` from an OpenCode config. |

## Files and paths

On Linux, the configuration directory is `~/.config/withnav/` and the data directory is
`~/.local/share/withnav/`.

| Where | What |
|---|---|
| `~/.config/withnav/models.json` | The catalogue: gateways, qualified model names, API kinds, capabilities, limits and variants. A `models.json` in the workspace wins over it, and `--models <file>` wins over both. |
| `~/.config/withnav/auth.json` | API keys, mode 0600. Write them with `nav auth set <provider>`. |
| `~/.config/withnav/config.json` | `layers`, `agents`, `default_agent`, and other settings such as `tui.theme`. |
| `~/.config/withnav/agents/*.md`, `<project>/.withnav/agents/*.md` | Agent definitions. `~/.config/opencode/agents/` is also read. |
| `~/.config/withnav/flows/`, `<project>/.withnav/flows/` | Editor flows. See [InstinctFlow](instinctflow.md). An older `pipeline.toml` is converted once, and the original is kept. |
| `<project>/.withnav/intent.md` | The intent record: what you are trying to do. The metacognition layer compares your code with it. |
| `<project>/.withnav/charts/` | Mermaid charts written by the Cartographe agent. |
| `~/.agents/AGENTS.md`, `~/.agents/rules.md`, `~/.agents/skills/` | Your rules and skills. `~/.claude/` is read too. See [Rules and skills](rules-and-skills.md). |
| `~/.local/share/withnav/withnav.db` | Conversation history. |
| `~/.local/share/withnav/withnav.jsonl` | Decision log. |
| `~/.local/share/withnav/grants.json` | Folder approvals you saved for `fetch`. See [Permissions](permissions.md). |
| `~/.local/share/withnav/tui.log` | Log of the terminal client. |
| `~/.local/share/withnav/model-catalog/` | Cache of the model lists reported by your gateways. |

`nav doctor` prints the configuration directory it actually uses.

## Settings reference

These are the keys of `config.json`. Anything not listed is kept untouched, so several
versions and clients can share the file. The Settings screens write the same file; see
[Using Nav](using-nav.md#settings-screens).

| Key | Default | Meaning |
|---|---|---|
| `layers.reflex` | none (layer off) | Model for [Reflex](layers.md). Must use the `fim` API. |
| `layers.instinct` | none (layer off) | Model for [Instinct](layers.md). Must use the `systemone` API. |
| `layers.metacognition` | none (layer off) | Model for [Metacognition](layers.md). Must use the `chat` API. |
| `default_agent` | `build` | The primary agent a new session starts with. See [Agents](agents.md). |
| `agents.<id>.model`, `agents.<id>.variant` | none | Model and variant of an agent. See [Agents](agents.md). |
| `compaction.writer` | `metacognition` | Who writes compaction summaries: `metacognition`, or `session` for the session's own model. If Metacognition is unavailable, the session model is used. |
| `compaction.chunk_tokens` | `64000` | Size of one rolling summary chunk, capped at half of the writer's input budget. A positive integer. |
| `permissions.automatic_review` | `true` | `false` asks you for every `bash`, `write` and `edit` call. See [Permissions](permissions.md). |
| `permissions.review_timeout_secs` | `40` | Deadline of one Metacognition permission review, in seconds. An integer from 5 to 300. It applies to new sessions and is inherited by their children. |
| `permissions.scratch_dirs` | private runtime directory, then a shared temp one | Absolute paths where the agent may write temporary files without asking. `[]` turns it off. See [Permissions](permissions.md#scratch-folders). |
| `subagents.max_concurrent` | `4` | How many subagents one session may run at once. An integer from 1 to 64. |
| `subagents.watchdog_interval_secs` | `300` | How often Instinct checks that a background subagent makes progress. `0` turns it off. |
| `agent.watchdog_interval_secs` | `300` | How often Instinct checks that the main conversation makes progress. `0` turns it off. See [Long turns](permissions.md#long-turns-and-the-watchdog). |
| `tui.theme` | `tide-dark` | Terminal colour theme: `ayu-dark`, `ayu-light`, `nord-dark`, `nord-light`, `tide-dark`, `tide-light`, `moss-dark` or `moss-light`. |

Layer models apply after you restart WithNav. The other keys apply to new sessions.

An invalid value is an error, not a silent default. `nav doctor` names the key.

## Example setup

The repository holds a complete pair of examples:
[`models.example.json`](https://github.com/PixmaNts/withnav/blob/develop/models.example.json)
and [`config.example.json`](https://github.com/PixmaNts/withnav/blob/develop/config.example.json).
Adapt the endpoints and assignments, then copy them to `~/.config/withnav/`. They contain no real
keys. The notes on [Gateways and models](models.md#example-setup) explain the choices in them.
