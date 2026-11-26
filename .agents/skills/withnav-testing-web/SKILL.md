---
name: withnav-testing-web
description: Browser-client checks of the WithNav self-test (nav web). Load from withnav-testing when the Captain tests from the browser client.
---

# WithNav self-test — browser client

Same protocol as `withnav-testing`. `<run>` is the run folder.

## Hooks in the shared scenarios

- **V1**: `nav web` is the `nav` binary, so `nav --version` is the server's version.
- **P**: questions are cards with Once / Always / Reject buttons.
- **I1 / M1**: a stored image shows as a thumbnail; a click opens it in the Image tab of the session
  panel.
- **C1 / C2**: the attach button, paste, or drop into the composer, then send.
- **S1**: the secret card. Before submitting it, run **W2** on it.
- **S5**: the key icon in the top bar.
- **A1**: the subagent appears in the session panel.
- **N, G, X settings.** The gear in the top bar opens Settings. A layer model applies after a
  restart: the Captain runs `nav serve stop` in a terminal, then `nav web --workspace <run>` and
  reopens this conversation from the new link. The conversation's title in the sidebar is what the
  Captain reads for the id check. Notices ("instinct rejected") show in the transcript: quote them.
  G4c and G5c: what the session panel and transcript show of the check result and the note.
- **R**: the memory conversation is a conversation whose workspace is `<run>/memrepo`. Start `nav
  web` with `--workspace <run> --workspace <run>/memrepo` (repeat the flag) so the page offers both,
  or the Captain runs a second `nav web --workspace <run>/memrepo` for it. Slash commands answer as
  transcript lines (`Remembered "<title>" · <scope> · <id> v<n>`, `Forgot …`, `<title> · <id> v<n>`).
- **D**: the sidebar's top entries **Memory** and **Projects** (`#/memory`, `#/projects`), the badge
  on **Memory** (what waits for the Captain), and the group dialog. D5's conversation starts in
  `<run>/memrepo-new`, so the page needs `--workspace <run>/memrepo-new` too.
- **O**: the session panel. O2 and O3 (Goal criteria statuses, Memory tab) are pending until the
  Session tab lands; run them then. O4's quiet lines in the transcript, same.
- **H** (pending): the permission card, its "reviewing…" line, Hold for me, the badges and the
  browser notification (H8: the browser must have granted notifications; record the permission).

## Client checks

- **W1 Too large.** The Captain attaches `big.png`. PASS when it is refused before it is listed.
- **W2 Secret card by keyboard** (during S1). Using only the keyboard: Tab reaches one option per
  group, the arrow keys move the choice and wrap at both ends, Enter submits. PASS when every
  control is reachable that way.
- **W3 Refused uploads.** The Captain attaches `truncated.png` and sends "W3". PASS when the upload
  is refused (corrupt or truncated), the composer unlocks with the draft kept, and you receive no
  image. Same with `notimage.png` and `empty.png`.
- **W4 Secrets list.** The Captain opens the key icon: the list takes the focus; Escape closes it
  and the focus returns to the icon. With it open, they switch to another conversation and back.
  PASS when each conversation shows its own list (or none), never the other's, and never stays on
  "Loading…". Smoke check: it does not force a late answer.
- **W5 Media zoom.** On an MCP image (M1) the Captain tabs to the preview and presses Enter. PASS
  when it zooms and the focus ring is visible. SKIP without M1.
- **W6 MCP App text.** If M2 ran with an app whose page has accented or non-Latin text: PASS when it
  renders unchanged.
- **W8 Needs you badge.** After R9 (the proposals wait): PASS when the **Memory** entry's badge
  shows the count (hover: "Waiting for you"), `#/memory?view=needs` lists the same rows as `GET
  /v1/memory/needs-you`, each with Accept and Reject reachable by keyboard, and the badge is gone
  after the last answer.
- **W9 Group dialog by keyboard** (during D5). PASS when focus lands inside the dialog on open,
  Tab and the arrow keys move through the radio group (each group, **No group**, **New group**;
  **New group** reveals a name field), **Save** stays disabled until a choice (and a name for a new
  group) exists, Escape and a click on the backdrop both act as **Ask me later**, and focus does not
  escape behind the dialog.
- **W10 Memory page by keyboard.** On `#/memory`: Tab reaches the filters, the search box, each
  item, and in the detail the version checkboxes and the edit, invalidate, restore and scope
  actions; Enter or Space activates. PASS when each is reachable that way and the focus ring is
  visible. The unreachable-server banner is not forced: it would stop this conversation.
- **W11 Session tab (pending).** The Goal, Ledger and Memory sections of the session panel and the
  "Remembered" line with an undo: PASS when they equal O1–O4 against HTTP. SKIP — not built until
  the Session tab lands.
- **Order.** W8–W11 and D1–D10 run before W7.
- **W7 Failed send, last.** Stopping the server stops this conversation too, so write the report
  first, then give the Captain the whole procedure in one message and end your turn:
  1. Type "W7 draft" in the composer without sending.
  2. Press `ctrl+c` in the terminal running `nav web`, then press Send in the page.
  3. Note whether an error shows, the composer unlocks and "W7 draft" is still there.
  4. Run `nav web` again with the same `--workspace`, open the new link it prints, reopen this
     conversation and send what you saw in step 3. Check also that the thumbnails of I1 and C1
     show again after the restart.
  When that message arrives, PASS when all three held in step 3 (and the thumbnails show); append
  the row to the report.
