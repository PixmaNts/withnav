# WithNav

[![CI](https://github.com/PixmaNts/withnav/actions/workflows/ci.yml/badge.svg?branch=develop)](https://github.com/PixmaNts/withnav/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/PixmaNts/withnav)](https://github.com/PixmaNts/withnav/releases)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](LICENSE-MIT)

**You set the course. Nav lights the way.** <https://withnav.dev>

## What it is

WithNav is a pair-programming system. You keep the keyboard and stay the author of the code.
Several models of different speeds watch your work, complete it, judge it and discuss it with you:

- **Reflex** completes what you type, as ghost text.
- **Instinct** makes small, fast decisions: when to speak, and whether a tool call is safe.
- **Metacognition** writes short remarks beside your code and reviews risky tool calls.
- **Cognition** is Nav, the chat you talk to, with agents that read, edit and run commands.

It is one Rust binary, `nav`, with a terminal client (`nav tui`), a browser client (`nav web`), a
language server and a VS Code extension.

## Install

Download the VSIX for your platform from the
[GitHub Releases page](https://github.com/PixmaNts/withnav/releases) and install it:

```sh
code --install-extension withnav-linux-x64-<version>.vsix
```

- `linux-x64` and `linux-arm64` packages bundle `nav`. Nothing else to install.
- The universal package has no binary. Put `nav` on your `PATH` or set `withnav.serverPath`.
- Nix: `nix build .#withnav .#vscode-withnav`, or use `overlays.default`.
- From source: `cargo build --release --bins`.
- Native Windows is not supported yet. Use WSL or a VS Code Remote window.

VS Code 1.91 or later is required. The extension is not on the Marketplace yet. See the
[install guide](https://withnav.dev/docs/install.html) for details.

## Quick start

1. Describe a gateway and a model in `~/.config/withnav/models.json`.
2. Give the `build` agent that model in `~/.config/withnav/config.json`.
3. Store your API key with `nav auth set <provider>`, or export the variable named in `api_key_env`.
4. Run `nav doctor --offline` to check the setup.
5. Open a project in VS Code and open the Nav chat, or run `nav tui` or `nav web`.

The [quick start](https://withnav.dev/docs/quick-start.html) has the two files, ready to copy.

## Documentation

The full guide is at **<https://withnav.dev/docs/>**:

- [Install](https://withnav.dev/docs/install.html) and [quick start](https://withnav.dev/docs/quick-start.html)
- [Using Nav](https://withnav.dev/docs/using-nav.html) and the [command line](https://withnav.dev/docs/cli.html)
- [Configuration](https://withnav.dev/docs/configuration.html): models, layers and agents
- [Permissions](https://withnav.dev/docs/permissions.html): how Nav asks before it acts
- [Troubleshooting](https://withnav.dev/docs/troubleshooting.html), including `nav doctor`

To work on the code, read [`ARCHITECTURE.md`](ARCHITECTURE.md) for how it is laid out and
[`CONTEXT.md`](CONTEXT.md) for the vocabulary.

## Status

WithNav is pre-1.0 and moves fast. Every release and its changes are listed in
[`CHANGELOG.md`](CHANGELOG.md).

## Contributing and community

- Ask questions → [GitHub Discussions (Q&A)](https://github.com/PixmaNts/withnav/discussions/new?category=q-a)
- Report a bug → the [bug issue form](https://github.com/PixmaNts/withnav/issues/new?template=bug_report.yml)
- Contribute → [CONTRIBUTING.md](CONTRIBUTING.md)
- Code of Conduct → [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)
- Report a security issue → [SECURITY.md](SECURITY.md)

## License

WithNav is dual-licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your
option.
