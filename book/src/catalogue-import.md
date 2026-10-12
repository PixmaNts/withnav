# Import a catalogue from OpenCode

If you already use OpenCode, `scripts/import-opencode-models` builds `models.json` and `auth.json`
from its config. Run it from a checkout of the repository.

```sh
scripts/import-opencode-models [--dry-run] [--config-dir DIR] [--opencode FILE] [--with-windows]
scripts/import-opencode-models --self-test
```

| Flag | Meaning |
|---|---|
| `--dry-run` | Print a summary and write nothing. Try this first. |
| `--config-dir DIR` | Where to write the WithNav files. |
| `--opencode FILE` | The OpenCode config to read. Default `~/.config/opencode/opencode.jsonc`. |
| `--with-windows` | Copy context windows from OpenCode instead of letting WithNav read them from the gateway. |
| `--self-test` | Run the script's own tests and exit. |

## What it does

- It imports **OpenAI-compatible providers only**. Any other provider is skipped with a warning.
- It writes qualified model names (`<gateway>/<upstream id>`) and sets
  `"catalog_format": "omniroute"` on the `omniroute` provider. The same upstream id from two
  gateways stays distinct.
- It never writes layer or agent choices to `config.json`. You make those yourself; see
  [Layers](layers.md) and [Agents](agents.md).
- Combo models get `low`, `medium` and `high` variants in the request body. Variants you declared
  by hand are kept when you run it again.
- It leaves context windows out, so WithNav reads them from the provider. `--with-windows` copies
  them instead.

## API keys

- A literal `apiKey` goes to `auth.json` (mode 0600).
- An `{env:NAME}` key becomes `"api_key_env": "NAME"` in `models.json`. Any key stored in
  `auth.json` for that provider is **removed**, because WithNav prefers a stored key over the
  environment and it would hide `{env:NAME}`. Other entries are kept, and the script says what it
  removed.
- Other references (such as `{file:...}`) are refused. So is a `baseURL` that carries credentials.

## Safety

Nothing is written until the whole config checks out. Each file is replaced atomically, but the two
are not replaced together. If the second replacement fails, the first file is already updated, and
the script tells you which one was and which was not.
