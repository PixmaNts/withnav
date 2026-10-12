# WithNav — VS Code extension

[Repository](https://github.com/PixmaNts/withnav) · [Report an issue](https://github.com/PixmaNts/withnav/issues)

WithNav pair-programming companion client. Launches the `nav lsp` language server over stdio,
renders its end-of-line remarks, offers inline suggestions from the reflex layer, forwards
diagnostics and user feedback, and hosts the Cognition chat view.

## What it shows

- **Agent-first conversations:** choose a primary agent (the configured default is preselected),
  then optionally override its model. `/new [agent]` starts fresh on that agent's assignment,
  without inheriting an earlier conversation's override. `/agent` uses the selected agent's own
  assignment or saved session override. `/model` and `/variant` override only that agent in that
  conversation; the server persists and restores the choice.
- **Canonical model catalog:** searchable choices use exact `gateway/upstream_id` names, grouped
  under collapsible gateway/provider headings. Search includes collapsed groups; only the first
  50 matching choices render. Hover the chat header for lower layers' configured models, variants
  and disabled reasons, and cognition's effective model/source. “Usable” means valid configuration,
  not an online-status check. Assignments are read-only; config editing is out of scope. Remarks
  use the configured metacognition layer; the old model-selection setting has been removed.

- **Remarks**: short end-of-line comments from the metacognition, with a verdict (useful, noise,
  dismissed).
- **Suggestions**: ghost text from the reflex layer, as native inline completion. Turn them off with
  `withnav.reflex.enabled`.
- **Cognition chat**: a webview in the WithNav activity-bar container (view id `withnav.chat`, named
  "Nav"). It talks to the session server that `nav lsp` runs over HTTP and SSE. It streams
  the conversation, lists past conversations of the workspace to resume, and has a model picker for
  new and open conversations (switches during a running turn apply after it finishes) and a gauge of the next request’s estimated context against the model’s window; hover or focus it for the breakdown, input budget and last request’s count.
  Type `/` for a keyboard-accessible command palette: `/model`, `/variant`, `/agent`, `/compact`,
  `/context`, `/skills`, `/stop`, `/new` and `/help`. `/skills` fills a selected skill into your
  next request; command outcomes appear as notices. Commands never attach editor context;
  `//text` sends the literal `/text`. Unknown or refused commands keep your draft.
  Nav wakes appear in it as messages from Nav. It sends the open file and selection
  as editor context.
- **Subagents in chat:** see delegated goals and live states beneath the conversation. Open a
  subagent's read-only transcript in a separate editor tab without leaving the parent chat. Stop or continue a parked subagent
  with confirmation; permission requests are still reviewed in the parent conversation.
- **Agent controls in chat**: permission cards show the full reason and command (or a file path
  and bounded content), with Allow once and Deny plus an optional deny message. Subagents' requests
  stay in the parent conversation. Sending while approval is pending asks for confirmation.
  Out-of-steps and watchdog pauses offer Continue; watchdog pauses also offer Stop. Subagent
  output is labelled untrusted, selected skills appear beside your messages, and tool rows have
  expandable approval notes when the server provides review evidence.

## Requirements

- **Linux x64 and arm64:** install `withnav-linux-x64-<version>.vsix` or
  `withnav-linux-arm64-<version>.vsix` from the GitHub Release. It bundles a static `nav`; nothing
  else to install. A `withnav.serverPath` you set wins over the bundled binary.
- **Universal package** (`withnav-<version>.vsix`, no binary; macOS, Nix, development): put a `nav`
  binary on `PATH`, or set `withnav.serverPath` to its absolute path. The extension appends `lsp`;
  do not put the subcommand in the setting. Old `withnav-lsp` and `navai-lsp` paths use `nav` in
  the same directory, with a one-time notice to update the setting.
- **Windows x64:** native packaging is prepared as `withnav-win32-x64-<version>.vsix`, with
  `bin/nav.exe`. Until a release includes it and the Rust port is validated, use WSL or a Remote
  window with the Linux package. Explicit paths with spaces need no shell quotes; the LSP starts
  directly. See the [Windows install/support notes](https://withnav.dev/docs/install.html#native-windows-x64)
  for runtime prerequisites and shell limitations.
- The "WithNav" output channel starts with the line `Using nav binary (...)`: setting, bundled or
  path.
- Node.js 18+ to build.

## Build and package

```
cd vscode-withnav
npm ci
npm run compile
npm run package
code --install-extension withnav-*.vsix
```

`npm run package` bundles the runtime dependency (`vscode-languageclient`) with esbuild into
`out/extension.js`, so the `.vsix` ships one self-contained file (no `node_modules` inside the
package; `.vscodeignore` excludes them). `vsce package` triggers `npm run compile` via the
`vscode:prepublish` hook.

`scripts/package-vsix.sh <linux-x64|linux-arm64|win32-x64|universal>` (from the repository root)
writes the release packages into `dist/`. Linux variants build a static `nav` with `cargo-zigbuild`
and `zig` (or reuse `NAV_BINARY`) and ship it as `bin/nav`. Windows uses a native MSVC build and
ships `bin/nav.exe`; under Git Bash, pass the prebuilt executable via `NAV_BINARY`. Universal
packages contain neither executable; stale bundled binaries are cleared for every variant.

## How it works

- Activation: `onStartupFinished`; the client uses document selector `[{ scheme: "file" }]`.
- Server notifications handled: `withnav/remark`, `withnav/clear`, `withnav/status`.
- Client notifications sent: `withnav/feedback`, `withnav/diagnostics` (debounced 500 ms
  per uri, `file` scheme only, excluding Nav's own diagnostics), `withnav/intent`.
- Settings use `withnav.*`; explicit `navai.*` values are honored when the new key is unset, with a one-time migration notice. Configuration lives under `withnav.*` settings, sent as `initializationOptions` and via
  `workspace/didChangeConfiguration`. Setting `withnav.enabled` to false stops the server;
  toggling it back on restarts it (a failed start is retried on the next settings change).
- If the server binary can't start, the status bar shows `$(error) WithNav` with the reason
  in the tooltip — the extension stays alive.

## Commands and keybindings

| Command | Key | Action |
| --- | --- | --- |
| `withnav.markUseful` | Ctrl+Alt+Y | Rate the current remark "useful", then clear it |
| `withnav.markNoise` | Ctrl+Alt+N | Rate the current remark "noise", then clear it |
| `withnav.dismiss` | Ctrl+Alt+D | Send "dismissed", then clear it |
| `withnav.setIntent` | click status bar | Tell Nav your current goal |
| `withnav.focusChat` | command palette ("WithNav: Focus Cognition Chat") | Open and focus the Cognition chat view |

Keybindings are active only when a remark is visible (`withnav.remarkVisible` context key).

## Development

- `npm run watch` — rebuild the bundle on change.
- `npm run typecheck` — `tsc --noEmit` type check only.
- `npm test` — deterministic event projection, replay and extension-host control tests.
- Output channel "WithNav" shows the LSP traffic log.

## Settings

| Setting | Default | Description |
| --- | --- | --- |
| `withnav.enabled` | `true` | Enable Nav |
| `withnav.serverPath` | `nav` | Binary (PATH or absolute), launched with `lsp`; when unset, the bundled `bin/nav` is used if present |
| `withnav.idleMs` | `3000` | Idle delay before Nav may speak |
| `withnav.minChangedLines` | `3` | Minimum changed lines to consider speaking |
| `withnav.cooldownMs` | `20000` | Cooldown between remarks on one file |
| `withnav.reflex.enabled` | `true` | Offer inline suggestions from the reflex layer |

Windows artifacts are a compile/packaging preview, not a supported runtime. Private persistence (credentials, settings, grants, session ownership, flows and scratch), native no-follow file tools, shell execution even with Git Bash, and browser auto-open are disabled. Trusted Git Bash support is upcoming.
