# Quick start

This page takes you from a fresh install to a first working session. It uses one OpenAI-compatible
gateway and one model. You can add more later.

## 1. Describe your gateway and model

WithNav reads its **catalogue** from `~/.config/withnav/models.json`. It lists the gateways you
can call and the models behind them. Create the file:

```json
{
  "providers": {
    "openai": {
      "base_url": "https://api.openai.com/v1",
      "api_key_env": "OPENAI_API_KEY"
    }
  },
  "models": {
    "openai/gpt-4.1": {
      "provider": "openai",
      "model": "gpt-4.1",
      "api": "chat",
      "context_window": 1000000,
      "supports_tools": true
    }
  }
}
```

Change the gateway, model and `context_window` to match your account. Every model name is
`<gateway>/<upstream id>`; see [Gateways and models](models.md).

## 2. Give the default agent a model

`~/.config/withnav/config.json` decides which model does which job:

```json
{
  "default_agent": "build",
  "agents": {
    "build": { "model": "openai/gpt-4.1" }
  }
}
```

This is the minimum. The `build` agent can now work. The three lower layers stay off until you
assign them a model; see [Layers](layers.md).

## 3. Store your API key

Either export the variable named in `api_key_env`:

```sh
export OPENAI_API_KEY=...
```

or store the key with WithNav:

```sh
nav auth set openai
```

`nav auth set` reads one line from standard input and does not hide it as you type. To keep it off
the screen, pipe it in, for example `printf '%s\n' "$OPENAI_API_KEY" | nav auth set openai`. The
key goes to `~/.config/withnav/auth.json` with mode 0600. A stored key wins over the environment
variable.

## 4. Check the setup

```sh
nav doctor --offline
```

`--offline` makes no network call. With the files above you should see warnings that the Reflex,
Instinct and Metacognition layers are not assigned. That is expected. There must be no `✕ failed`
line. See [Troubleshooting](troubleshooting.md) for how to read the output.

## 5. Start

In a project folder, pick one:

- **VS Code:** open the folder and click the WithNav icon in the activity bar to open the Nav chat.
  See [VS Code extension](vscode.md).
- **Terminal:** run `nav` (or `nav tui`).
- **Browser:** run `nav web`.
- **One task:** `nav run -p "explain what this repository does"`.

`write`, `edit`, `bash` and `fetch` ask before they act, so a first session is safe to try. See
[Permissions](permissions.md).

## Next steps

- Add models for the lower layers: [Layers](layers.md).
- Start from a full example instead: [Gateways and models](models.md#example-setup).
- Already use OpenCode? [Import its models](catalogue-import.md).
