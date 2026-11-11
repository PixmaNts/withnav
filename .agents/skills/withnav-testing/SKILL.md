---
name: withnav-testing
description: Acceptance self-test of an installed WithNav build, run by the agent with the Captain at the client. Load when the Captain asks to test, validate or smoke-test WithNav itself, a release or a pre-release; then load the client skill (withnav-testing-tui, withnav-testing-extension or withnav-testing-web).
---

# WithNav self-test

You test the WithNav you are running in, end to end, with the Captain as your eyes and hands on the
client. You drive; the Captain observes what only a human sees (a card, a thumbnail, a focus ring),
answers prompts, and hands you images. Every check ends with a verdict backed by evidence.

## Protocol

1. Ask the Captain which client this is (terminal, VS Code, browser) if they did not say, then load
   the matching skill: `withnav-testing-tui`, `withnav-testing-extension` or `withnav-testing-web`.
   Its **hooks** run inside the shared scenario they name; its **client checks** run after the
   shared scenarios.
2. **Brief the operator first.** Explain the checks in order, the Captain's actions, the tools
   you will use, and the run folder and cleanup. Ask which version and, for a branch build, which
   commit they expect. Wait for the Captain's "go" before version commands or fixture creation,
   and before touching any workspace file, secret or permission.
3. **V1 Version.** Run `command -v nav` and `nav --version`; record both. PASS only when the
   agreed package version and, when supplied, commit match. A branch build reads
   `<package version>-<normalized branch>+<sha>`: the branch name alone does not prove its package
   version. For example, `0.8.2-release-v0-8-3+<sha>` is still package version `0.8.2`, not `0.8.3`.
   Record a dirty suffix and what is unverified; it is not evidence of a clean expected commit.
   The client skill says how to read the client's own version, which can differ from PATH.
4. Pick a run id: `date +%Y%m%d-%H%M%S`. The run folder is `withnav-selftest-<run id>` at the
   workspace root. Create the fixtures with
   `python3 <this skill's base_dir>/fixtures.py withnav-selftest-<run id>` (the `skill` tool's
   answer gives `base_dir`). The script refuses an existing folder; its docstring says what each
   file is for.
5. Run the shared scenarios in order, then the client checks. One scenario at a time. Before each
   scenario, tell the Captain, in order: (a) what the Captain must do or watch, (b) the exact text
   to type or click, (c) what you will do next. Only then act. After the act, collect the
   observation. Never act first and ask afterwards.
6. Collect every human observation with the `ask` tool, as a closed question with options
   (`Yes` / `No` / `Not shown`), one observation per question.
7. A verdict is **PASS**, **FAIL** or **SKIP** (prerequisite missing, or the Captain chose to skip).
   FAIL quotes the evidence: the tool output, the error text, the Captain's answer. A FAIL stays a
   FAIL; retry only after an obvious slip of yours, and report both runs. A check that depends on the
   model's behaviour (a subagent that stops early, a batched call) is **SKIP — invalid run**, not a
   product FAIL.
8. Finish with the report, then clean up.
9. Once the client's last check (if it has one) is in the report, read `post-analysis.md` in this
   skill's `base_dir` and follow it to turn the findings into proposed changes to these skills.

Touch only what this run created: the run folder, the report, the test secret. Leave every other
file, secret, rule and conversation as it was.

## Shared scenarios

`<run>` stands for the run folder.

### P — Permissions

Credentials always ask the Captain, whatever their rules (fixed check), and **Always** is never
offered for them: a `.env` file gives a question on every run, unless yolo is on (then P1 and P2
are SKIP) or a rule of the Captain's denies it (record the denial and SKIP).

- **P1** Before acting, ask the Captain to **Reject** the coming question with the message
  `rejected for the test: answer BANANA`. Then `write` `KEY=placeholder` to `<run>/.env`. PASS when
  the rejection reaches you with the message the Captain typed (the relay is exact; a typo of theirs
  is not a FAIL — quote what you received).
- **P2** Ask the Captain to accept once (**Once** or **Allow**, depending on the client), then
  repeat the call. PASS when it runs. Ask whether **Always** was offered on either question:
  FAIL if it was.
- **P3** `write` any text to `/tmp/withnav-selftest-<run id>.txt`. PASS when it runs without a
  question (writes to the temp folder run unasked unless a rule of the Captain's asks or denies;
  if one did, record it and SKIP).

### I — Images from tools

Run `read` on each fixture and report what you got: an image, text, or a note.

| Id | File | Expected |
|---|---|---|
| I1 | `ok.png` | An image. With a model that takes images, describe it: a blue square top-left on red. Without, a note that an image was shown counts as PASS. The Captain sees it (client skill: thumbnail or numbered line). |
| I2 | `drawing.svg` | An image (rendered to PNG): a blue circle on yellow. |
| I3 | `empty.svg` | An image, not text. |
| I4 | `fake.svg` | The model receives its text, no image. The client need not display the read result. |
| I5 | `truncated.png` | The call succeeds with a note `[image … not kept: …]` naming a corrupt or truncated image; nothing shown. |
| I6 | `corrupt.png` | Same as I5 (the note names the damaged data). |

