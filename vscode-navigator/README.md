# Navai — VS Code extension

Pair-programming navigator client. Launches the `navai-lsp` language server over stdio,
renders its end-of-line remarks, offers inline suggestions from the reflex layer, forwards
diagnostics and user feedback, and hosts the Cognition chat view.

## What it shows

- **Remarks**: short end-of-line comments from the metacognition, with a verdict (useful, noise,
  dismissed).
- **Suggestions**: ghost text from the reflex layer, as native inline completion. Turn them off with
  `navai.reflex.enabled`.
- **Cognition chat**: a webview in the Navai activity-bar container (view id `navai.chat`, named
  "Cognition"). It talks to the session server that `navai-lsp` runs over HTTP and SSE. It streams
  the conversation, lists past conversations of the workspace to resume, and has a model picker for
  the next new conversation (an open conversation keeps its model) and a gauge of the context used.
  Navigator wakes appear in it as messages from the navigator. It sends the open file and selection
  as editor context.
- **Agent controls in chat**: permission cards show the full reason and command (or a file path
  and bounded content), with Allow once and Deny plus an optional deny message. Subagents' requests
  stay in the parent conversation. Sending while approval is pending asks for confirmation.
  Out-of-steps and watchdog pauses offer Continue; watchdog pauses also offer Stop. Subagent
  output is labelled untrusted, selected skills appear beside your messages, and tool rows have
  expandable approval notes when the server provides review evidence.

## Requirements

- A `navai-lsp` binary on `PATH`, or set `navai.serverPath` to an absolute path.
- Node.js 18+ to build.

## Build and package

```
cd vscode-navigator
npm ci
npm run compile
npm run package
code --install-extension navai-*.vsix
```

`npm run package` bundles the runtime dependency (`vscode-languageclient`) with esbuild into
`out/extension.js`, so the `.vsix` ships one self-contained file (no `node_modules` inside the
package; `.vscodeignore` excludes them). `vsce package` triggers `npm run compile` via the
`vscode:prepublish` hook.

## How it works

- Activation: `onStartupFinished`; the client uses document selector `[{ scheme: "file" }]`.
- Server notifications handled: `navigator/remark`, `navigator/clear`, `navigator/status`.
- Client notifications sent: `navigator/feedback`, `navigator/diagnostics` (debounced 500 ms
  per uri, `file` scheme only, excluding the navigator's own diagnostics), `navigator/intent`.
- Settings moved from `navigator.*` to `navai.*`; a value still set under the old `navigator.*` key is honored until a `navai.*` one is set. Configuration lives under `navai.*` settings, sent as `initializationOptions` and via
  `workspace/didChangeConfiguration`. Setting `navai.enabled` to false stops the server;
  toggling it back on restarts it (a failed start is retried on the next settings change).
- If the server binary can't start, the status bar shows `$(error) Navai` with the reason
  in the tooltip — the extension stays alive.

## Commands and keybindings

| Command | Key | Action |
| --- | --- | --- |
| `navai.markUseful` | Ctrl+Alt+Y | Rate the current remark "useful", then clear it |
| `navai.markNoise` | Ctrl+Alt+N | Rate the current remark "noise", then clear it |
| `navai.dismiss` | Ctrl+Alt+D | Send "dismissed", then clear it |
| `navai.setIntent` | click status bar | Tell the navigator your current goal |
| `navai.focusChat` | command palette ("Navai: Focus Cognition Chat") | Open and focus the Cognition chat view |

Keybindings are active only when a remark is visible (`navai.remarkVisible` context key).

## Development

- `npm run watch` — rebuild the bundle on change.
- `npm run typecheck` — `tsc --noEmit` type check only.
- `npm test` — deterministic event projection, replay and extension-host control tests.
- Output channel "Navai" shows the LSP traffic log.

## Settings

| Setting | Default | Description |
| --- | --- | --- |
| `navai.enabled` | `true` | Enable the navigator |
| `navai.serverPath` | `navai-lsp` | Server binary (PATH or absolute) |
| `navai.idleMs` | `3000` | Idle delay before the navigator may speak |
| `navai.minChangedLines` | `3` | Minimum changed lines to consider speaking |
| `navai.cooldownMs` | `20000` | Cooldown between remarks on one file |
| `navai.modelAlias` | `metacognition` | Model alias (from `models.json`) used for remarks |
| `navai.reflex.enabled` | `true` | Offer inline suggestions from the reflex layer |
