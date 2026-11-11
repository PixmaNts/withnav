# Post-analysis: improve the self-test from this run

Run after the report and cleanup. You turn what this run taught into proposed changes to the
self-test skills. You propose; the Captain decides. Do not edit the skill files during the run:
an edited skill asks for validation again, and a change made mid-run would make its own report
inconsistent.

## 1. Classify every non-PASS

For each FAIL and SKIP of the report, give its cause, one of:

- **product**: WithNav did not do what the check expects, and the check matches the docs
  (`book/src/`, `docs/SPEC.md`, CHANGELOG). This stays in the report as a bug; quote the doc line.
- **skill**: the check was wrong, ambiguous or impossible as written (wrong expected text, a step
  the Captain could not do, an order that broke the run, a question the Captain had to interpret).
- **environment**: a missing prerequisite (no MCP server, no clipboard tool, text-only model).
- **model**: the model did not follow the workload (batched calls, ended early).

Also list every PASS where you or the Captain hesitated: a PASS reached by interpretation is a
**skill** finding too.

Done when every non-PASS has exactly one cause and one line of evidence.

## 2. Find what the run did not test

- Read the `Unreleased` section of `CHANGELOG.md` (and the latest released section when testing a
  release). For each user-visible entry, name the check ids that exercise it, or **none**.
- Note behaviour you met during the run that no check asserts (an error message, a prompt, a UI
  state the Captain mentioned).

Done when every user-visible CHANGELOG entry maps to check ids or to **none**.

## 3. Propose changes

For each **skill** finding and each **none** worth testing, write one proposal:

```
### <new id or existing id> — <fix | new check | remove | reorder>
Skill: withnav-testing | withnav-testing-tui | withnav-testing-extension | withnav-testing-web
Why: <evidence from this run, or the CHANGELOG entry it covers>
Cost: <Captain actions, minutes, model steps>
Text:
<the check exactly as it should read in the skill, same style: id, action, PASS condition>
```

Keep a proposal only when the Captain can run it interactively at reasonable cost and its PASS
condition is observable: a tool output, an error text, or one closed question to the Captain.
Shared behaviour goes in `withnav-testing`, client-specific behaviour in its client skill. A new
fixture goes into `fixtures.py` with a line in its docstring. Prefer fixing a check over adding
one.

Done when every **skill** finding has a proposal or a stated reason for none.

## 4. Hand over

Append a `## Post-analysis` section to the report: the cause table (step 1), the coverage map
(step 2), the proposals (step 3). Then ask the Captain, with `ask`, which proposals to apply. Apply
only those, in a later turn after the run, to the skill files named in each proposal, and show the
diff.
