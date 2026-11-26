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
   and before touching any workspace file, secret or permission. When groups N to H will run, add
   the settings the Captain will change and restore, the register entries the run creates (they
   cannot be deleted) and the files you will read under the data directory.
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
   FAIL; retry only after an obvious slip of yours, or the accidental acceptance in P1 described
   below, and report both attempts. A check that depends on the model's behaviour (a subagent that
   stops early, a batched call) is **SKIP — invalid run**, not a product FAIL.
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
  is not a FAIL — quote what you received). If the Captain accidentally accepts instead of rejecting,
  record the first attempt and allow one explicitly labelled retry; report both attempts.
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
  (e.g. `wnst-` plus random letters), availability 1 h, scope this session. Ask the Captain to
  confirm the selected availability and scope without revealing the value. Compare the expiry in the
  response with the call time, allowing for clock and interaction delay. PASS when the tool answers
  `stored as $NAV_SELFTEST_KEY …`, session scope and an expiry consistent with 1 h. If the selection
  or time cannot be verified, record it as unverified; report a verified mismatch as FAIL.
- **S2** `bash`: `test -n "$NAV_SELFTEST_KEY" && echo present`. PASS on `present`.
- **S3 / S4** SKIP output and chat redaction unless a separate safe test interface is available
  and explicitly authorized. Never print a secret or ask the Captain to send its value in chat.
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
- **M0-health** Record `nav doctor`'s exit status and each failed check separately from the MCP
  verdict. PASS when it exits successfully with no failed checks; FAIL with the failed check and
  its output otherwise.
- **M1** Call a tool that returns an image block or structured preview. Record separately whether
  you received image bytes and whether the Captain saw a thumbnail or board preview. PASS only for
  the capability observed; a text response does not prove an image block was received. A note
  naming an image limit with nothing shown also counts as PASS for the limit.
- **M2** If a tool offers an app (**Open App**; browser and VS Code), the Captain opens it and uses
  one control that calls the server. PASS when it works.
- **M3** (SKIP when no server offers prompts) The Captain runs `/<server>:<prompt>`, with its
  arguments if any. PASS when its messages arrive as your message inside a block marked as the
  server's untrusted text.

## Records, settings and the server API (groups N to H)

Groups N to H read WithNav's own files and HTTP routes. State what you will read in the briefing.

```sh
D=${XDG_DATA_HOME:-$HOME/.local/share}/withnav; O=$D/server/owner.json
api() { m=$1; p=$2; shift 2; curl -sS --noproxy '*' -w '\nHTTP %{http_code}\n' -X "$m" \
  -H "Authorization: Bearer $(jq -r .token "$O")" -H 'Content-Type: application/json' \
  "http://127.0.0.1:$(jq -r .port "$O")$p" "$@"; }
```

- `owner.json` holds the server token, so a call that names it is a credential read and asks: the
  Captain answers Once. The token is used inside `api` and never printed. Put all the HTTP checks of
  a scenario in one `bash` call, so one question covers them.
- Files under `$D`: `instinct-calls.jsonl`, `goal-checks.jsonl`, and per workspace
  `flows/runs/<workspace hash>/runs.jsonl` and `scores.jsonl` (the hash folder that changed after
  your call; `ls -t` finds it). Before a step, note `wc -l` of the file; afterwards read only the
  new lines (`tail -n +<n+1>`). Quote a field, never a whole line: a capture holds payloads.
- Settings (`~/.config/withnav/config.json`: `layers.instinct`, `goal_watch.notes`,
  `permissions.review`, `agent.watchdog_interval_secs`) are changed by the Captain in the client's
  Settings screen, never by you: it is the Captain's configuration and a write to it is protected.
  Before the first change, read the current values and record them; restoring them is part of
  Cleanup. You confirm a change by reading the file back with `jq`.
- The id of this conversation: `api GET /v1/sessions`, take the newest session of this workspace,
  and confirm it with the Captain by its title (the client skill says where the Captain sees it).
- Items of the register are never deleted: every test item ends invalidated, not removed. Name
  every item, group and note with `WNST-<run id>` so it can be told apart afterwards.

A group is SKIP, naming the prerequisite, when its layer is off (`nav doctor --layer instinct`,
`--layer metacognition`) or the setting cannot be changed from the client.

