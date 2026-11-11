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
