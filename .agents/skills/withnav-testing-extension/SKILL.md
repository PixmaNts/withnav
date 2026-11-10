---
name: withnav-testing-extension
description: VS Code extension checks of the WithNav self-test. Load from withnav-testing when the Captain tests from the VS Code chat.
---

# WithNav self-test — VS Code extension

Same protocol as `withnav-testing`. `<run>` is the run folder.

## Hooks in the shared scenarios

- **V1**: ask for the expected extension version during the briefing. The Captain reads the
  installed version in the Extensions view; compare it separately from `nav --version`. The
  extension may run its bundled `nav`: if you cannot verify which binary it launched, mark the
  running server's identity unverified rather than inferring it from PATH.
- **P**: cards show Allow (accept once) and Reject. An Always choice is conditional and must be
  absent on P1/P2's credential-file questions.
- **I1 / M1**: a stored image shows as a thumbnail; a click opens it beside the chat.
- **C1 / C2**: paste the image into the composer, then send. Test file drop separately below:
  success through paste does not establish that drop works.
- **S5**: the key icon in the conversation header.
- **A1**: the subagent chip or the dropdown of the agent strip opens the child's transcript.
- **After A2**: wait for the second park, then the Captain clicks the child's Continue button.
  PASS (record as **E9**) when it resumes and its next notice still reports top-ups used 1/3. Then
  run A3 on that park.

## Client checks

- **E1 Image limits.** The Captain pastes `img1.png` … `img9.png`, either as a batch or one by one
  without sending (record which). PASS when eight are listed and an error names the one not
  attached (which one depends on the input order).
  They remove every pending image, then add `big.png` alone: PASS when it is refused as too large
  and not listed.
- **E2 Refused uploads.** The Captain attaches `notimage.png` with `img1.png` and sends "E2".
  PASS when the refusal names `notimage.png`, the invalid file never reaches the message, and
  either nothing is sent with the draft kept, or only `img1.png` reaches you; record which.
  Repeat with `empty.png`, including the sequence "empty file alone, then valid + empty together".
  A valid image completing its read must not erase another file's refusal. Confirm the received
  image count from the message's image blocks; thumbnails in the composer alone do not prove
  what was sent. If that evidence is unavailable, record the uncertainty rather than claiming
  the empty file was stored.
- **E3 Only the sent batch leaves.** The Captain attaches `img1.png` and `img2.png` and sends "how
  many images?" (PASS on two), then sends "and now?" with nothing attached (PASS on none). Smoke
  check: tiny images load before Send, so the wait-for-reads path is not forced.
- **E4 First message of a new conversation.** You cannot see another conversation, so the Captain
  reports: `WithNav: New Conversation`, attach `ok.png`, send "how many images in this message?
  answer with a number", then send "and in this one?" with nothing attached. They come back to this
  tab and give both answers. PASS on 1 then 0. They may close the new conversation.
- **E5 Thumbnails by keyboard.** On I1's and C1's thumbnails: Tab to it, Enter or Space. PASS when
  each opens in an editor group beside the chat and the focus outline stays visible. Distinguish
  an editor group in the same VS Code window from a separate OS window; record the latter as a
  placement mismatch even if keyboard activation worked.
- **E6 Pickers.** Record the shortcuts shown by the installed build. Use the advertised favourite
  shortcut twice on a highlighted agent, then twice on a highlighted model. Use the advertised
  group-folding shortcut twice in the model picker. PASS when each action toggles once per press,
  focus stays in the picker, and no VS Code search, editor folding, or other host action runs.
  The corrected build advertises Ctrl+Alt+F for favourites and Ctrl+Alt+A for model groups.
  Check that plain Ctrl+F and Alt+A no longer toggle picker controls, and that AltGr typing is
  not captured. On macOS record whether VoiceOver uses Control+Option before trying those keys.
- **E7 Tabs.** With two conversation tabs open, the Captain closes the one that is not this test,
  then resizes the window. PASS when this composer resizes and no error appears. Deleting a
  conversation is not tested: a refused delete cannot be forced from here, and a real delete would
  remove the Captain's data.
- **E8 Reveal.** Find a file and a function in it (`grep`), then `reveal` with `path` and `symbol`.
  Ask the Captain to observe the cursor without clicking a line (a click would move it). PASS
  when the file opens with the cursor on the function. If focus makes that unverifiable, record
  SKIP with the reason instead of retrying as an unexplained PASS. Then `reveal` a short file
  with `path` and a `line` past its end, no `symbol`: PASS when it is refused, names the line
  count, and no tab opens.
- **E10 MCP App reload.** If M2 ran: the Captain reloads the app panel twice, then uses the same
  control once. PASS when the tool ran once (the server's log, or a counter the app shows).
- **E12 Image file drop.** The Captain drags `ok.png` from the file manager onto the composer,
  then sends "E12". PASS when it appears as one pending image and reaches you once. Record the
  source application. A working paste is not a substitute; if drop fails, keep this FAIL and
  continue the remaining checks through paste.
- **E13 Session controls.** The Captain runs `WithNav: Show Sessions`, searches for a known
  conversation, and compares All with Waiting for you. PASS when the list opens, search finds
  the conversation, and the filters agree with its observed state. Close the list without deleting.
- **E14 Font size.** The Captain records `withnav.chat.fontSize`, changes it to another valid
  value, then restores it. PASS when the chat text updates both times without reloading.
- **E15 Image chooser.** The Captain uses **Attach images**, selects `img1.png` and `empty.png`,
  then types in the composer. PASS when only `img1.png` is pending and the refusal still names
  `empty.png` after typing and sending. They dismiss that refusal explicitly. Open the chooser
  again and cancel: PASS when the draft is unchanged and Send unlocks. The paperclip is editor
  context, not this chooser; verify it still offers editor context separately.
- **E11 Reload.** The Captain runs `Developer: Reload Window` and reopens this conversation. PASS
  when the thumbnails of I1 and C1 show again. Write the report before this check and append its
  row when the Captain is back.