### N — Instinct rejection and call records

Preconditions: Instinct assigned and answering (`nav doctor --layer instinct` is green); the
current `layers.instinct` recorded. N1 to N2 need a model of the catalogue that uses the
`systemone` API and that the gateway refuses with a 4xx (a model id it does not serve). Ask the
Captain for one; without it, SKIP N1 and N2 as environment.

- **N1 Rejection notice.** The Captain sets the Instinct model to the refused one in Settings; a
  layer model applies after WithNav restarts, so they run `nav serve stop` (no turn running; the
  client skill says how to come back), reopen the client and resume this conversation.
  `bash` `ls <run>`, then `bash` `ls <run>/doomed`. PASS when the Captain sees one notice, "instinct
  rejected", naming the status (an HTTP 4xx) and the provider's reason on one line of at most 300
  characters, no credential in it, and no second notice after the second call, and the notice is not
  the "instinct unavailable" one. Each call went on as the Captain's rules say (it ran, or asked).
  Read-back: the newest `permission-default` run in `runs.jsonl` is failed and carries
  `provider_status` (the 4xx); `nav doctor --layer instinct` shows "HTTP 4xx rejected" and, if the
  reason echoed a credential or a URL, only `[redacted]` / `[url omitted]`.
- **N2 No outage window, notice once more.** The Captain restores the Instinct model and restarts
  WithNav the same way. `bash` `ls <run>`. PASS when the call is judged at once (a new `permission-default` run with status ok
  in `runs.jsonl`, and a line in `scores.jsonl` whose `identity.model` is the restored model), with
  no wait. Then the Captain sets the refused model again, restarts, and you run one more call: PASS when the
  notice comes again, once (Instinct answered in between). The Captain restores the model and
  restarts a last time. Each swap costs a restart: the repeat is optional when time is short.
- **N3 Score lines.** After N2's good call: the new `scores.jsonl` line has `run_id`, `flow_name`,
  `definition_digest`, `identity.model`, `status`, `call_id`, `answers`, `actions`, and `projection`
  (2 once the consequence review is in the build); the file mode is `600`. Run a call whose
  arguments hold the marker: `bash` `echo WNST-<run id>`. PASS when `grep -c WNST-<run id>
  scores.jsonl` prints `0` (no argument, state or question text); `runs.jsonl` may hold it.
- **N4 Decision rows.** `nav permissions history --json`: the row of that `echo` call has a
  `flow_trace` listing, for each flow run, its run id, the Instinct model and the flow definition's
  digest. PASS when the run id, model and digest of the `permission-default` entry equal the
  `run_id`, `identity.model` and `definition_digest` of its `scores.jsonl` line; the protected
  check (credential-file call of P1, if it ran in this session) also names a model and a digest.
- **N5 Compaction records.** The Captain runs `/compact` in a conversation with enough text to be
  summarised (the shared scenarios made some). PASS when `instinct-calls.jsonl` gained a line with
  `caller` `prune` and this `session_id`, holding `ts`, `model`, `state_bytes`, `state_sha256` (64
  hex characters), `questions` (names), `answers`, `latency_ms`, and `error` null or one of
  `unavailable`, `invalid`, `cancelled`, `abandoned`; and `grep -c` of any word of the conversation
  in that line is `0` (the state is never stored). SKIP when compaction made no Instinct call
  because the text was too short: record what `/compact` answered.
- **N6 Watchdog records (optional: 3 minutes, a short turn the Captain waits for).** The Captain
  sets `agent.watchdog_interval_secs` to `30` and starts a new conversation (it applies to new
  sessions); run `bash` `sleep 100` (the Captain accepts it).
  PASS when `instinct-calls.jsonl` has a line with `caller` `root_watchdog` for this session, and
  `latency_ms` set. The Captain restores the value. A `child_watchdog` line appears the same way
  after A1 when it lasted past `subagents.watchdog_interval_secs`: record it, never fail on its
  absence.

### G — Goal watch

