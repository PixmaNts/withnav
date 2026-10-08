# One process, two channels: LSP for the editor, HTTP/SSE for the conversation

The binary VS Code launches hosts everything: the Language Server, the event bus, the four layers
and the local HTTP/SSE server built for the conversation. Editor events travel over LSP; the
conversation with the cognition travels over the HTTP/SSE server, so that any client (the VS Code
chat panel and the terminal client) talks to the same client/server API. Each channel carries what it was
designed for.

## Status

Accepted for now, deliberately revisitable. The terminal client now exists (`navai tui`): it starts its
own private server, or connects to a running one with `--url`, so it does not yet share a navigator
with the editor. A separate daemon with the Language Server reduced to a relay becomes the better
shape once several editors or a TUI must share one navigator.

Naming status: the human role is now called the Captain and the product WithNav;
see [ADR 0014](0014-rebrand-withnav.md). Original decision wording is retained.
