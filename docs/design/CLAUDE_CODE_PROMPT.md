# Rebrand Navai → WithNav: migrate the repo

You are working in `PixmaNts/Navai` (crate `rust-agent`). The product is renamed **WithNav** (domain `withnav.dev`). A design handoff is in `design_handoff_withnav/`: read `README.md` first, then use `WithNav Brand.dc.html` (brand book, open in a browser with `support.js` beside it) as the source of truth for names, copy and colors. The other `.dc.html` files are HTML references for the TUI, VS Code and WebUI screens. They are not code to copy.

Work on a branch `rebrand/withnav`. Make one commit per step below. Run `cargo build`, `cargo test` and `cargo clippy` after each Rust step, and `npm run compile` in the VS Code extension after its step. Do not rename the GitHub repo or change links to `github.com/PixmaNts/Navai`.

## 0. Inventory first
Before editing, run `rg -i "navai|navigator|explore|explorer|librarian|oracle|review|driver"` and write the list of hits to `REBRAND_TODO.md`, grouped by: user-facing string, identifier, path/config key, docs. Ask me before touching anything that is a public API field, a persisted DB column or a config key that would break existing users. For those, add a fallback that reads the old name.

## 1. Names
| Old | New |
|---|---|
| Navai, navai (product) | WithNav |
| `navai` CLI binary | `nav` (keep a `navai` alias binary or symlink for one release, printing a deprecation notice) |
| `.navigator/`, `.navai/` (project dir) | `.withnav/` (read old dir if new one is absent; log a one-line migration hint) |
| `~/.config/navai`, `~/.local/share/navai`, `$XDG_RUNTIME_DIR/navai`, `/tmp/navai` | `…/withnav` (same fallback) |
| `navai.db` | `withnav.db` (move on first start if only the old file exists) |
| `NAVAI_*` env vars | `WITHNAV_*` (still read `NAVAI_*` with a warning) |
| `vscode-navigator/`, extension id `pixmants.navai`, commands `navigator.*`, settings `navai.*` | `vscode-withnav/`, `pixmants.withnav`, `withnav.*` |
| `navai-lsp` / "navigator language server" | `withnav-lsp` / "WithNav language server" |
| The human, "driver" | **the Captain** (`author: "captain"`; accept `"driver"` when reading) |
| UI label "Cognition" for the chat/agent panel | **Nav** (the layer itself keeps the name Cognition) |

Leave the internal module `src/navigator/` in place unless the rename is trivial. If you rename it, do it in its own commit.

## 2. Agents (the Crew)
Nav is the single point of contact. It runs the `build` and `plan` modes. Update `src/agent/definitions.rs`, the default agent Markdown files, `models.example.json` and `config.example.json`:

| Old key | New key | Crew name (FR / EN) | Mode | Permissions | Default model (models.example.json) |
|---|---|---|---|---|---|
| `build`, `plan` | unchanged | Nav · Le Navigateur / The Navigator | primary | read, write after confirmation | unchanged |
| `explore`, `librarian` | `vigie` | Vigie · L'Exploratrice / The Lookout | subagent | read-only (code, docs, web), must cite sources | `omniroute/openai/gpt-6-luna` · medium |
| `review`, `oracle` | `lux` | Lux · Le Conseiller expert / The Expert Advisor | all | read-only, produces recommendations | `omniroute/openai/gpt-6-astra` · high |
| `explorer` | `mousse` | Mousse · L'Équipier léger / The Lightweight Deckhand | subagent | local edits after approval | `omniroute/opencode-go/deepseek-v4-flash` · low |
| new | `cartographe` | Cartographe · Le Cartographe / The Cartographer | subagent, on demand | read-only, writes Mermaid charts to `.withnav/charts/` | `omniroute/openai/gpt-6-luna` · medium |
| new | `sondeur` | Sondeur · Le Sondeur / The Deep Diver | subagent, on demand | read, runs tests and profilers after approval | `omniroute/cc/claude-opus-5-5` · high |