Preconditions: Metacognition answering; at most 8 checks in the run (the limit is 10 an hour per
conversation; a skipped check publishes nothing, so record it as SKIP — invalid run); the value of
`goal_watch.notes` recorded and set to `false` by the Captain for G1 to G4. A check runs beside the
turn and takes up to 60 s: after each turn that should start one, poll the ledger (below) every 15 s
for up to 90 s before you conclude it did not run. The Captain sets the goal with `/goal`; you
never can.

`goal_check` read: `api GET /v1/sessions/<id>/ledger | jq .goal_check` returns `null` when no check
has finished for the goal's current revision, else `{revision, turn_id, statuses[{index,status,
evidence}], claims[{claim,status,evidence}], note_sent, note}`; the line of the same check in
`goal-checks.jsonl` has the same `claims` and `note_sent` / `note_skipped`.

- **G1 Supported criterion and claim.** The Captain types `/goal WNST-<run id> goal watch` then
  `/goal criterion check-passes.sh exits 0` (`/goal` alone must show the objective and numbered
  criterion 1). `bash` `sh <run>/check-passes.sh`, then end the turn with exactly: "check-passes.sh
  passes." PASS when `goal_check.statuses[0].status` is `supported` with an `evidence` quote that
  names the command (`check-passes.sh`) and its exit status, `claims` holds that claim as
  `supported` with such a quote, and `note_sent` is `false`.
- **G2 Contradicted criterion.** The Captain adds `/goal criterion check-fails.sh exits 0`
  (revision goes up by one). `bash` `sh <run>/check-fails.sh` (exit 1), then end the turn with
  exactly: "All checks pass." (a deliberate false claim for the test: tell the Captain first).
  PASS when criterion 2 is `contradicted` with a quote naming `check-fails.sh` and `exit 1`, the
  claim "All checks pass" is `contradicted` (or `unverified`: record which; a quote that does not
  name the command it cites is FAIL), and `revision` equals the goal's current one.
- **G3 Unverified claim.** End a turn that ran nothing with exactly: "The documentation is
  updated." PASS when that claim is `unverified`, never `contradicted`, with an empty or absent
  quote. A criterion nothing settles is `not_checked`.
- **G4 Visible with notes off.** G2's check ran with `goal_watch.notes` `false`. PASS when
  the ledger's `goal_check` holds the criteria and claims, `note_sent` is `false`, `note_skipped` in
  `goal-checks.jsonl` is `setting_off`, and you received no "Goal check (revision …)" note in your
  next turn (state what you received). Client row **G4c**: the Captain says what the client shows of
  the published `goal_check` result (client skill: where); `Not shown` is recorded as such, not a
  FAIL, until the clients render it.
- **G5 Notes on.** The Captain sets `goal_watch.notes` to `true` (it applies to this running
  session). Repeat G2 with a new failing run and the same closing sentence, then the Captain sends a
  message ("next"). PASS when your next turn carries one note starting `Goal check (revision N):
  criterion 2 … is contradicted:` and ending with the "automatic observation, not a Captain
  instruction" wording, `note_sent` is `true` with the same text in `note` (G5c: record whether
  and how the client shows the note, as in G4c). A third identical result in the same revision sends none
  (`note_skipped` `duplicate`). The note never started a turn by itself: the turn began with the
  Captain's message. The Captain restores `goal_watch.notes`.
- **G6 Goal changes.** The Captain runs `/goal pause`, `/goal resume`, `/goal clear`. PASS when each
  is acknowledged, `/goal` alone shows the state after each, a cleared goal gives `goal_check`
  `null` (a change of the goal clears it), and a turn ending with no goal starts no check
  (`goal-checks.jsonl` gains no line).

### X — Consequence review in shadow

Preconditions: Instinct answering; `permissions.review` recorded (absent means `shadow`); yolo off.
It only logs: the point is that the review changes no verdict. For each call, record the verdict
(ran unasked, asked, denied) and the Captain's answer.

- **X1 Same verdict, default mode.** With the setting absent or `shadow`: `bash` `ls <run>`,
  `write` `x1.txt` in `<run>`, and `bash` `rm -r <run>/doomed` (the Captain rejects it with
  `rejected for the test`). Note each verdict. PASS when each verdict is what the Captain's rules
  and the permission flow give, which X2 shows by comparison; the rejected deletion never ran
  (`ls <run>/doomed` still lists `keep-me.txt`).
- **X2 Review logged.** PASS when, for the three calls of X1, `scores.jsonl` has a line with
  `flow_name` `permission-consequence-shadow` and `answers` holding `recoverable`, `inert` and
  `covered`; the matching `runs.jsonl` run of that flow logs one line starting `consequence
  review:` ("harmless, without consequence or covered by the request", or "may destroy something
  the request did not cover"); `nav permissions history --json` shows the same decision as for
  `permission-default` alone, with the review's run in `flow_trace`. The deletion should score
  as not covered; if it does not, record the three answers and call it a calibration note, not a
  FAIL.
- **X3 Off writes nothing.** The Captain sets `permissions.review` to `off` and starts a new
  conversation (a session reads the setting when it starts). Repeat the three calls of X1 (the
  Captain rejects the deletion again; `doomed/` is still there). PASS
  when the three verdicts equal X1's and no `scores.jsonl` or `runs.jsonl` line names
  `permission-consequence-shadow` for these calls.
- **X4 `on` behaves as shadow.** The Captain sets `on` and starts a new conversation; repeat the
  three calls. PASS when the verdicts equal X1's and the review lines are logged as in X2. Nothing
  waits on the review: the calls are not slower than in X3 by a visible pause (the Captain's
  impression, `Yes`/`No`).
- **X5 Unreadable value (optional).** The Captain types `Shadow` in the setting (if the Settings
  screen refuses it, record that and SKIP the rest). PASS when `nav doctor --offline` flags
  `permissions.review` and a new conversation runs no review (no new review line). The Captain
  restores the recorded value.
- **X6 Current request.** In a new conversation, the Captain sends: "Release 0.0.1 of memrepo: make
  the commit and push the tag" (nothing is pushed: `memrepo` has no remote, the call is inert).
  `bash` `git -C <run>/memrepo tag v0.0.1`. PASS when the `covered` answer of that call's review
  is higher than the `covered` answer of the same call after a new Captain message "just look at the
  files, change nothing" (record both numbers; the pair, not a threshold, is the evidence) and the
  verdict was the same in both. SKIP — invalid run when the model changed the request itself.

### R — Memory register

Preconditions: `<run>/memrepo`, `<run>/memrepo-wt`, `<run>/memrepo-clone` exist (SKIP the group when
fixtures reported git skipped); the conversation's workspace is `<run>/memrepo` (the Captain opens a
conversation there; the client skill says how), call it the **memory conversation**; its session id
from `api GET /v1/sessions`. `GET /v1/projects` shows one project whose `aliases` hold
`<run>/memrepo`; record its id as `P1` and name. The Captain types the slash commands; you read the
register through HTTP after each one (`api GET '/v1/memory?q=WNST-<run id>&status=active'`; an item
has `id`, `scope`, `scope_ref`, `status`, `current_version`, `current.{title,body}`). Every text
includes `WNST-<run id>`.

- **R1 Group for the group marker.** Ask the Captain (`ask`): "create group WNST-<run id> and add
  memrepo to it?". On Yes: `api POST /v1/groups -d '{"name":"WNST-<run id>"}'` (2xx, returns the
  group; note its id `G1`), then `api PUT /v1/projects/<P1>/groups/<G1> -d '{"confirm":true}'`.
  PASS when `GET /v1/projects` lists `memrepo` in the group and the history holds the addition.
  Then `/remember @group:no-such-group-<run id> text`: PASS when it is refused naming the group
  (record the exact message) and no item is created.
- **R2 Scope markers.** The Captain types, one by one:
  1. `/remember WNST-<run id> default scope`
  2. `/remember @project WNST-<run id> project scope`
  3. `/remember @session WNST-<run id> session scope`
  4. `/remember @global WNST-<run id> global scope`
  5. `/remember @group:WNST-<run id> WNST-<run id> group scope` 
  PASS when each answer reads `Remembered "<title>" · <scope> · <id> v1`, with scope `project
  <name>` for 1 and 2, `session` for 3, `global` for 4, `group WNST-<run id>` for 5; HTTP shows one
  item per text with `scope` project, project, session, global, group, `scope_ref` = `P1` for the two
  project ones, `current_version` 1, status `active`. Number 1 and number 2 are separate items.
- **R3 Words that look like markers.** `/remember global rule WNST-<run id> no at sign`, `/remember
  @global` (nothing after), and `/remember @unknown WNST-<run id> kept as text`. PASS when the first
  is stored in the project (not global) with a body that starts with `global rule`, the second is
  refused with the usage text and stores nothing, and the third is stored in the project with a
  body starting `@unknown`.
- **R4 Read it back.** `/memory` lists the active items you can read here (each line `<title> · <id>
  v<n>`: every active item made so far), `/memory search WNST-<run id>
  default`, `/memory show <id>`, `/memory history <id>`. PASS when the list matches HTTP's active
  items for this session, the search returns only the default-scope item, `show` shows its current
  wording and `history` lists `v1 · created`. A full id and a unique prefix of at least six
  characters both work; a five-character prefix is refused (record the message).
- **R5 Revise with a stale version.** Take item 1 of R2 (id `I1`). The Captain types `/memory revise <I1>
  v1 WNST-<run id> second wording`: PASS when it answers `v2`. Then, as the second client, you:
  `api POST /v1/memory/<I1>/revise -d '{"expected_version":1,"text":"WNST-<run id> stale"}'`. PASS
  when the status is `HTTP 409` with `reason` `version_conflict` and `current_version` `2`, and
  `GET /v1/memory/<I1>` still has exactly two versions, the current wording being the second. Then
  the Captain types the stale form `/memory revise <I1> v1 WNST-<run id> stale` too: PASS when it is
  refused and the history is unchanged. Last, you revise with `expected_version` 2: `HTTP 2xx`, v3.
- **R6 Invalidate, restore.** `/memory invalidate <I1> v3 WNST-<run id> test`: PASS when the answer
  shows the item as invalidated (v4), `/memory` no longer lists it, `/memory search` does not find
  it, `/memory show <I1>` still does and `history` lists v1 to v4 with the change names
  (`created`, `revised`, `revised`, `invalidated`). `/memory restore <I1> v4`: PASS when it is active
  again at v5 and listed again; versions 1 to 4 are unchanged (read them back).
- **R7 Forget.** `/forget <item 2's id> WNST-<run id> done`. PASS when it answers `Forgot "<title>"`,
  the item is `invalidated`, and its history gained an invalidation with the reason. `/forget` with
  no id is refused with the usage text.
