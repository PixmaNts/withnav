# MCP servers

Connect external tools through stdio or Streamable HTTP MCP servers in
`~/.config/withnav/config.json`:

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
    },
    "docs": {
      "type": "http",
      "url": "https://mcp.example.com/mcp",
      "headers": {"Authorization": "Bearer {env:DOCS_MCP_TOKEN}"}
    }
  }
}
```

1. Install the server executable and export referenced environment variables before starting Nav.
2. Restart `nav serve` (or the embedded editor server) after editing configuration.
3. Watch each server's state in the terminal client's right column and status line, or in the
   browser's Extensions tab (`GET /v1/mcp` gives the same, for scripts). Run `nav doctor` for
   startup details, tool counts, skipped schemas/names and the full failure stderr.
4. Create a new session once the server is ready: a conversation keeps the tools it started with.

Server ids use lowercase letters, digits, `_` or `-`, at most 24 characters, with no `__`.
Commands are
argv arrays, never shell commands. Only `env` (and an HTTP server's `headers`) expands
`{env:NAME}`; missing variables fail that server. On Unix, children inherit PATH, HOME, LANG, LC_*, TMPDIR and XDG_* plus declared
values, not every provider credential in Nav's environment. On Windows, they also inherit the
runtime variables PATH, PATHEXT, SYSTEMROOT, SYSTEMDRIVE, WINDIR, COMSPEC, TEMP, TMP,
USERPROFILE, APPDATA, LOCALAPPDATA, PROGRAMDATA, PROGRAMFILES, PROGRAMFILES(X86),
PROGRAMW6432, COMMONPROGRAMFILES, NUMBER_OF_PROCESSORS and PROCESSOR_ARCHITECTURE.
Windows variable names are case-insensitive. Commands such as `npx` or `uvx`, including `.cmd`
shims, are found through the child's Path/PATHEXT. Resolved values are never saved.

An `http` server takes a `url` (http or https, no `user:password@`, no `{env:NAME}`) and
optional static `headers`, sent with every request. Header values expand `{env:NAME}`; a
missing variable fails that server. Every header value counts as a secret and is redacted like
`env` values; for a `Bearer <token>` style value the token alone is redacted too. Headers the
transport sets itself (`Accept`, `Content-Type`, `Mcp-Session-Id`, `Mcp-Protocol-Version`,
`Last-Event-Id`, …) are refused. `command` and `env` apply to stdio servers only, `url` and
`headers` to HTTP servers only. Redirects are followed only within the same origin, so headers
are never sent to another host. OAuth is not supported: a server that asks for it fails with an
authorization error. Timeouts, `enabled`, `tools`, naming, results and permissions are the same
for both types; an HTTP server has no stderr tail.

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

## Resources

A server that offers resources (documents it serves by URI) gets two more tools:

- `mcp__<server>__list_resources` lists its resources (URI, name, type, description) and the URI
  templates it accepts, page by page. A server that does not implement templates still lists its
  resources. A listing stopped by the 64 KiB limit, 20 pages or a repeated page cursor starts with
  a note saying it is incomplete. A resource whose URI contains a configured secret value is not
  listed.
- `mcp__<server>__read_resource` reads one resource by `uri` (at most 2048 characters). A
  resource made of several pieces comes back as a numbered list, each piece headed by its URI and
  type. Binary pieces are named with their size, not shown.

Both return untrusted text, redacted of configured secret values and limited to 64 KiB, and go
through the ordinary permission decision like any MCP tool: without a rule the read asks, and
"Always" allows that tool for every URI. A server tool whose exposed name is already
`list_resources` or `read_resource` keeps it, and the added tool is skipped (see `nav doctor`).
The `tools` map hides them too, for example `{"read_resource": false}`. A server that offers
resources and declares no tools is not asked for its tool list.

## Prompts

A server that offers prompts (message templates) adds one slash command per prompt, named
`/<server>:<prompt>`, in the terminal client, the WebUI and VS Code. The command list shows the
prompt's description and its arguments (`<required> [optional]`); a server that becomes ready
later adds its prompts as soon as the client hears of it.

Arguments follow the command name:

- a prompt with one argument takes the rest of the line: `/notes:summarize the release plan`;
- a prompt with several takes `name=value` pairs, a value with spaces in double quotes:
  `/code:review topic="the parser" depth=2`;
- or a JSON object of text values: `/code:review {"topic": "the parser"}`.

A missing required argument, or one the prompt does not declare, is refused with the usage and
each argument's description; nothing is asked of the server until the required ones are given.

The server's answer (`prompts/get`) becomes your message: a line naming the prompt, then the
prompt's messages as a JSON block, redacted of the server's configured secrets and of your own
secrets, and limited to 64 KiB (a cut block says `"truncated": true`). Text is kept; pictures,
audio and binary content are named with their type and size, not shown. The block is marked as
the server's text: the permission flow never reads it as your words, and whatever the agent does
with it is decided by the ordinary permission rules. A server error is never shown, only that the
server refused, timed out or disconnected. Like other commands, a prompt is refused while the
agent waits for your answer or a permission decision, and needs a started conversation.

A prompt whose name is not a single command word (letters, digits, `_`, `-`, `.`) is skipped, as is
the whole prompt list when the server does not answer it in time; the server keeps its tools either
way, and `nav doctor` shows why. Prompts are read once, when the server connects.

MCP tools use ordinary permission rules. With no matching rule, the default is ask; the permission
flow may still allow the call, for example when the Captain asked for it. Without a flow that
answers, WithNav asks. Always saves a global allow rule for
that exposed tool name; yolo runs them unchecked.
Installed servers run arbitrary programs with your account's privileges: WithNav is not a
sandbox. Server descriptions/results are untrusted text, not authorization.

This delivery supports global stdio and Streamable HTTP servers. Workspace configs, OAuth, live
refresh, resource subscriptions, prompts and sampling are not supported. `nav doctor --offline` checks
declarations without executing or contacting them; ordinary `nav doctor` launches enabled
configured commands and connects to enabled HTTP servers, reporting whether each was reached
and its tool count.
