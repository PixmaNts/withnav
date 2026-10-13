# WithNav

**You set the course. Nav lights the way.**

WithNav is a pair-programming system. You keep the keyboard and stay the author of the code.
Several models of different speeds watch your work, complete it, judge it and discuss it with
you. You get the speed of AI assistance without losing your mental model of your own codebase.

In this guide you are **the Captain**. **Nav** is the assistant you talk to. WithNav is the whole
system around them.

## The four layers

Each layer has one job and its own model. You choose the models; see [Layers](layers.md).

| Layer | What it does | Where you see it |
|---|---|---|
| **Reflex** | Completes what you type, fast. | Ghost text in the editor (suggestions). |
| **Instinct** | Makes small, typed decisions: when to speak, whether a tool call is safe. It never writes free text. | Mostly invisible. It decides when Nav speaks and which tool calls need your approval. |
| **Metacognition** | Compares your code with your intent, and decides doubts about a long turn. | Short remarks at the end of a line. You rate each one: useful, noise or dismissed. |
| **Cognition** | The conversation. It discusses concepts and problems, and does delegated work with the full set of tools. | The Nav chat in VS Code, the terminal or the browser. |

Without a delegation, Nav assists and you write the code. When you ask Nav to do a piece of
work itself, that is a **delegation**.

## Nav and the crew

Cognition runs **agents**. Nav is the main one, working as `build` (implement, verify, report) or
`plan` (research and write plans in Markdown). Behind Nav is a crew you can call on:

| Member | Agent id | Role |
|---|---|---|
| Vigie, the Lookout | `vigie` | Read-only scouting of code, documentation and the web, with sources. |
| Lux, the Expert Advisor | `lux` | Read-only analysis and recommendations. |
| Mousse, the Lightweight Deckhand | `mousse` | Small local edits, after your approval. |
| Cartographe, the Cartographer | `cartographe` | On demand. Draws maps as Mermaid charts in `.withnav/charts/`. |
| Sondeur, the Deep Diver | `sondeur` | On demand. Root-cause work; runs tests and profilers after your approval. |

## What you install

- **`nav`**, one binary. It is the agent core and its clients:
  - `nav tui`: a terminal chat client.
  - `nav web`: a chat client in your browser, with the InstinctFlow Studio.
  - `nav run`: one task from a script or a shell.
  - `nav serve`: the session server alone.
  - `nav lsp`: the language server behind the editor features.
  - `nav doctor` and `nav auth`: diagnostics and API keys.
- **The VS Code extension**. It starts `nav lsp`, shows remarks and suggestions, and hosts the
  Nav chat. It can bundle `nav`.

## Where to go next

1. [Install](install.md) WithNav.
2. Follow the [Quick start](quick-start.md) to a first working session.
3. Read [Permissions](permissions.md) before you let Nav run commands.

WithNav is pre-1.0 and moves fast. Every release is listed in the [Changelog](changelog.md).
