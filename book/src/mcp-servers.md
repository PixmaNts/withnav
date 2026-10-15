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
that server. On Unix, children inherit PATH, HOME, LANG, LC_*, TMPDIR and XDG_* plus declared
values, not every provider credential in Nav's environment. On Windows, they also inherit the
runtime variables PATH, PATHEXT, SYSTEMROOT, SYSTEMDRIVE, WINDIR, COMSPEC, TEMP, TMP,
USERPROFILE, APPDATA, LOCALAPPDATA, PROGRAMDATA, PROGRAMFILES, PROGRAMFILES(X86),
PROGRAMW6432, COMMONPROGRAMFILES, NUMBER_OF_PROCESSORS and PROCESSOR_ARCHITECTURE.
Windows variable names are case-insensitive. Commands such as `npx` or `uvx`, including `.cmd`
shims, are found through the child's Path/PATHEXT. Resolved values are never saved.

Tools are exposed as `mcp__github__get_issue`, with provider-safe names at most 64 characters.
The optional `tools` map hides original tool names matching any `false` glob. An agent's
existing exact-name `tools:` frontmatter can hide the exposed name too. A subagent receives
its parent session's MCP tools, unless its own definition or its parent's hides them; a server
that became ready after the parent session started is absent from both.
A tool whose name contains a configured secret value is skipped (see `nav doctor`), and so is a
tool whose schema combines a `$ref` with sibling constraints that would replace the referenced
ones, or whose `$ref` merge would change what `additionalProperties`, `unevaluatedProperties`,
`additionalItems` or `items` applies to. Descriptions, schemas, results and the stderr tail are
redacted of configured secret values.

Redaction catches secrets echoed as-is or in ordinary JSON; it is not a guarantee against a
server that deliberately disguises a secret it was given. Known gaps: a secret spelled with
unusual JSON escapes (a `\u` escape of an ASCII character, `\/`, mixed hexadecimal case), and a
secret that is a number inside a schema `default`, `examples` or `enum`. A server that re-encodes
its own secret already holds it and could send it anywhere: installing it is a decision to
trust it.

MCP schemas count toward the ordinary context budget; hide tools you do not need.

Servers start in the background. A slow or failed server does not prevent session creation;
its tools are absent from that session. `nav run` is the exception: before creating its single
session it waits until every enabled server is ready or failed, at most the largest
`startup_timeout_ms` (and never more than 30 s). Existing tool inventories are frozen: a server becoming
ready later requires a new session. Calls fail clearly on connection loss, have deadlines and
cancel notifications, and results are limited to 64 KiB. Crashed servers receive five bounded
restart attempts. Shutdown cleans up process groups, including launcher descendants. On Windows,
each server runs in a job object, so stopping it also stops its children.

MCP tools use ordinary permission rules. With no matching rule, the default is ask; the permission
flow may still allow the call, for example when the Captain asked for it. Without a flow that
answers, WithNav asks. Always saves a global allow rule for
that exposed tool name; yolo runs them unchecked.
Installed servers run arbitrary programs with your account's privileges: WithNav is not a
sandbox. Server descriptions/results are untrusted text, not authorization.

This delivery supports global stdio servers only. Workspace configs, HTTP, live refresh,
resources, prompts and sampling are not supported. `nav doctor --offline` checks declarations
without executing them; ordinary `nav doctor` launches enabled configured commands.
