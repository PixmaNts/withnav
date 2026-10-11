# AGENTS.md

Guidance for AI agents working in WithNav (formerly Navai / `rust-agent`).

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
  `npm run compile` in `vscode-withnav/` all pass. Its description says what changed for the
  Captain, and how it was verified.
- A release: bump the version in `Cargo.toml` and `vscode-withnav/package.json`, move the
  `Unreleased` entries of `CHANGELOG.md` under the new version, merge `develop` into `main`, tag
  `vX.Y.Z`, push the tag. The tag triggers `.github/workflows/release.yml`, which builds the static
  Linux binaries and the VSIX packages and publishes the release with them (`scripts/package-vsix.sh`
  builds the same files locally into `dist/`).
- Parallel agents work in separate git worktrees, one branch each, and never edit another
  branch's files.

## Agent skills

### Issue tracker

Issues and specs live as local markdown files under `.scratch/<feature-slug>/`. See `docs/agents/issue-tracker.md`.

### Triage labels

The five canonical triage roles, each label string equal to its name. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `CONTEXT.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.
