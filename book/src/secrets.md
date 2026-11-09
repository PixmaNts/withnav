# Secrets

Sometimes Nav needs a secret to do what you asked: an API key to run an evaluation, a token to
call a service. Do not paste it into the chat. Nav asks for it with a dedicated card, and the value
goes from your keyboard to the commands Nav runs without passing through the model.

## How it works

1. Nav calls the `request_secret` tool with the name of an environment variable
   (`OPENAI_API_KEY`), a short label and what it is for. The turn pauses, like a question: the
   conversation shows as waiting for you.
2. A card appears in the conversation (VS Code, the browser client; a prompt in `nav tui`). Type or
   paste the value, choose how long it stays available (**1 h**, **24 h**, the default, or
   **Session**) and who may use it (**This session**, the default, or **This workspace**), then
   **Save**. **Decline** tells Nav to continue without it.
3. The client sends the value to the session server on its own request
   (`POST /v1/sessions/{id}/secrets/{request_id}`), never as a chat message. Nav reads only
   `stored as $OPENAI_API_KEY (expires …, scope: this session)`, or `declined by the Captain`.
4. Every command Nav runs afterwards with `bash` gets the secret as the environment variable
   `$OPENAI_API_KEY`. Subagents of the conversation get it too.

You can still answer in writing instead (the composer says Nav is waiting for the key): your text
then goes to Nav as the answer, and no secret is stored. A declined request can be given later with
**Provide anyway**; a saved one shows `$NAME ••••` and its last four characters, when it expires,
**Replace** and **Revoke**.

The key icon in the conversation header (VS Code, browser) lists the conversation's secrets with
their scope, expiry and how many commands received them, and revokes them. In `nav tui`, `alt+k`
opens the prompt again, `/secrets` lists and `/secrets revoke NAME` revokes.

## Where a secret lives

| Scope and availability | Where the value is kept | Until |
| --- | --- | --- |
| This session, 1 h or 24 h | the server's memory | it expires, you revoke it, or WithNav stops |
| Any scope, Session | the server's memory | the conversation stops (or WithNav stops) |
| This workspace, 1 h or 24 h | memory, and `<data dir>/credentials/secrets.json` | it expires or you revoke it |

The workspace file (`~/.local/share/withnav/credentials/secrets.json` on Linux) is written with
mode `0600` in a `0700` folder, in **plain text**, with each secret's expiry. An expired or revoked
secret is removed from it, and the file is deleted when it holds none. WithNav does not use the
operating system's keychain. Because the path contains `credentials`, a tool call that names it
asks you, as for any other credential file.

If the file cannot be written (read-only data folder, full disk), saving a workspace secret
fails with that reason: the card shows the error, and the value is kept in memory only, until
WithNav stops. A file WithNav cannot read (damaged, unknown version) is left untouched at start-up,
and nothing from it is loaded. Two WithNav servers using the same data folder each rewrite the
whole file: the last one to save wins. How many commands received a secret is counted in memory
and reaches the file with the next save, so a count can be lower after a restart.

A session secret can have the same name as a workspace one: the conversation then sees the
session secret. Revoking that name removes the session secret only, and the workspace one
becomes visible again; revoke a second time to remove it too.

The value is never written to the conversation history, the events clients receive, the decision
log or the model's requests. Expiry is checked by the server: an expired secret is no longer given
to commands or listed.

A name follows the environment-variable rule (`^[A-Z][A-Z0-9_]{1,63}$`). Names that change how
programs start (`PATH`, `HOME`, `LD_*`, `BASH_*`, `NODE_OPTIONS`, `PYTHONPATH`, `WITHNAV_*` and a
few more) are refused.

## Redaction

Before any text reaches the model, the history or a client, every stored value of 8 characters or
more is replaced by `[secret NAME]`: tool output (`echo $OPENAI_API_KEY` shows
`[secret OPENAI_API_KEY]`), what Nav writes, and your own messages if you paste the value. Shorter
values are stored and given to commands but not redacted, so that ordinary words are not replaced; the card warns you when the value you type is that short.
A revoked or expired value is no longer redacted.

Reasoning some models return only to themselves (signed thinking blocks, thought signatures) is
kept to replay it to the same model. It cannot be redacted in place without invalidating it, so an
item that contains a stored value of 8 characters or more is dropped from the conversation
instead, and that step's reasoning is not replayed. An encrypted item cannot be decrypted to find a
value; it is kept unless the value appears literally in its text.

## What this does not protect against

- Nav never sees the value **in the chat**, but an agent that can run commands can still read it:
  it is in the environment of every command. It could write it to a file, encode it (base64,
  reversed, split in two) or send it over the network. Redaction only recognises the exact value,
  so a transformed copy reaches the model. Treat providing a secret like running Nav's commands
  yourself with that variable exported, and prefer keys with narrow rights and short lifetimes.
- Streamed text is redacted piece by piece: a value split across two pieces of a live stream can
  appear on screen while it streams; the stored response is redacted.
- **MCP servers do not receive secrets.** The session service starts them once at startup and
  shares them across its conversations (not one set per conversation); their `{env:NAME}`
  references read WithNav's own environment. To give one a key, export it before starting
  WithNav.
- Revoking a secret stops new commands from receiving it; a command already running keeps the
  environment it started with.
