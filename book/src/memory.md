# Memory register

Use `/remember <text>` to save a memory item immediately. It belongs to the active project, or to this session if it has no project. Linked Git worktrees share a project; separate clones do not. To choose a scope explicitly, start the text with `@global`, `@group:<name>`, `@project`, or `@session`. Without `@`, a word such as `global` stays part of the text.

- `/memory` lists eligible active items; `/memory search <text>` searches them.
- `/memory show <id>` shows the current version; `/memory history <id>` lists all versions.
- `/memory revise <id> v<n> <text>` appends a new wording if `n` is still current.
- `/memory invalidate <id> v<n> [reason]` retires that version without erasing it.
- `/memory restore <id> v<n>` makes an invalidated item active again with a new version.
- `/forget <id> [reason]` invalidates whatever version is current when the write runs.

Use a full item ID or a unique prefix of at least six characters. Memory is not permission: even an item phrased as an instruction never grants access or becomes a Captain message in the permission flow.

The register records the source session for slash commands. The source event sequence is currently null.

## HTTP API

Memory routes use the server's configured bearer authentication.

- `GET /v1/memory?session=&scope=&scope_ref=&status=&kind=&q=&limit=` returns `{items}`.
- `POST /v1/memory` takes `{text, scope, scope_ref?, title?, why?, kind?, source_session_id?}` and returns the item with status 201.
- `GET /v1/memory/{id}` returns the item with `versions`.
- `POST /v1/memory/{id}/revise`, `/invalidate`, `/restore`, and `/rescope` take `expected_version` and an optional `reason`. Revise accepts `text`, `title`, or `why`. Restore accepts `from_version`. Rescope takes `scope`, optional `scope_ref`, and `confirm: true` for widening.
- `GET /v1/projects` returns `{projects, groups, history}`. Projects include aliases and session links. `group_answered: false` means the group question is open; creation does not publish an event.
- `POST /v1/groups` and `PATCH /v1/groups/{id}` take `{name}` and return the created or renamed group.
- `PUT /v1/projects/{id}/groups/{group}` takes `{confirm: true}` to apply the addition. `DELETE` at the same path removes the membership. Both return the membership change.
- `PUT /v1/sessions/{id}/project` takes `{project_id}` (a project ID or `null`) and returns the attachment or detachment change.
- `POST /v1/projects/{id}/group-answer` takes `{group_id, later?}`. Use `null` for no group or `later: true` to leave the question open. Returns the project.
- `GET /v1/memory/needs-you` returns `{changes}` (up to five pending changes). `POST /v1/memory/needs-you/{change}/accept` or `/reject` returns the decided change.

A stale write returns HTTP 409 with `{reason: "version_conflict", current_version}`. An unconfirmed widening returns HTTP 409 with `{reason: "widening_requires_confirmation", affected_projects}`.

Include a `source_session_id` to apply that session's secret redaction. There is no global secret matcher: without a source session, the authenticated Captain's text is stored as given. Do not put secrets in that text.

Root sessions with write access can use `project_group_add(project_id, group_id)` and `session_attach(project_id)` to request changes. These tools only propose widening; the Captain decides in Needs you. `project_group_remove(project_id, group_id)` acts immediately only on the session's active project; otherwise it proposes removal. `session_detach()` applies immediately. Both leave history. None of the four controls authorizes other tools.

## Web pages

`nav web` shows the register on two pages, reached from the top of the sidebar or the command palette.

- `#/memory` lists items. Filter by scope, kind and status (active by default); the search box matches title, wording, quote and the stated reason as you type. Select an item to read its wording, its exact quote and every version, tick two versions to see the word-by-word difference, and edit, invalidate, restore, or change scope. Each write takes an optional reason and appears in the item's history. A toast offers **Undo**, which writes a new version that restores the previous state; nothing is overwritten.
- If the item changed while you were editing (HTTP 409 `version_conflict`), the page shows the newer version, keeps your draft, and offers to apply it on top of that version.
- A move that lets more projects read an item (HTTP 409 `widening_requires_confirmation`) asks you to confirm, listing the projects that would start to read it.
- **Needs you** (`#/memory?view=needs`, with a badge in the sidebar) lists up to five proposed project-group and session-project changes that wait for your answer. Accept or reject each one.
- `#/projects` shows groups with their member and item counts, each project with its paths and sessions, and a history of membership changes. Add or remove a project from a group (adding asks first when it widens who reads what), attach or detach a session (the link stays in the history), and create or rename a group.
- The first time a session starts in a repository the register does not know, a dialog asks which group it belongs to. **Ask me later** leaves the question open; it is asked again at the next start.

Not shown yet: relations between items, which sessions an item was sent to, pinning, and filtering by authority. Proposed items cannot be accepted from the page because the server has no route for it.
