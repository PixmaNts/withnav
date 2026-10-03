# AGENTS.md

Guidance for AI agents working in Navai (formerly `rust-agent`).

## Read first

[`ARCHITECTURE.md`](ARCHITECTURE.md): what runs, which module owns what, the data flows, and where
new code goes. Check its "Where new code goes" list before writing a helper or a test harness: the
one you need probably exists.

## Branches and pull requests

- `main`: releases only, each one tagged `vX.Y.Z`. Never commit to it directly.
- `develop`: the integration branch. Every change reaches it through a pull request.
- Work happens on a branch off `develop`: `feat/<name>` for a feature, `fix/<name>` for a bug,
  `refactor/<name>`, `docs/<name>` or `test/<name>` otherwise. One topic per branch; short names in
  kebab-case.
- A pull request into `develop` is ready when `cargo build --bins`, `cargo test`,
  `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` and, if the extension changed,
  `npm run compile` in `vscode-navigator/` all pass. Its description says what changed for the
  driver, and how it was verified.
- A release: bump the version in `Cargo.toml` and `vscode-navigator/package.json`, move the
  `Unreleased` entries of `CHANGELOG.md` under the new version, merge `develop` into `main`, tag
  `vX.Y.Z`, push the tag.
- Parallel agents work in separate git worktrees, one branch each, and never edit another
  branch's files.

## Agent skills

### Issue tracker

Issues and specs live as local markdown files under `.scratch/<feature-slug>/`. See `docs/agents/issue-tracker.md`.

### Triage labels

The five canonical triage roles, each label string equal to its name. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `CONTEXT.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.
