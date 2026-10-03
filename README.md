# Navai

A pair-programming system in Rust. The human keeps the keyboard and stays the author of the code.
Several models of different speeds watch, complete, judge and discuss. Under them runs a small,
owned coding-agent core with a terminal client and a local server.

> **Status:** v0.2.0 is released. `develop` holds more: a terminal client, build and plan agents,
> permissions, skills, rules, model variants, a context budget and manual compaction. See
> [`CHANGELOG.md`](CHANGELOG.md). [`ARCHITECTURE.md`](ARCHITECTURE.md) maps the code and
> [`CONTEXT.md`](CONTEXT.md) defines the vocabulary.

## What exists today

**The agent core** (`navai`):

- `navai tui`: a terminal chat client. Several conversations in tabs (up to nine, `alt+1` to
  `alt+9`), a header with the workspace, git branch, agent and model, a right-hand column with the
  context breakdown, skills and git changes (`alt+r`), a key bar, a command palette (`ctrl+p` or
  `/`), a model picker with variants, a session list to resume, and four themes (Ayu Dark and
  Light, Nord Dark and Light; `/theme`). It starts a private server, or connects to one with
  `--url`.
- `navai run -p "<task>"`: one task in a terminal. It never approves a tool call by itself.
- `navai serve`: the session server alone (HTTP and SSE, loopback by default).
- `navai auth set|list|remove <provider>`: API keys.
- **Agents.** `build` (the default: implement, verify, report) and `plan` (research, write markdown
  plans). The driver switches between turns. Agents can also be defined in Markdown files.
- **Permissions.** `write`, `edit`, `bash` and `fetch` ask first: allow once, allow the folder, or
  deny with a message. For `plan`, a second model (Jev) checks each gated call (`write`, `edit`, `bash`,
  `fetch`) except a plain `.md` write, and what it does not clear is asked to the driver. This is a
  guard against drift, not a sandbox.
- **Tools.** `read`, `write`, `edit`, `bash`, `grep`, `glob`, `fetch`, `ask`, and `skill` when
  skills are found.
- **Rules and skills.** `AGENTS.md`, `CLAUDE.md` and `rules.md` files, and `SKILL.md` skills, are
  read when a session starts.
- **Models.** Aliases in `models.json`. The context window is read from the provider when you do
  not set it. A model can offer variants (for example reasoning effort), chosen with `/variant`.
- **Context.** An estimate of the next request and how full the window is. `/compact` summarizes
  the older part of a long conversation.
- **Session commands.** `/help`, `/model [alias]` (switches the model of the conversation between
  turns, refused if the history does not fit the new window), `/new [alias]`, `/stop`, `/context`,
  `/variant`, `/compact`, `/agent`. In the terminal client, `/model <alias>` and `/new <alias>` open a
  new tab on that model; the model picker does the same.

**The navigator** (`navai-lsp` and the VS Code extension in `vscode-navigator/`): a Language
Server that watches your edits. A reflex layer offers ghost-text suggestions, an instinct layer
decides when to speak, a metacognition layer writes short remarks beside your code, and a
cognition chat discusses the work with you. See [`vscode-navigator/README.md`](vscode-navigator/README.md).

### Command line

| Command | Main flags |
|---|---|
| `navai run` | `-m/--model <alias>`, `-p/--prompt <text>` (a `/command` runs it), `--workspace <dir>`, `--models <file>`, `--agent <build\|plan>`, `--variant <name\|off>`, `--show-thinking`, `--yes` (approve every request once; unsafe) |
| `navai tui` | `--workspace <dir>`, `--model <alias>` or `--session <id>` (resume), `--models <file>` or `--url <server>` (token in `NAVAI_SERVER_TOKEN`) |
| `navai serve` | `--bind <addr>` (default `127.0.0.1:7310`), `--allow-non-loopback`, `--models <file>` |
| `navai auth` | `set <provider>`, `list`, `remove <provider>` |

`navai run` exit codes: `0` done, `1` error, `2` the agent asked a question, `3` a tool call needs
approval (`permission_required`), `130` interrupted.

## Install

```bash
cargo build --release --bins
install -m755 target/release/navai target/release/navai-lsp ~/.local/bin/
(cd vscode-navigator && npm ci && npm run package)
code --install-extension vscode-navigator/navai-0.2.0.vsix
```

## Configuration and data

| Where | What |
|---|---|
| `~/.config/navai/models.json` | model aliases (`reflex`, `instinct`, `metacognition`, `cognition` and your own); a `models.json` in the workspace or `--models` wins |
| `~/.config/navai/auth.json` | API keys, mode 0600; write them with `navai auth set <provider>` |
| `~/.config/navai/config.json` | client settings, such as `tui.theme` |
| `~/.config/navai/agents/*.md`, `<project>/.navai/agents/*.md` | agent definitions; `~/.config/opencode/agents/` is also read |
| `~/.config/navai/navigator/pipeline.toml`, `<project>/.navigator/` | the navigator's decision pipeline and intent record |
| `~/.agents/AGENTS.md`, `~/.agents/rules.md`, `~/.agents/skills/` | user rules and skills (`~/.claude/` is read too); projects add `AGENTS.md`, `rules.md` and `.agents/skills` (also `.claude/` and `.opencode/`) |
| `~/.local/share/navai/` | conversation history `navai.db`, decision log `navai.jsonl`, `tui.log`, `model-catalog/` |

To build `models.json` and `auth.json` from an OpenCode config, run
`scripts/import-opencode-models [--dry-run] [--config-dir DIR] [--opencode FILE] [--with-windows]`
(`--self-test` runs its built-in tests). It imports OpenAI-compatible providers only, keeps the first
of two providers that share a model id, and writes one alias per model, the role aliases, and
`"catalog_format": "omniroute"` on the omniroute provider. Context windows are left out so Navai
reads them from the provider; `--with-windows` copies them from OpenCode instead. A literal `apiKey`
goes to `auth.json` (mode 0600); an `{env:NAME}` key becomes `api_key_env` in `models.json`; other
references and a `baseURL` that carries credentials are refused. Nothing is written until the whole
config checks out, and both files are replaced atomically.

Coming from `rust-agent`? The first start of `navai` or `navai-lsp` moves your data and config from
`~/.local/share/navigator/`, `~/.local/share/rust-agent/` and `~/.config/rust-agent/` to the new
dirs; nothing is overwritten. Remove the old `rust-agent` binaries and the old `Navigator`
extension (`rust-agent.navigator`) yourself.

## Direction (not built)

The first design ([`docs/SPEC.md`](docs/SPEC.md)) aimed higher: one event bus and actor model under
everything, with external-process plugins over JSON-RPC, MCP tools, non-blocking subagents, a
meta-loop co-agent, hot reload and a memory integration. **None of that is built.** The bus, the
server and the clients are. Subagents are the next large piece; `explore` and `review` are defined
but cannot be started yet. Automatic compaction is also not built: `/compact` is manual. `docs/SPEC.md` says, section by section, what is built.

## Contributing

The repository is on GitHub: <https://github.com/PixmaNts/Navai>. `main` holds releases only.
Work branches off `develop` as `feat/<name>` or `fix/<name>` and comes back through a pull request
into `develop`. A release merges `develop` into `main` and is tagged `vX.Y.Z`. Details in
[`AGENTS.md`](AGENTS.md).

## License

The crates declare `MIT OR Apache-2.0` (`Cargo.toml`). No `LICENSE` file is in the repository yet.
