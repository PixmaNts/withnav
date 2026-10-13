# MCP servers

Connect external tools through stdio MCP servers in `~/.config/withnav/config.json`:

```json
{
  "mcp": {
    "github": {
      "type": "stdio",
      "command": ["github-mcp-server", "stdio"],
      "env": {"GITHUB_PERSONAL_ACCESS_TOKEN": "{env:GITHUB_TOKEN}"},
      "enabled": true,
      "startup_timeout_ms": 10000,
      "call_timeout_ms": 60000,
      "tools": {"delete_*": false}
    }
  }
}
```

1. Install the server executable and export referenced environment variables before starting Nav.
2. Restart `nav serve` (or the embedded editor server) after editing configuration.
3. Run `nav doctor` to inspect startup, tool counts, skipped schemas/names and failure stderr.
4. Create a new session once the server is ready.

Server ids use lowercase letters, digits, `_` or `-`, at most 24 characters, with no `__`.
Commands are
argv arrays, never shell commands. Only `env` expands `{env:NAME}`; missing variables fail
that server. Children inherit PATH, HOME, LANG, LC_*, TMPDIR and XDG_* plus declared values,
not every provider credential in Nav's environment. Resolved values are never saved.

Tools are exposed as `mcp__github__get_issue`, with provider-safe names at most 64 characters.
The optional `tools` map hides original tool names matching any `false` glob. An agent's
existing exact-name `tools:` frontmatter can hide the exposed name too. MCP schemas count
toward the ordinary context budget; hide tools you do not need.

Servers start in the background. A slow or failed server does not prevent session creation;
its tools are absent from that session. Existing tool inventories are frozen: a server becoming
ready later requires a new session. Calls fail clearly on connection loss, have deadlines and
cancel notifications, and results are limited to 64 KiB. Crashed servers receive five bounded
restart attempts. Shutdown cleans up process groups, including launcher descendants.

MCP tools use ordinary permission rules (no rule = ask). Always saves a global allow rule for
that exposed tool name; yolo runs them unchecked.
Installed servers run arbitrary programs with your account's privileges: WithNav is not a
sandbox. Server descriptions/results are untrusted text, not authorization.

This delivery supports global stdio servers only. Workspace configs, HTTP, live refresh,
resources, prompts and sampling are not supported. `nav doctor --offline` checks declarations
without executing them; ordinary `nav doctor` launches enabled configured commands.
