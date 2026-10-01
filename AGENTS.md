# AGENTS.md

Guidance for AI agents working in this repository.

## Read first

[`ARCHITECTURE.md`](ARCHITECTURE.md): what runs, which module owns what, the data flows, and where
new code goes. Check its "Where new code goes" list before writing a helper or a test harness: the
one you need probably exists.

## Agent skills

### Issue tracker

Issues and specs live as local markdown files under `.scratch/<feature-slug>/`. See `docs/agents/issue-tracker.md`.

### Triage labels

The five canonical triage roles, each label string equal to its name. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `CONTEXT.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.
