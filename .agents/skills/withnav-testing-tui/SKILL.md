---
name: withnav-testing-tui
description: Terminal-client checks of the WithNav self-test (nav tui). Load from withnav-testing when the Captain tests from the terminal client.
---

# WithNav self-test — terminal client

Same protocol as `withnav-testing`. The Captain types the commands and keys below; tell them exactly
what to type. `<run>` is the run folder.

## Hooks in the shared scenarios

- **Actions in the client.** For every check where the Captain acts inside the TUI (C, S4, S5,
  T1–T4, T6–T9), state the action in plain text and end your turn; the Captain replies with the
  observation in their own message. Never hold an `ask` question while the Captain has an action to
  do: a pending question owns the keyboard and they cannot type in the client.
- **V1**: `nav tui` is the `nav` binary, so `nav --version` is the client's version.
- **P**: questions are inline: `o` Once, `a` Always, `r` Reject (then a message).
- **I1 / M1**: a stored image shows as a numbered line; `/image <n>` opens it in the system viewer.
  There is no inline thumbnail or zoom in the terminal.
- **C1 / C2**: `/attach <run>/ok.png`, then the message.
- **S5**: `/secrets` lists, `/secrets revoke NAV_SELFTEST_KEY` revokes.
- **A1**: `alt+s` opens the child's read-only transcript.
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
