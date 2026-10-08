# Rebrand the product to WithNav, with a 0.7 compatibility window

Date: 2026-10-08

Status: Accepted.

## Decision

The product is **WithNav** (withnav.dev). The human is **the Captain**; Nav is the
single point of contact, running the `build` and `plan` agents. No agent calls itself
the Captain. Write the new names; continue reading old names throughout 0.7, with
a one-line deprecation warning or migration hint. Removal is planned for 0.8.

| Old name | New name | Compatibility in 0.7 |
|---|---|---|
| Navai; crate `rust-agent` | WithNav; crate `withnav` | Product/crate naming only; no old crate alias |
| `navai`, `navai-lsp` | `nav`, `withnav-lsp` | `navai` alias prints a deprecation notice; configure the new LSP binary |
| `.navai/`, `.navigator/` | `.withnav/` | Read `.navai/`, then `.navigator/`, when the new directory is absent |
| `~/.config/navai`, `~/.local/share/navai` | `~/.config/withnav`, `~/.local/share/withnav` | Move old files on first start without overwriting new ones |
| `$XDG_RUNTIME_DIR/navai`, `~/.cache/navai/scratch`, temp `navai` | Corresponding `withnav` directories | Retain old-path compatibility during 0.7 |
| `navai.db`, `navai.jsonl` | `withnav.db`, `withnav.jsonl` | Move on first start when only the old file exists |
| `NAVAI_*` | `WITHNAV_*` | Read old variables with a warning when the new variable is unset |
| `vscode-navigator/`, `pixmants.navai`, view `navigator` | `vscode-withnav/`, `pixmants.withnav`, view `withnav` | Install the new extension; no duplicate extension or directory |
| Settings `navai.*`; commands `navigator.*` / `navai.*` | `withnav.*` | Settings fall back when unset; use new command ids |
| LSP `navigator/*` | `withnav/*` | Server accepts both; sends only `withnav/*` |
| Wire author, approval source, origin and selection `driver` | `captain` | Readers accept both; writers emit `captain` |
| Flow action `ask_driver` | `ask_captain` | Old action remains accepted in TOML |
| `explore`, `librarian` | `vigie` | Deprecated aliases with warnings |
| `review`, `oracle` | `lux` | Deprecated aliases with warnings |
| `explorer` | `mousse` | Deprecated alias with warning |

Cartographe and Sondeur join the Crew on demand. Cartographe writes Mermaid charts
to `.withnav/charts/`. Tide Dark is the default theme when no choice is saved.
Metacognition remarks are Beacons signed by Nav; the layers have no persona.

## What stays

The GitHub repository is renamed `github.com/PixmaNts/withnav` (2026-10-08); GitHub
redirects the old `github.com/PixmaNts/Navai` URL. The internal module
remains `src/navigator/`. Technical layer names remain Reflex, Instinct,
Metacognition and Cognition, labelled Completion, Triage, Beacon, and Nav and the
crew respectively. Standard terms such as commit, test, branch and file stay plain.

## Consequences

Compatibility readers must be tested, not implemented as silent renames. Existing
data remains readable; migration must not overwrite a new configuration. Historical
ADRs and released changelog entries retain their original names, with status notes
pointing here where needed. No documentation site is introduced by this decision.
