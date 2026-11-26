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
  G4c: the right panel's **Session** tab, **GOAL** section: each criterion reads `supported`,
  `contradicted` or `not checked` (evidence on hover), the goal row `last checked <time>`, and
  **CLAIMS · agent's last turn** lists the claims as `supported`, `contradicted` or `unverified`.
  G5c: the transcript block `Goal note · automatic observation, not a Captain message` with the
  note's text and its age.
- **R**: the memory conversation is a conversation whose workspace is `<run>/memrepo`. Start `nav
  web` with `--workspace <run> --workspace <run>/memrepo` (repeat the flag) so the page offers both,
  or the Captain runs a second `nav web --workspace <run>/memrepo` for it. Slash commands answer as
  transcript lines (`Remembered: "<title>" · <scope> · undo` with a link, `Forgot "<title>" · <scope> ·
  <id> v<n>`, `<title> · <id> v<n>`); the Remembered line does not show the item's id: read it from HTTP.
- **D**: the sidebar's top entries **Memory** and **Projects** (`#/memory`, `#/projects`), the badge
  on **Memory** (what waits for the Captain), and the group dialog. D5's conversation starts in
  `<run>/memrepo-new`, so the page needs `--workspace <run>/memrepo-new` too.
- **O**: the right panel's **Session** tab (**GOAL**, **LEDGER**, **MEMORY · applies to this session**
  with **Open Memory**); O1 to O3 read it as the base skill says, O4's lines are in the transcript
  (the `undo` link of the Remembered line, the Goal note block). That is also W11.
- **H**: the permission card has **Once**, the Always label when offered, **Reject** with an optional
  message and, only while the review runs, **Hold for me**. Review lines: "Metacognition is
  reviewing…", the badge "Waiting for you" with the conclusion, "Relied on: …" and "Consequence: …",
  "Held for you: the review stopped.", "No review; your call."; outcomes "Allowed once by
  Metacognition.", "Denied.", "Review cancelled: you answered first.", and the error "Already
  started by Metacognition — it ran once." under the card. The top bar shows `<n> waiting` (the
  bell) with the sessions' list, a mute bell per session and, while the browser's permission is
  undecided, **Notify me when I am away**; the sidebar marks a waiting session with a dot. H8: the
  browser must have allowed notifications (record the permission); keep the page behind another
  window, since the page raises none while it is visible on that conversation. **Settings for H:**
  the gear, `permissions.review` to `on` (it applies to a new session): the sidebar's **New
  session** starts the conversation that H runs in. H7: reopen the page from the link `nav web`
  prints.

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
- **W11 Session tab.** The right panel's **Session** tab (the Captain opens the panel and picks it):
  PASS when the **GOAL**, **LEDGER** and **MEMORY** sections, the `undo` / `redo` of the Remembered
  line and the Goal note block equal O1 to O4 against HTTP, **Show more** expands a long
  objective (more than about 120 characters: the Captain sets one with `/goal`) and collapses it
  again, hovering a criterion shows its evidence, **Open Memory** goes to `#/memory`, and the
  panel updates by itself after a `/remember` and after the next goal check (no reload).
- **W12 Waiting list by keyboard** (during H8). PASS when the top bar's count is a button
  (`aria-expanded`), Enter or Space opens the list, each row opens its session, the mute bells are
  reachable with Tab and report `aria-pressed`, and a second press of the count closes the list
  (record whether Escape also does).
- **Order.** W8–W12 and D1–D10 run before W7.
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
