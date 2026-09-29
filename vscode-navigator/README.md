# Navigator — VS Code extension

Pair-programming navigator client. Launches the `rust-agent-lsp` language server over stdio,
renders its end-of-line remarks, forwards diagnostics and user feedback.

## Requirements

- A `rust-agent-lsp` binary on `PATH`, or set `navigator.serverPath` to an absolute path.
- Node.js 18+ to build.

## Build and package

```
cd vscode-navigator
npm ci
npm run compile
npm run package
code --install-extension navigator-*.vsix
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
- Configuration lives under `navigator.*` settings, sent as `initializationOptions` and via
  `workspace/didChangeConfiguration`. Setting `navigator.enabled` to false stops the server;
  toggling it back on restarts it (a failed start is retried on the next settings change).
- If the server binary can't start, the status bar shows `$(error) Navigator` with the reason
  in the tooltip — the extension stays alive.

## Commands and keybindings

| Command | Key | Action |
| --- | --- | --- |
| `navigator.markUseful` | Ctrl+Alt+Y | Rate the current remark "useful", then clear it |
| `navigator.markNoise` | Ctrl+Alt+N | Rate the current remark "noise", then clear it |
| `navigator.dismiss` | Ctrl+Alt+D | Send "dismissed", then clear it |
| `navigator.setIntent` | click status bar | Tell the navigator your current goal |

Keybindings are active only when a remark is visible (`navigator.remarkVisible` context key).

## Development

- `npm run watch` — rebuild the bundle on change.
- `npm run typecheck` — `tsc --noEmit` type check only.
- Output channel "Navigator" shows the LSP traffic log.

## Settings

| Setting | Default | Description |
| --- | --- | --- |
| `navigator.enabled` | `true` | Enable the navigator |
| `navigator.serverPath` | `rust-agent-lsp` | Server binary (PATH or absolute) |
| `navigator.idleMs` | `3000` | Idle delay before the navigator may speak |
| `navigator.minChangedLines` | `3` | Minimum changed lines to consider speaking |
| `navigator.cooldownMs` | `20000` | Cooldown between remarks on one file |
| `navigator.modelAlias` | `fast` | Model alias used for remarks |
