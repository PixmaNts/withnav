# Per-call permission review

Date: 2026-10-04

Status: Accepted

## Decision

Review every enabled bash, write and edit call before dispatch, and native read/grep/glob calls
outside readable roots or targeting credentials. Ordinary inside-root reads remain ungated.
Approvals bind a canonical target, re-resolved before no-follow dispatch; no root is added.
The instinct answers effects, scope and risk together. Non-risky read-only calls and low-risk project changes may run; risky or
outside-project changes require synchronous metacognition approval. Any uncertainty or reviewer
failure asks the driver. Cancellation interrupts rather than creating a prompt.

Sensitive credentials and keys deterministically force risky review, followed by a typed driver
decision even if metacognition approves. Risky read-only bash calls never auto-run on the instinct's
word. Reading ordinary Navai config follows read review; `auth.json` is sensitive. Config mutations
remain driver-only.

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
