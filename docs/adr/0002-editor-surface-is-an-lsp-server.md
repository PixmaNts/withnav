# The editor surface is a Language Server plus a thin VS Code extension

The navigator must see every edit the driver makes, and a coding-agent plugin (such as an OpenCode
plugin) only sees the agent's own events. A Language Server receives edits for free
(`didChange` / `didSave`), is written in Rust inside this crate, and is editor-agnostic. VS Code
cannot attach to an arbitrary Language Server without an extension, so a thin extension launches it
and renders what standard LSP renders poorly: VS Code shows `Hint` diagnostics as faint dots only,
so remarks travel as custom `navigator/*` notifications and are drawn as end-of-line decorations.

A prototype built on 2026-09-29 validated the experience: the first remark arrived about 2.4 s after
the trigger, anchored on the right line, and was relevant.

Naming status: the human role is now called the Captain and the product WithNav;
see [ADR 0014](0014-rebrand-withnav.md). Original decision wording is retained.
