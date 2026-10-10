# Per-call permission review

Date: 2026-10-04

Status: Accepted

Naming status: the human role is now called the Captain and the product WithNav;
see [ADR 0014](0014-rebrand-withnav.md). Original decision wording is retained.

## Decision

Review every enabled bash, write and edit call before dispatch, and native single-file read calls
outside readable roots or targeting credentials. Only single-file `read` gets outside-root review;
outside `grep`/`glob` stay refused. Ordinary inside-root reads remain ungated, with canonical
no-follow dispatch even without review. Searches skip sensitive entries before opening and report
a count; directory grep uses ripgrep with pre-open case-insensitive exclusions generated from the
same sensitive-name rules as the gate. The missing-ripgrep fallback uses `ignore::WalkBuilder` on
a bounded cancellable blocking worker, preserving ignore and hidden rules.
Approvals bind a canonical target, re-resolved before no-follow dispatch; no root is added.
The instinct answers effects, scope and risk together. Non-risky read-only calls and low-risk project changes may run; risky or
outside-project changes require synchronous metacognition approval. Any uncertainty or reviewer
failure asks the driver. Cancellation interrupts rather than creating a prompt.

Sensitive native reads deterministically force risky review, followed by a typed driver
decision even if metacognition approves. Risky read-only bash calls never auto-run on the instinct's
word. Reading ordinary Navai config follows read review; `auth.json` is sensitive. Config mutations
remain driver-only.
Bash is not confined. Sensitive-file detection for bash is best effort: supported literal paths
are canonicalized and recognized ambiguous sensitive expansions tighten risk, but arbitrary
scripts or indirect shell operands can evade classification. Only native file tools enforce the
deterministic sensitive-path rule. In-root search walks reopening descendants by path remain a
follow-up requiring descriptor-relative traversal.

The plan role question shares the instinct request when needed. Markdown exempts only that
role question, never safety review. Fetch retains its origin grants.

## Why

A folder approval is too broad for commands with indirect effects. Reviewing the exact call
keeps the driver's authorization narrow while permitting understood routine work.

## Rejected

Saved bash and file grants no longer bypass review. Reviewer silence is not approval;
metacognition remarks cannot serve as permission decisions. Deterministic checks only tighten
model classifications.

## Consequences

`permissions.automatic_review` defaults to true; false asks for each call. Old clients' folder
answers are normalized to one call. Persisted grants remain readable without migration.

The review is advisory, not a sandbox. Bash retains the process's ambient permissions, and a
mistaken classification remains a residual risk. Approval never widens writable roots.

## Amendment — 2026-10-10

Confident read-only effects are decided by Instinct before task-fit and workspace checks, below
sensitivity 0.5 and without deterministic credential, mutation, overwrite, removal, destructive,
opaque-execution or executable-identity floors. Unknown plain programs and read-filter pipes are
not grounds for review alone; fd duplication and redirects to `/dev/null` are not writes.
Metacognition timeout retains a floor-free read-only verdict below normal sensitivity; writes ask.
Captain intent is an ordered conversation, including untrusted assistant replies and selected skill
metadata, rather than a keyword-reset task anchor. Rules appear once; Instinct budgets from the
configured context window (`tokens * 3 / 2` bytes, fallback 16 KiB), preserving the latest Captain
turn or marking the request incomplete. Metacognition retains 64 KiB; the plan guard is unchanged.
Typed edit/write allow-once precedents bind canonical file and scope until the next Captain turn,
never sensitive or protected-config paths, and never restored from durable history.
