---
name: withnav-testing-tui
description: Terminal-client checks of the WithNav self-test (nav tui). Load from withnav-testing when the Captain tests from the terminal client.
---

# WithNav self-test — terminal client

Same protocol as `withnav-testing`. The Captain types the commands and keys below; tell them exactly
what to type. `<run>` is the run folder.

## Hooks in the shared scenarios

- **Actions in the client.** For every check where the Captain acts inside the TUI (C, S4, S5,
  T1–T4, T6–T12, and every Captain step of N, G, X, R, D, O), state the action in plain text and end your turn; the Captain replies with the
  observation in their own message. Never hold an `ask` question while the Captain has an action to
  do: a pending question owns the keyboard and they cannot type in the client.
- **V1**: `nav tui` is the `nav` binary, so `nav --version` is the client's version.
- **P**: questions are inline: `o` Once, `a` Always, `r` Reject (then a message).
- **I1 / M1**: a stored image shows as a numbered line; `/image <n>` opens it in the system viewer.
  There is no inline thumbnail or zoom in the terminal.
- **C1 / C2**: `/attach <run>/ok.png`, then the message.
- **S5**: `/secrets` lists, `/secrets revoke NAV_SELFTEST_KEY` revokes.
- **A1**: `alt+s` opens the child's read-only transcript.
- **N, G, X settings.** `/config` opens Settings; `ctrl+s` writes `config.json`. A layer model
  applies after a restart: the Captain quits (`ctrl+q`), runs `nav serve stop`, then `nav tui
  --workspace <run>` and resumes this conversation with `ctrl+o`. `ctrl+o` also lists conversations:
  the Captain reads this one's title for the id check. Notices ("instinct rejected") and the
  goal-check result appear where the TUI puts them: ask the Captain to say where and quote it.
- **G**: `/goal …` in the editor; the right column (`alt+r`) shows the goal before the ledger.
  G4c: the right column's goal section (`alt+r`): `✓`, `✗` or `·` before each criterion for
  supported, contradicted and not checked, and a `claims` row with `✓`, `✗` or `?` (unverified) lines;
  the evidence is not shown here. G5c: the transcript line `Goal note · automatic observation, not a
  Captain message` and the note's text under it.
- **R**: the memory conversation is a second terminal, `nav tui --workspace <run>/memrepo`, so this
  conversation keeps its workspace. The slash commands answer as transcript lines: `Remembered:
  "<title>" · <scope> · undo with /forget <id>`, `Forgot "<title>" · <scope> · <id> v<n>`, one
  `<title> · <id> v<n>` line per item. Quote what the Captain reads (no JSON expected).
- **D**: the TUI has no Projects or Memory page. The Captain runs `nav web --workspace <run>` and
  does D1–D10 in the browser as the web skill says (D5: a conversation started from
  `nav tui --workspace <run>/memrepo-new`; record where the question appears). The accept and
  reject of R9 and R10 are answered there.
- **O**: the right column (`alt+r`, shown by itself from 140 columns): **GOAL**, **LEDGER**, **MEMORY**.
  O1 to O3 read it as the base skill says (no revision number, no evidence on hover, no link to the
  memory page); O4's line is the `Remembered: … · undo with /forget <id>` one: the terminal has no
  undo link, so the Captain types `/forget <id>`.
- **H**: the card is the inline question, with the keys line `o once   a Always   r reject   h hold`
  (`a` only when offered; `h hold` only while the review runs). Review lines: ` Metacognition is
  reviewing…`, ` waiting for you: Metacognition did not allow this call` (then the conclusion,
  ` relied on: …`, ` consequence: …`), ` held for you: the review stopped`, ` no review; your call`;
  outcomes ` allowed once by Metacognition`, ` denied`, ` review cancelled: you answered first`; and
  a notice `Already started by Metacognition — it ran once` when `o` or `h` came too late.
  `r` asks for a message (Enter sends; `esc` rejects without one). New conversation: `ctrl+n`;
  switch with `alt+1`..`alt+9`. H8: no desktop notification; a background tab is marked unread and
  the right column's `nav` row reads `waiting for you`. H7: after the restart, `nav tui
  --workspace <run>` and `ctrl+o` to resume.
- **Settings for H.** `/config` opens Settings; the Captain sets `permissions.review` to `on`,
  `ctrl+s` writes it, then `ctrl+n` opens the new conversation that reads it.
- **After A2**: wait for the second park, then the Captain presses `alt+c` on the child. PASS
  (record as **T5**) when it resumes and its next notice still reports top-ups used 1/3 (the
  Captain's continue is not counted). Then run A3 on that park.

## Client checks

- **T1 Refused at attach.** `/attach <run>/big.png`, then `/attach <run>/empty.png`. PASS when each
  is refused with its reason and neither is listed. As of 0.8.2 the reasons are
  `<file> is over the 3.9 MB limit; downscale the image first` and `<file> is empty`.
- **T2 Refused at send.** `/attach` only checks size and emptiness; the server judges the bytes
  when the message is sent. `/attach <run>/truncated.png` and send "T2". PASS when the send reports
  the image refused (corrupt or truncated) and you receive no image. `/attach clear`, then the same
  with `<run>/notimage.png` (refused as not an image).
- **T3 Attach clear.** `/attach <run>/ok.png`, `/attach clear`, send "anything attached?". PASS when
  you receive no image. (Smoke check: it does not force the race of a clear during the check.)
- **T4 Viewer.** After C1, `/image`. PASS when the system viewer opens the last picture.
- **T5** See the hook after A2 (SKIP when A did not run).
- **T6 Paste.** The Captain copies a picture and runs `/paste-image`, then sends it. PASS when you
  receive it. SKIP without `wl-paste` or `xclip`.
- **T7 MCP status.** `alt+r` shows the right column. PASS when each MCP server has a dot (green
  ready, orange starting, red failed) and the status line reads `mcp <ready>/<enabled>`, disabled
  servers not counted; no `mcp` summary when none is enabled.
- **T8 Resume.** The Captain quits (`ctrl+q`), runs `nav tui` again, `ctrl+o` and resumes this
  conversation, then sends "resumed". PASS when the images of the conversation are listed with their
  numbers again and `/image 1` opens the first. Write the report before this check, and append T8's
  row when the Captain is back.
- **T9 Drop.** The Captain drags `<run>/ok.png` from their file manager onto the terminal window,
  then sends "dropped". PASS when the image is attached (a chip or a numbered line) and you receive
  it.
- **T10 Goal and ledger column** (after G). `alt+r`. PASS when the column shows the goal before
  the ledger (objective, status, criteria count and the marked criteria) and the ledger's turns, files and
  repeated failures equal `GET /v1/sessions/<id>/ledger`. Same check as O1 to O3.
- **T11 Command answers.** In the memory conversation: `/remember @project WNST-<run id> t11`,
  `/memory`, `/memory history <id>`, `/forget <id>`. PASS when each prints the readable line of
  R2/R4/R7 (not raw JSON) and the register agrees (`GET /v1/memory`).
- **T12 Goal across a restart.** After the restart of N1/N2 (or, if N was skipped, `ctrl+q`, `nav
  serve stop`, `nav tui`, `ctrl+o`), `/goal`. PASS when the goal and its criteria are the same as
  before and the right column shows them again. Run it before T8.

T10–T12 run before T8, which quits the client.
