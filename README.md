# WithNav

**You set the course. Nav lights the way.** <https://withnav.dev>

A pair-programming system in Rust. The human keeps the keyboard and stays the author of the code.
Several models of different speeds watch, complete, judge and discuss. Under them runs a small,
owned coding-agent core with a terminal client and a local server.

> **Status:** this checkout includes the changes prepared for v0.5.0: a terminal client, agents,
> permissions, skills, rules, model assignments, a context budget and compaction. See
> [`CHANGELOG.md`](CHANGELOG.md). [`ARCHITECTURE.md`](ARCHITECTURE.md) maps the code and
> [`CONTEXT.md`](CONTEXT.md) defines the vocabulary.

> **Breaking change for 0.5:** model names are gateway-qualified and assignments are separate
> from the catalogue ([ADR 0011](docs/adr/0011-layers-models-and-agent-overrides.md)). The guide
> describes this checkout, not your installed binaries. Existing configuration needs manual migration.

## What exists today

- **InstinctFlow Studio.** Draw, test, save and reload flows: [Captain's guide](docs/instinct-flow.md).

**The agent core** (`nav`):

- `nav tui`: a terminal chat client. Several conversations in tabs (up to nine, `alt+1` to
  `alt+9`), a header with the workspace, git branch, agent and model, a right-hand column with the
  context breakdown, skills and git changes (`alt+r`), a key bar, a command palette (`ctrl+p` or
   `/`), a model picker with variants, a session list to resume, and eight themes (Ayu, Nord,
   Tide and Moss, dark and light; `/theme`). Tide Dark is the default. It starts a private server, or connects to one with
  `--url`.
- `nav run -p "<task>"`: one task in a terminal. It never approves a tool call by itself, unless you pass `--yes`.
- `nav serve`: the session server alone (HTTP and SSE, loopback by default).
- `nav auth set|list|remove <provider>`: API keys.
- **Agents.** `build` (the default: implement, verify, report) and `plan` (research, write markdown
  plans). The Captain switches between turns. Agents can also be defined in Markdown files.
- **Permissions.** `write`, `edit`, `bash` and `fetch` ask first: allow once, allow the folder, or
  deny with a message. For `plan`, a second model (Instinct) checks each gated call (`write`, `edit`, `bash`,
  `fetch`) except a plain `.md` write, and what it does not clear is asked to the Captain. This is a
  guard against drift, not a sandbox.
- **Tools.** `read`, `write`, `edit`, `bash`, `grep`, `glob`, `fetch`, `ask`, and `skill` when
  skills are found.
- **Rules and skills.** `AGENTS.md`, `CLAUDE.md` and `rules.md` files, and `SKILL.md` skills, are
  read when a session starts.
- **Models.** Model access, provider-reported context windows and request variants (for example
  reasoning effort). The catalogue and assignment rules are described below.
- **Context.** An estimate of the next request and how full the window is. `/compact` summarizes
  the older part of a long conversation.
- **Session commands.** `/help`, `/stop`, `/context`, `/compact`, and selection commands.
  `/model`, `/variant`, `/agent` and `/new [agent]` are explained below.

**The navigator** (`nav lsp` and the VS Code extension in `vscode-withnav/`): a Language
Server that watches your edits. A reflex layer offers ghost-text suggestions, an instinct layer
decides when to speak, a metacognition layer writes short remarks beside your code, and a
cognition chat discusses the work with you. See [`vscode-withnav/README.md`](vscode-withnav/README.md).

### Command line

| Command | Main flags |
|---|---|
| `nav run` | `-m/--model <gateway/id>`, `-p/--prompt <text>` (a `/command` runs it), `--workspace <dir>`, `--models <file>`, `--agent <agent>`, `--variant <name\|off>`, `--show-thinking`, `--yes` (turn on bypass for the run: ordinary permission prompts run once, protected ones still stop it with exit 3; unsafe) |
| `nav tui` | `--workspace <dir>`, `--model <gateway/id>` or `--session <id>` (resume), `--models <file>` or `--url <server>` (token in `WITHNAV_SERVER_TOKEN`) |
| `nav serve` | `--bind <addr>` (default `127.0.0.1:7310`), `--allow-non-loopback`, `--models <file>` |
| `nav auth` | `set <provider>`, `list`, `remove <provider>` |

`nav run` exit codes: `0` done, `1` error, `2` the agent asked a question, `3` a tool call needs
approval (`permission_required`), `130` interrupted.

## Install

```bash
cargo build --release --bins
install -m755 target/release/nav ~/.local/bin/
(cd vscode-withnav && npm ci && npm run package)
code --install-extension vscode-withnav/withnav-*.vsix
```

## Configuration and data

`models.json` is a **catalogue**, not a list of choices. Each model name is
`<gateway>/<upstream_id>`: in `omniroute/combo/gpt-6-astra`, `omniroute` names the configured
endpoint and `combo/gpt-6-astra` is sent to it verbatim. `config.json` makes the **assignments**.
The old alias entries `default`, `fast` and `cognition` are removed, not compatibility names.

| Where | What |
|---|---|
| `~/.config/withnav/models.json` | catalogue: gateways, qualified model names, API kinds, capabilities, limits and variants; a workspace `models.json` or `--models` wins |
| `~/.config/withnav/auth.json` | API keys, mode 0600; write them with `nav auth set <provider>` |
| `~/.config/withnav/config.json` | `layers`, `agents`, `default_agent`, and client settings such as `tui.theme` |
| `~/.config/withnav/agents/*.md`, `<project>/.withnav/agents/*.md` | agent definitions; `~/.config/opencode/agents/` is also read |
| `~/.config/withnav/flows/`, `<project>/.withnav/flows/` | editor flows; historical `pipeline.toml` files are converted once, originals retained |
| `<project>/.withnav/intent.md` | the navigator's intent record |
| `~/.agents/AGENTS.md`, `~/.agents/rules.md`, `~/.agents/skills/` | user rules and skills (`~/.claude/` is read too); projects add `AGENTS.md`, `rules.md` and `.agents/skills` (also `.claude/` and `.opencode/`) |
| `~/.local/share/withnav/` | conversation history `withnav.db`, decision log `withnav.jsonl`, `tui.log`, `model-catalog/` |

### Example setup

Start from [`models.example.json`](models.example.json) and
[`config.example.json`](config.example.json), adapting endpoints and assignments before copying
them to `~/.config/withnav/`. No real keys are included. Use `nav auth set omniroute` and
`nav auth set inception`, or the example's `OMNIROUTE_API_KEY` and `INCEPTION_API_KEY`
environment variables. A stored key takes precedence over the environment.

The example leaves chat limits for provider discovery rather than guessing them: a usable chat
model needs a resolvable context window. Reflex FIM and Instinct System One retain fixed sample
windows and output caps; verify those values against your deployment. Combo variants declare
`low`, `medium`, `high` as request-body `reasoning_effort`, never a `-high` model-name suffix.

`build` is the default agent when `default_agent` is absent. Its assigned chat model must be
usable. `plan`, `vigie` and `lux` assignments are optional: an unassigned primary agent
uses the default agent's model; an unassigned child uses its parent's effective model. To use
the Crew, choose `vigie` (read-only scouting), `lux` (expert advice), or `mousse`
(small local edits after approval). `cartographe` and `sondeur` are available on demand;
Cartographe writes Mermaid charts to `.withnav/charts/`. `config.json`
assignments win over `model:` in a definition's header; adding an assignment does not itself
create an agent definition.

### Layers, agents and editor context

| Layer | Assignment | API |
|---|---|---|
| Reflex | one model, editor-only inline completion | `fim` (Mercury in the example) |
| Instinct | one model, typed decisions and supervision | `systemone` |
| Metacognition | one model, remarks and review; prefers `low` when offered, otherwise model default | `chat` |
| Cognition | agents with independent roles and model choices, not a singleton model entry | `chat` agent loop |

An **editor session** adds the pair-programmer prompt, `intent` tool and navigator conversation
feed to its chosen agent. It starts on the same default agent as other sessions; editor context
does not select a model. Children do not inherit the editor attachment.

- `/new [agent]` starts fresh on that agent's assignment, not the previous session's overrides.
- `/model <gateway/id>` switches the current agent's model in the same session at a safe boundary
  (after the turn when busy), taking the new model's default variant. A pending request must not
  apply to a different agent. The history must fit the destination budget, with compaction when possible.
- `/variant <name|off|inherit>` sets a named variant, explicitly disables variants, or removes the
  variant override to return to the assignment/model default. Bare `/model` and `/variant` report
  the effective choice, its source and available choices.
- `/agent <id>` uses that agent's assignment or its saved override. `/model` and `/variant` choices
  persist per **(session, agent)**: switch back or reopen to restore them. They never mutate global
  `config.json`. A child with its own model uses that assignment; an unassigned child inherits the
  parent's **effective** model and variant, including local overrides.

The clients choose an agent first, with an optional local model override. Model pickers group
by gateway, then provider within it (`omniroute` → `cc`, `combo`, …); groups collapse but search
includes hidden groups. The TUI right panel and VS Code header tooltip show lower-layer choices
and the current agent's model, marked when overridden. The historical `navai.modelAlias` setting remains removed.

### Startup errors and migration

The `navai` command is gone: run `nav`. New project files go in
`.withnav/`; when absent, `.navai/` then `.navigator/` are read with a migration hint.
Old config/data files move to the WithNav directories on first start without
overwriting new files. `NAVAI_*` variables remain fallbacks for `WITHNAV_*`, with
warnings. Old agent ids still resolve as deprecated aliases. See
[ADR 0014](docs/adr/0014-rebrand-withnav.md) for the full compatibility window;
removal is planned for 0.8.

A missing, unknown or API-incompatible lower-layer assignment disables that layer with a clear
diagnostic: no reflex suggestions, permission review asks the Captain when it cannot decide, and
unavailable instinct supervision uses the visible renewable 500-step fallback cap. These
fallbacks do not silently choose a different lower-layer model. An unknown nondefault agent model
falls back to the default agent's model with a warning naming the agent and requested model.
An unknown/nonprimary `default_agent`, or a default agent with no usable chat model, is fatal to
server and LSP startup: the error names the field/model and valid agents where applicable.

Configuration migration for 0.5 is **manual**; WithNav does not automatically convert old configuration
or create missing agent definitions. Historical sessions whose model identity cannot be proved stay
readable, read-only archives with resume disabled, never silently rebound to a new model. Existing
session ids are preserved.

### Catalogue import

To build `models.json` and `auth.json` from an OpenCode config, the importer entry point is
`scripts/import-opencode-models [--dry-run] [--config-dir DIR] [--opencode FILE] [--with-windows]`
(`--self-test` runs its built-in tests). It imports OpenAI-compatible providers only,
writes qualified catalogue names and `"catalog_format": "omniroute"` on the omniroute provider,
and never writes layer/agent choices to `config.json`. Gateway qualification keeps otherwise equal
upstream ids distinct. Combo entries receive `low`/`medium`/`high` body variants; manually declared
variants are preserved when merging. Context windows are left out so WithNav
reads them from the provider; `--with-windows` copies them from OpenCode instead. A literal `apiKey`
goes to `auth.json` (mode 0600). An `{env:NAME}` key becomes `api_key_env` in `models.json`, and any key
stored in `auth.json` for that provider is removed, because WithNav prefers a stored key over the
environment (other entries are kept; the script says what it removed). Other references and a
`baseURL` that carries credentials are refused. Nothing is written until the whole config checks out.
Each file is replaced atomically; the two are not replaced together. If the second replacement
fails, the first file has already been updated, and the script says which one was and which was not.

Coming from `rust-agent`? The first start of `nav` moves your data and config from
`~/.local/share/navigator/`, `~/.local/share/rust-agent/` and `~/.config/rust-agent/` to the new
dirs; nothing is overwritten. Remove the old `rust-agent` binaries and the old `Navigator`
extension (`rust-agent.navigator`) yourself.

## Direction

The first design ([`docs/SPEC.md`](docs/SPEC.md)) aimed higher: one event bus and actor model under
everything, with external-process plugins over JSON-RPC, MCP tools, non-blocking subagents, a
meta-loop co-agent, hot reload and a memory integration. That document is not a deployment
checklist. The bus, server and clients exist; background children and automatic compaction are
mapped in [`ARCHITECTURE.md`](ARCHITECTURE.md), along with the selection rules above.
Configuration editors that write `config.json` and flow configuration are planned for 0.6;
they are not part of 0.5.

## Contributing

The repository is on GitHub: <https://github.com/PixmaNts/withnav>. `main` holds releases only.
Work branches off `develop` as `feat/<name>` or `fix/<name>` and comes back through a pull request
into `develop`. A release merges `develop` into `main` and is tagged `vX.Y.Z`. Details in
[`AGENTS.md`](AGENTS.md).

## License

WithNav is dual-licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option
(`license = "MIT OR Apache-2.0"` in `Cargo.toml`).
