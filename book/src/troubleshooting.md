# Troubleshooting

Start with `nav doctor`. It reads your configuration, says what is wrong and suggests a fix.

## Check your setup with `nav doctor`

```sh
nav doctor --workspace /path/to/project
nav doctor --offline --json
nav doctor --layer instinct
```

| Flag | Meaning |
|---|---|
| `--offline` | Make **zero network calls**. Read only local config and cache, and mark the live checks as skipped. A variant offer that cannot be verified is a warning, not a false success. |
| `--json` | Print one JSON report on standard output: `schema_version`, `checks` and `summary`. |
| `--workspace <dir>` | Check this project. Default: the current directory. |
| `--layer <reflex\|instinct\|metacognition>` | Limit the layer and live checks to one layer. Shared config, agent, compaction and workspace checks still run. |

> **Without `--offline`, doctor sends small synthetic requests to your configured providers and
> may incur charges.** It makes two Reflex FIM calls (plain and with bounded context), one
> Instinct call (an editor decision), and one tool-free Metacognition chat call. Instinct keeps its 2-second deadline. FIM is bounded at 10
> seconds and chat at 20 seconds. No project source, session or tool is executed or sent. Provider
> catalogue discovery may also run to verify offers.

Doctor resolves configuration exactly as WithNav does: the same platform config directory,
workspace catalogue, agent precedence, default fallback and layer assignments. It also reports the
compaction writer, the trusted system `bash`, the `nav lsp` version and, when it can detect it,
the VS Code server path (`nav` by default). Missing optional features and failed detection are
warnings.

Doctor changes nothing. It does not migrate, fix, validate, reload or write anything, including
metadata caches. For each global and workspace flow it reports whether its current content is
validated; a flow that is not validated does not load.

### Read the output

```text
✓ ok assignment.layer.instinct: gateway/typesafe/jev variant=off
✓ ok live.instinct.editor: valid answer (145 ms)
⚠ warning flows.workspace.guard: current flow version not validated; not loaded
✕ failed live.metacognition.chat: HTTP 401 rejected; Check provider authentication, model id and API support.
```

Each line is `status id: value`, then a hint when there is one. Statuses are `ok`, `warning` and
`failed`.

In `--json`, each check has an id, a status, the measured value, an optional sanitised error
category, a hint and the elapsed milliseconds. Neither mode prints response bodies,
credential-bearing URLs, keys or environment values. Attach `nav doctor --offline --json` to a bug
report when configuration may be involved.

**Exit code:** `0` means no failed check (warnings are allowed). `1` means at least one failure.
`2` means invalid flags or layer names.

## Common problems

### `Error: cannot load startup models.json`

WithNav found no catalogue, or it is invalid. It looks for `models.json` in this order: the path
you gave with `--models`, the workspace, then `~/.config/withnav/`. Create one; see the
[Quick start](quick-start.md). A catalogue key that does not equal `<provider>/<model>`, a gateway
that is not declared, or a duplicate key is rejected.

### `default_agent '<name>' is unknown` or `has no usable model`

A bad `default_agent`, or a default agent with no usable chat model, stops server and
language-server startup. The error names the field, the model and the valid agents. Check:

- `default_agent` names a primary agent (`build` or `plan`, or a custom primary one).
- `agents.<id>.model` is a name that exists in `models.json`, in the form `<gateway>/<id>`.
- The gateway has a key: `nav auth list`, or its `api_key_env` variable is exported. For example,
  `model 'openai/gpt-4.1' authentication unavailable for provider 'openai'`.
- The model has a context window, from `models.json` or from the gateway.

See [Agents](agents.md#assign-models-to-agents).

### A layer does nothing

A missing, unknown or API-incompatible assignment turns that layer off, with a diagnostic. Look
for `assignment.layer.<name>` in `nav doctor`. The model must use the right `api`: `fim` for Reflex,
`systemone` for Instinct and `chat` for Metacognition. See [Layers](layers.md).

No other model is chosen for you. A layer that is off never falls back silently:

- Without Reflex, there are no suggestions.
- Without Instinct, the permission flow cannot judge: your rules apply, and a call with no rule
  asks you.
- Without Metacognition, there are no remarks.

An unknown model for a non-default agent falls back to the default agent's model, with a warning
that names the agent and the requested model.

### `✕ failed auth.provider.<name>`

The gateway has no usable key. Run `nav auth set <provider>`, or export the variable named in
`api_key_env`. A stored key wins over the environment variable.

### Permission prompts for a command you trust

Answer **Always** to save the allow rule shown on the button. Later calls still go through the
fixed floor, the repeated-call check and the permission flow's danger check.
You can also add a rule yourself, for example
`nav permissions add bash 'cargo test *' allow`. See [Permissions](permissions.md). Do not use
bypass to avoid prompts you do not understand.

### `nav run` exits with code 3

A tool call needs approval and nobody can give it. `nav run` never approves on its own. Rerun
with `--yes` only if you accept that ordinary calls run without asking; `deny` rules still apply
and protected requests still stop it.

### VS Code shows `$(error) WithNav` in the status bar

The extension could not start the server. Hover the status item for the reason. Open the
**WithNav** output channel: its first line, `Using nav binary (...)`, shows which binary it tried.
Then check `withnav.serverPath` and see [Install](install.md). The extension stays alive and
retries on the next settings change.

### Old conversations cannot be resumed

A conversation whose model identity cannot be proved stays readable as a read-only archive, with
the reason resume is disabled. It is never silently attached to a different model. Session ids are
kept.

## Older installs

WithNav was called Navai before 0.7. The old names still work in 0.7 and print a warning. Removal
is planned for 0.8; see
[ADR 0014](https://github.com/PixmaNts/withnav/blob/develop/docs/adr/0014-rebrand-withnav.md).
Switch to the new names:

| Old | New |
|---|---|
| `navai` command | `nav` |
| `.navai/`, `.navigator/` in a project | `.withnav/`. When `.withnav/` is missing, the old folders are read, with a hint. |
| `NAVAI_*` environment variables | The same name with `WITHNAV_`. The old one is read when the new one is unset. |
| `navai.*` VS Code settings | `withnav.*`. An old value is honoured when the new key is unset, with a notice. |
| `withnav-lsp` or `navai-lsp` in `withnav.serverPath` | `nav`. The extension uses `nav` in the same directory and asks you to update the setting. |
| Agent ids `explore`, `librarian`, `review`, `oracle`, `explorer` | `vigie`, `vigie`, `lux`, `lux`, `mousse` |
| `~/.config/navai` | `~/.config/withnav`. `nav doctor` warns about an old directory and tells you to move it by hand; it never moves files. |

`nav doctor` lists every old name it finds.