- **R8 Linked worktree and clone.** With item 2 forgotten, make a fresh project item: `/remember
  @project WNST-<run id> shared by worktrees`. Then, from your shell and without model calls:
  `nav run --workspace <run>/memrepo-wt -p "/memory"` and the same with
  `--workspace <run>/memrepo-clone` (record the output format; if `nav run` cannot run a command
  without a model, SKIP R8 and say so). PASS when the worktree's list holds `shared by worktrees`
  and the clone's does not, nor does it list the project items of memrepo; `GET /v1/projects`
  shows memrepo and memrepo-wt as aliases of one project id and memrepo-clone as another.
  The session-scoped item (R2.3) shows in neither run.
- **R9 The agent's scope tools wait for the Captain.** `GET /v1/projects` for the id `P2` of
  memrepo-clone (created by R8). Call `project_group_add` with `project_id` `P2` and `group_id`
  `G1`. PASS when the result is a pending change ("Waiting for the Captain's confirmation.") and
  `GET /v1/projects` still lists no membership of `P2`; `api GET /v1/memory/needs-you` has one
  change (project_group, add, proposed by agent). Do the same for `session_attach` with `project_id`
  `P2`: pending, and this conversation's project link did not change. The Captain answers in Needs
  you (client skill): Reject the first, Accept the second, then you read back: rejected one left no
  membership and stays in the history as rejected; the accepted one now shows in `GET /v1/projects`.
  Neither tool changed what any other tool may do (record that no permission card appeared).