### C — Images from the Captain

The client skill says how to attach. **C1**: the Captain sends `ok.png` with "what do you see?";
PASS when you describe the blue square on red (or, with a text-only model, receive the note that an
image was shown). **C2**: one picture of their own. Limits and refusals are client checks.

### S — Secrets

The test secret is `NAV_SELFTEST_KEY` (names starting with `WITHNAV_` are reserved and refused).
Act on that name only.

- **S1** `request_secret` with `name: "NAV_SELFTEST_KEY"`, `label: "Self-test key"`,
  `reason: "WithNav self-test"`. The Captain enters a throwaway value of at least 8 characters
  (e.g. `wnst-` plus random letters), availability 1 h, scope this session. PASS when the tool
  answers `stored as $NAV_SELFTEST_KEY …`.
- **S2** `bash`: `test -n "$NAV_SELFTEST_KEY" && echo present`. PASS on `present`.
- **S3** `bash`: `printf '%s\n' "$NAV_SELFTEST_KEY"`. PASS when the output reads
  `[secret NAV_SELFTEST_KEY]`, and the Captain confirms the tool row on screen shows that too.
- **S4** Ask the Captain to send the value in a message. PASS when you receive
  `[secret NAV_SELFTEST_KEY]` instead of the value, and the Captain confirms their message in the
  transcript shows the placeholder.
- **S5** The Captain opens the secrets list (client skill says where) and finds the
  `NAV_SELFTEST_KEY` row: scope session, the last four characters of their value, a use count
  above 0. They revoke that row only; you rerun S2. PASS when it now prints nothing.
- **S6** `request_secret` again (same arguments); the Captain declines. PASS when the tool answers
  that the Captain declined.
- **S7 (optional: writes the secrets file)** `request_secret` again; the Captain chooses scope
  this workspace, 1 h. Then `bash`:
  `stat -c '%a %n' ~/.local/share/withnav/credentials ~/.local/share/withnav/credentials/secrets.json`
  (Linux; the path names credentials, so it asks: the Captain answers Once). PASS on `700` and
  `600`. The Captain revokes the row; rerun `stat`: PASS when the file is gone or no longer lists
  the name (do not read the file's content). Persistence across a restart is not covered here.

### A — Subagents and the step budget (optional: about 150 subagent steps)

Ask the Captain before running it; it runs on the model assigned to `mousse`.

- **A1** `task` with `agent: "mousse"` and this prompt: "Self-test workload. In every response,
  make exactly one tool call: bash `true`. Never batch calls, never stop on your own, never write a
  final answer: keep going until you are stopped." PASS when you receive one notice that it used its
  steps, with top-ups used 0/3 and its last tool call. Record the step count it reports (50 unless
  `WITHNAV_MAX_STEPS` is set). The Captain opens the child's read-only transcript (client skill)
  and confirms it shows the steps.
- **A2** `decide_subagent` with `child_id` from the notice and `decision: "continue"`. PASS when it
  resumes and, one budget later, one new notice reports top-ups used 1/3.
- Client hook: the Captain's own continue (client skill), if it has one, runs here.
- **A3** `decide_subagent` with `decision: "stop"`. PASS when its result arrives once, marked
  stopped at its step budget, with its partial output.

If the child ends or batches before a park, record A as SKIP — invalid run.

### M — MCP (SKIP when no MCP server is configured)

- **M0** Run `nav doctor`. PASS when each configured MCP server is listed with its state; an HTTP
  server (`"type": "http"`) shows whether it was reached and its tool count. Compare with the
  `mcp__*` tools you have.
- **M1** Call a tool that returns an image or a screenshot. PASS when you get the image and the
  Captain sees it (client skill says how), or you get a note naming a limit and nothing is shown.
- **M2** If a tool offers an app (**Open App**; browser and VS Code), the Captain opens it and uses
  one control that calls the server. PASS when it works.
- **M3** (SKIP when no server offers prompts) The Captain runs `/<server>:<prompt>`, with its
  arguments if any. PASS when its messages arrive as your message inside a block marked as the
  server's untrusted text.

## Report

Write `withnav-selftest-<run id>-report.md` at the workspace root (outside the run folder), then
show it in chat:

```
WithNav self-test — <client> — <nav --version> (<command -v nav>) — <date>

| Id | Check | Verdict | Evidence |
|----|-------|---------|----------|
```

One row per check id, then a short list of FAILs with how to reproduce. Evidence is one line: the
quoted output, error or Captain answer. End with what this run does not cover: races the client
checks only smoke-test, persistence of workspace secrets across a restart, MCP App error statuses.

## Cleanup

`rm -r <run> /tmp/withnav-selftest-<run id>.txt`; it names `<run>/.env`, so it asks: the Captain
answers Once. Ask the Captain to revoke `NAV_SELFTEST_KEY` if it
is still listed. A client check that must run last (the client skill says so) comes after this.
