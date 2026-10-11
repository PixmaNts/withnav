# Contributing to WithNav

WithNav is a pair-programming system in Rust: you keep the keyboard, and several models of
different speeds watch, complete, judge and discuss. See the [README](README.md) for what it does
and [`ARCHITECTURE.md`](ARCHITECTURE.md) for how the code is laid out.

## Questions and help

Ask in [GitHub Discussions](https://github.com/PixmaNts/withnav/discussions/new?category=q-a)
(Q&A), not in issues.

## Reporting bugs

Use the [bug report form](https://github.com/PixmaNts/withnav/issues/new?template=bug_report.yml).
A good report has the WithNav version (`nav --version`), your OS, how you installed it, the steps
that reproduce the problem, what you expected and what happened. Attach the output of
`nav doctor --offline --json` when configuration may be involved. It makes no network calls.

For a security problem, do not open an issue: see [`SECURITY.md`](SECURITY.md).

## Proposing features

Use the [feature request form](https://github.com/PixmaNts/withnav/issues/new?template=feature_request.yml).
For a large idea, start a thread in
[Discussions](https://github.com/PixmaNts/withnav/discussions) first, before writing code.
Read [`CONTEXT.md`](CONTEXT.md) for the vocabulary and [`docs/adr/`](docs/adr/) for decisions
already taken.

## Development setup

From the repository root.

**Rust core** (`nav`, `nav lsp`). The minimum supported Rust version is 1.89 (`rust-version` in
`Cargo.toml`).

```sh
cargo build --release --bins
```

**Web client** (`webui/`). Its build output `webui/dist/` is committed and embedded in the binary,
so `cargo build` does not need Node. You only need it when you change the web client.

```sh
cd webui && npm ci && npm run dev
```

**VS Code extension** (`vscode-withnav/`).

```sh
cd vscode-withnav && npm ci && npm run compile
```

The repository has no `.vscode/launch.json`, so there is no ready-made F5 configuration. To try
the extension, build and install it as described in the README
(`npm run package`, then `code --install-extension vscode-withnav/withnav-*.vsix`). Releases also
publish platform-specific VSIX files for linux-x64 and linux-arm64 that bundle a static `nav`
binary, and a universal VSIX that uses `nav` from your `PATH` or the `withnav.serverPath` setting.
Native Windows is not supported yet; WSL or VS Code Remote works with the linux VSIX.

## Branching

- `main` holds releases only, each tagged `vX.Y.Z`. Never commit to it.
- Branch off `develop`: `feat/<name>`, `fix/<name>`, `refactor/<name>`, `docs/<name>` or
  `test/<name>`. One topic per branch, short kebab-case names.
- Open a pull request into `develop`.

More detail, including how releases are cut, is in [`AGENTS.md`](AGENTS.md).

## Commit style

Conventional commits, as in the history: `feat:`, `fix:`, `refactor:`, `test:`, `docs:`, `chore:`,
with an optional scope (`fix(permissions): ...`). Write in English, in the imperative mood
("add", not "added").

## Pull request checklist

A pull request is ready when these pass, run from the repository root:

```sh
cargo build --bins
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

If you changed the extension:

```sh
cd vscode-withnav && npm run compile
```

If you changed the web client:

```sh
cd webui && npm run typecheck && npm test
```

CI enforces this list. In the description, say what changed for the user and how you verified it.

## Changelog

Add a line under `Unreleased` in [`CHANGELOG.md`](CHANGELOG.md). Documentation-only changes
do not need one.

## Code of Conduct and security

- Taking part means following the [Code of Conduct](CODE_OF_CONDUCT.md).
- Report vulnerabilities privately, as described in [`SECURITY.md`](SECURITY.md).