- **R10 Removal from another project is a proposal.** Now `P2` is in `G1`. `project_group_remove`
  with `P2` and `G1` while this conversation's active project is another: PASS when the result is
  pending, the membership is unchanged until the Captain accepts in Needs you, then gone with a
  history entry. Optional: if this conversation's own project is in `G1` (the Captain added it
  in D2), `project_group_remove` for it answers "Memory scope narrowed." and acts at once.
- **R11 Widening needs confirmation over HTTP.** `api PUT /v1/projects/<P2>/groups/<G1> -d '{}'`
  (no confirm) with `P2` out of `G1`. PASS when the status is `HTTP 409` with `reason`
  `widening_requires_confirmation` and `affected_projects`, and no membership appeared.

### D — Projects pages and the group dialog

Web only for the pages; a terminal or editor Captain opens `nav web --workspace <run>` for D1 to D4
and the other client for the rest. State that in the briefing.

- **D1 Projects page.** `#/projects`. PASS when the Captain sees the group `WNST-<run id>` with its
  member and item counts (member count includes `memrepo`; item count includes R2.5's item),
  `memrepo` with its paths (`memrepo` and `memrepo-wt` under one project) and its sessions, and
  `memrepo-clone` apart.
- **D2 Membership by the Captain.** The Captain adds this conversation's project to the group on the
  page. PASS when the page asks first because it widens who reads what, lists the projects
  affected, and after Confirm the project shows in the group and the history lists the change;
  removing it again is immediate and also in the history.
- **D3 Create and rename.** The Captain creates `WNST-<run id>-b` and renames it `WNST-<run id>-c`.
  PASS when both show on the page and `GET /v1/projects` has the final name (groups cannot be
  deleted: say so in the report).
- **D4 Session link.** The Captain detaches the memory conversation from `memrepo` and attaches it
  again. PASS when the link shows each time and the history keeps both changes.
- **D5 New-project question.** Preconditions: a repository the register does not know: `git init`
  `<run>/memrepo-new` yourself (one commit, identity as in fixtures: the throwaway repo is in the
  run folder). The Captain starts a conversation with that workspace. PASS when the dialog "Which
  group does `memrepo-new` belong to?" shows its path and the choices (each group, "No group", "New
  group"), and **Ask me later** closes it with `GET /v1/projects` showing `group_answered` `false`
  for it. A new conversation in the same folder asks again. Then answer "No group" or choose
  `WNST-<run id>-c`: PASS when `group_answered` is `true` and it never asks again. Items scoped
  to the group are described as shared with its projects, global ones everywhere.
- **D6 Needs you.** R9 gave the page its entries: PASS when the sidebar badge counted them before
  the answers and the list (`#/memory?view=needs`) never showed more than five, each row reading
  "Add <project> to group <group>" or "Attach session … to <project>", proposed by Nav in the
  session, with Accept and Reject; after both answers the badge is gone and the page says "Nothing
  needs you." The Captain's answer in one client left the other's badge stale for no longer than
  its next refresh (record the delay).
- **D7 Memory page.** `#/memory`: the Captain filters by scope, kind and status (active by default),
  searches `WNST-<run id>` (matches title, wording, quote, reason as typed), opens R2's item
  to see every version and ticks two to see the word-by-word difference. PASS when each shows
  what HTTP does.
- **D8 Edit conflict on the page.** The Captain opens the item and starts editing, without saving.
  You revise it by HTTP (`expected_version` = its current one). The Captain saves. PASS when the
  page shows the newer version, keeps the Captain's draft, and offers to apply it on top. Nothing
  was written by the refused save.
- **D9 Undo.** The Captain invalidates an item with a reason and presses **Undo** in the toast.
  PASS when a new version restoring the earlier state exists (history grew by two, none lost) and
  the item is active.
- **D10 Widening on the page.** The Captain changes an item's scope to global (or group) from the
  detail. PASS when the page lists the projects that would start to read it and asks to confirm;
  declining writes nothing.

### O — Session panel

What each client shows of this conversation's goal, ledger and memory; compare with HTTP
(`GET /v1/sessions/<id>/ledger`, `GET /v1/memory?session=<id>`). Run after G and R, in the
conversation where they ran.

- **O1 Goal and ledger (terminal: built).** The Captain reads the right column. PASS when it shows
  the goal before the ledger with its revision and status (`active`/`paused`) and criteria, and
  the ledger turns, files changed and repeated failures that equal the ledger JSON (for instance,
  `check-fails.sh` ran once: not a repeated failure; run it twice and it is one).
- **O2 Criteria status (pending in the web panel).** Each criterion shows its last check status
  (`supported`, `contradicted`, `not checked`) with the evidence on hover, and "last checked"
  relative time; they equal `goal_check.statuses`. Terminal and extension: record what is shown,
  and SKIP if nothing.
- **O3 Memory tab (pending).** The items sent to Nav this turn with authority tags and why they
  were picked, linking to `#/memory`. PASS when the items match the register's active items for
  the session (global, group, project, session scopes of R2) and none of the invalidated ones.
- **O4 Transcript lines.** `/remember` and `/forget` leave `Remembered "<title>" · <scope> · <id> v<n>`
  and `Forgot "<title>" …` lines in the transcript (built); list lines `<title> · <id> v<n>`, `No
  memory items.` for an empty list. **Pending:** the goal-check result and the goal note as quiet
  lines in the transcript, and "Proposed to remember" for items extracted from the conversation.
  PASS for the built ones when the Captain's reading equals the command's answer.

### H — Parallel review of a permission card (pending: needs `permissions.review: on`)

Not built yet. Enable these when `on` makes the review answer for the Captain; until then `on`
behaves as `shadow` (X4) and each H check is SKIP — not built. Preconditions: Metacognition and
Instinct answering, `permissions.review` `on`, a new conversation, a call the flow asks about and a
request that does not cover it. Calls here are inert or on `<run>` files only.

- **H1 Reviewing.** A card is up for `bash` `rm -r <run>/doomed`. PASS when the buttons stay active
  and a line "reviewing…" shows (Captain's observation), and the Captain can answer at once.
- **H2 Hold for me.** PASS when the card offers **Hold for me**, pressing it keeps the card open,
  and the review's answer cannot close it.
- **H3 Allowed by Metacognition.** With a call the request covers (`bash` `ls <run>`), the Captain
  leaves the card alone. PASS when it closes as "Allowed by Metacognition" with a 2 to 3 sentence
  reason, the call ran once, and a second call of the same kind asks again (allow once only, never
  an always rule: `nav permissions list` shows no new rule).
- **H4 Waiting for you.** A call beyond the request (the deletion) → the card stays open with a
  "Waiting for you" badge and the review's conclusion above the buttons (the request relied on and
  the consequence, not a long reasoning). PASS when the Captain sees both.
- **H5 Protected write.** `write` `<run>/.env` shows "Protected: only you can answer." and no
  review line.
- **H6 Steer during a review.** While a card is reviewing, the Captain sends a steering message.
  PASS when the earlier review is invalidated (the card does not close on its stale answer).
- **H7 Restart with a card open.** With a card open, the Captain restarts the server (`nav serve stop
  --force` is a stop of running turns: the Captain agrees first). PASS when after the restart no
  stale card is left allowed, and the history row says what happened (`cancelled` or `expired`).
- **H8 One notification per blocked turn.** With the client in the background, a blocked turn
  gives one notification "Nav is waiting for you in <session>: <short call>", a badge on the
  session and the top bar; answering the card in any client closes it everywhere. PASS when the
  count of notifications is one for the turn and none remain after the answer.
- **H9 Compound line.** `bash` `ls <run> && rm -r <run>/doomed; echo done`: the card splits the
  line into commands, shows the consequence in one line and highlights the part that caused the
  question; the full line stays visible; the call runs or is refused as a whole.

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
checks only smoke-test, persistence of workspace secrets across a restart, MCP App error statuses,
the redaction of a secret in a remembered text, and every pending check of group H. List the
settings you recorded and whether each was restored.

## Cleanup

`rm -r <run> /tmp/withnav-selftest-<run id>.txt`; it names `<run>/.env`, so it asks: the Captain
answers Once. Ask the Captain to revoke `NAV_SELFTEST_KEY` if it
is still listed. If groups N to H ran: the Captain restores every setting you recorded
(`layers.instinct`, `goal_watch.notes`, `permissions.review`, `agent.watchdog_interval_secs`) and
you read each back. Memory items cannot be deleted: each remaining one named `WNST-<run id>` is
invalidated with `/forget` (the ones you list from `GET /v1/memory?q=WNST-<run id>&status=active`).
Groups and projects created for the run, and the conversations `nav run` started, stay: list them
in the report by name and id. A client check that must run last (the client skill says so) comes after this.