- Old keys must still resolve, as aliases that log a deprecation warning.
- Rename tools such as `spawn_oracle` to `spawn_lux`.
- Write the system prompts from the brand book (› Crew: role, personality, phrasing). In particular, Vigie separates "Observed" from "Not verified", and Lux answers as Fact / Hypothesis / Recommendation.
- No agent may refer to itself as the Captain.

## 3. Layers
Keep Reflex, Instinct, Metacognition and Cognition as technical names. Add the explanatory labels Reflex · Completion, Instinct · Triage, Metacognition · Beacon, Cognition · Nav and the crew. Metacognition remarks are now shown as **Beacons signed by Nav**. A Metacognition "wake" targets Nav, so update strings like `metacognition → cognition` to `metacognition → nav`. Layers have no persona: no "I", no agent icon.

## 4. Colors and type
Keep the Tide Dark / Tide Light grounds from `README.md` › Design tokens. Add these agent roles to the theme struct (`crates/withnav-tui/src/theme.rs`), the CSS variables of the WebUI and webview, and the VS Code theme mapping:

| Role | Tide Dark | Tide Light |
|---|---|---|
| nav (also `la`, logo rings, primary interaction) | `#5ecaed` | `#036b9a` |
| lux | `#f8ca65` | `#8f5d04` |
| mousse | `#f89177` | `#b14328` |
| vigie | `#72d8b3` | `#107460` |
| cartographe | `#dca3e5` | `#85468f` |
| sondeur | `#a3b3f8` | `#4d5aac` |

Every agent must also be identified by its glyph and name, never by color alone. Fonts: Bricolage Grotesque 600 (brand and headings), IBM Plex Sans (UI text), JetBrains Mono (code). Icons: Phosphor regular.

## 5. Logo and assets
- Use `design_handoff_withnav/logo/`: `withnav-mark*.svg`, `withnav-favicon.svg`, `withnav-icon*.svg`, `withnav-wordmark-*.svg` and `withnav-wordtype-*.svg` (all outlined, no font dependency), plus `agents/<name>-{dark,light,mono}.svg`.
- VS Code: `media/withnav.svg` comes from `withnav-mark-mono.svg`, and the Marketplace icon is a 512 px PNG from `withnav-icon.svg`.
- TUI header: `◆ withnav`, with ◆ in `nav` and the name in `text`.
- Remove the old wordmark files.

## 6. Copy
- Use the brand book's Microcopy section for loading, error, success, uncertainty and confirmation messages.
- Keep standard technical words (commit, test, branch, file).
- Use maritime terms only for feature names, each with a label: "Logbook · Activity history", "Voyage · Session", "Safe Harbor · Checkpoint", "Beacon · Remark".
- An error is always stated plainly, for example "cargo test failed (2 tests). Nothing was committed."
- Primary CTAs are "Set the Course / Fixer le cap" and "Confirm the Route / Confirmer la route".

## 7. Docs and site
Replace the repo's docs site with `site/` from the handoff, if present: the mdBook sources in `site/docs/{en,fr}`, the theme in `site/docs/theme/withnav.{css,js}` and the workflow `site/.github/workflows/docs.yml`, which goes to `.github/workflows/`. Then update `README.md`, `CHANGELOG.md` (add an "Unreleased · Rebrand to WithNav" entry listing every renamed path, key and env var, with their fallbacks), `Cargo.toml` (`description`, `homepage = "https://withnav.dev"`, binary name), `CONTEXT.md` (vocabulary: Captain, Nav, Crew, Beacon) and the ADRs. Add a new ADR `docs/adr/00NN-rebrand-withnav.md` that records the decision and the compatibility window.

## 8. Done when
- `rg -i navai` only matches the GitHub URL, the compatibility fallbacks, the CHANGELOG and the ADR.
- Old configs, `.navigator/` dirs, `NAVAI_*` vars and old agent keys still work, with warnings, and tests cover each fallback.
- The TUI, VS Code and WebUI match the handoff screens for names, glyphs and colors in both Tide themes.
- `REBRAND_TODO.md` is fully checked, then deleted in the last commit.
