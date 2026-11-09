---
name: withnav-testing-extension
description: VS Code extension checks of the WithNav self-test. Load from withnav-testing when the Captain tests from the VS Code chat.
---

# WithNav self-test — VS Code extension

Same protocol as `withnav-testing`. `<run>` is the run folder.

## Hooks in the shared scenarios

- **V1**: the extension may run its bundled `nav`, not the one on the PATH. The Captain reads the
  extension's version in the Extensions view; record it next to `nav --version`.
- **P**: questions are cards with Once / Always / Reject buttons.
- **I1 / M1**: a stored image shows as a thumbnail; a click opens it beside the chat.
- **C1 / C2**: paste or drop the image into the composer, then send.
- **S5**: the key icon in the conversation header.
- **A1**: the subagent chip or the dropdown of the agent strip opens the child's transcript.
- **After A2**: wait for the second park, then the Captain clicks the child's Continue button.
  PASS (record as **E9**) when it resumes and its next notice still reports top-ups used 1/3. Then
  run A3 on that park.

## Client checks

- **E1 Image limits.** The Captain selects `img1.png` … `img9.png` together. PASS when eight are
  listed and an error names the one not attached (which one depends on the order the picker gives).
  They remove every pending image, then add `big.png` alone: PASS when it is refused as too large
  and not listed.
- **E2 Refused uploads.** The Captain attaches `notimage.png` with `img1.png` and sends "E2". PASS
  when the refusal names `notimage.png` and either nothing is sent with the draft kept, or only
  `img1.png` reaches you; record which. Same with `empty.png`.
- **E3 Only the sent batch leaves.** The Captain attaches `img1.png` and `img2.png` and sends "how
  many images?" (PASS on two), then sends "and now?" with nothing attached (PASS on none). Smoke
  check: tiny images load before Send, so the wait-for-reads path is not forced.
- **E4 First message of a new conversation.** You cannot see another conversation, so the Captain
  reports: `WithNav: New Conversation`, attach `ok.png`, send "how many images in this message?
  answer with a number", then send "and in this one?" with nothing attached. They come back to this
  tab and give both answers. PASS on 1 then 0. They may close the new conversation.
- **E5 Thumbnails by keyboard.** On I1's and C1's thumbnails: Tab to it, Enter or Space. PASS when
  each opens beside the chat and the focus outline stays visible.
- **E6 Pickers.** In the agent picker the Captain searches and presses Ctrl+F on a highlighted
  agent. PASS when it becomes (or stops being) a favourite. The model picker's hint mentions Ctrl+F.
- **E7 Tabs.** With two conversation tabs open, the Captain closes the one that is not this test,
  then resizes the window. PASS when this composer resizes and no error appears. Deleting a
  conversation is not tested: a refused delete cannot be forced from here, and a real delete would
  remove the Captain's data.
- **E8 Reveal.** Find a file and a function in it (`grep`), then `reveal` with `path` and `symbol`.
  PASS when the file opens with the cursor on the function. Then `reveal` a short file with `path`
  and a `line` past its end, no `symbol`: PASS when it is refused, names the line count, and no tab
  opens.
- **E10 MCP App reload.** If M2 ran: the Captain reloads the app panel twice, then uses the same
  control once. PASS when the tool ran once (the server's log, or a counter the app shows).
- **E11 Reload.** The Captain runs `Developer: Reload Window` and reopens this conversation. PASS
  when the thumbnails of I1 and C1 show again. Write the report before this check and append its
  row when the Captain is back.
