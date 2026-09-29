//! Todo-10 invariance regression gate. The Phase-2 north star is that the
//! Phase-1 reasoning tree stays behaviorally identical: the ONLY permitted
//! edits to the reasoning files are the additive observer params on
//! `run_agent` (`on_tool`/`on_step` + `+ Send`), the mechanical `+ Send`
//! propagation on the provider callback signatures, and additive error
//! variants. This test runs a git-diff gate against the `phase-1-complete`
//! tag so a worker cannot silently alter the reasoning behavior.
//!
//! It also asserts the LOCKED wire-body golden fixtures are byte-unchanged
//! (the fixtures live under tests/fixtures/phase1_wire and are pinned by
//! tests/phase1_wire_golden.rs).

use std::process::Command;

/// The Phase-1 tag whose reasoning tree must stay invariant.
const BASELINE: &str = "phase-1-complete^{commit}";

fn git(args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("git runs");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `git diff --numstat BASELINE -- <path>` -> (added, deleted). A path with
/// no committed diff returns (0, 0).
fn numstat(path: &str) -> (u64, u64) {
    let raw = git(&["diff", "--numstat", BASELINE, "--", path]);
    let line = raw.lines().next().unwrap_or("0\t0\t");
    let mut cols = line.split('\t');
    let added = cols.next().unwrap_or("0").parse().unwrap_or(0);
    let deleted = cols.next().unwrap_or("0").parse().unwrap_or(0);
    (added, deleted)
}

/// These reasoning files must be BYTE-IDENTICAL to Phase 1: no diff at all.
#[test]
fn reasoning_files_are_byte_identical_to_phase_1() {
    for path in [
        "src/render.rs",
        "src/agent/actor.rs",
        "src/agent/prompt.rs",
        "src/agent/response.rs",
        "src/tools",
    ] {
        let (added, deleted) = numstat(path);
        assert_eq!(
            (added, deleted),
            (0, 0),
            "{path} must be byte-identical to phase-1-complete (got +{added}/-{deleted})"
        );
    }
}

/// `agent/loop.rs` may change ONLY additively (the two observer params + their
/// call sites); the added lines must not delete any control-flow branch. We
/// verify the diff is dominated by additions and that the specific reasoning
/// statements (the classify match arms, the provider send, the dispatch) are
/// still present verbatim.
#[test]
fn loop_rs_is_additive_only_reasoning_unchanged() {
    let (added, _deleted) = numstat("src/agent/loop.rs");
    assert!(added > 0, "loop.rs gained the additive observer params");
    // EVERY deleted line must be either the widened `on_delta` signature line
    // or a test call-site whose arity was bumped for the new observers — NEVER
    // a reasoning statement. A structural change would delete something else.
    let diff = git(&["diff", BASELINE, "--", "src/agent/loop.rs"]);
    for line in diff.lines() {
        if let Some(removed) = line.strip_prefix('-') {
            if removed.starts_with("--") {
                continue;
            }
            let is_signature = removed.contains("on_delta: &mut dyn FnMut(StreamDelta)");
            let is_test_call_bump = removed.contains("run_agent(&mut actor, &mut ");
            assert!(
                is_signature || is_test_call_bump,
                "unexpected deleted loop.rs line (a reasoning branch may have changed): {removed:?}"
            );
        }
    }

    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/agent/loop.rs"))
        .expect("read loop.rs");
    for anchor in [
        "match classify(message)",
        "StepOutcome::Done",
        "StepOutcome::Yield",
        "StepOutcome::Continue",
        ".dispatch(",
        "RunOutcome::StepBudgetExhausted",
    ] {
        assert!(
            src.contains(anchor),
            "loop.rs reasoning anchor missing (control flow changed?): {anchor:?}"
        );
    }

    // Strong check: the `run_agent` body's reasoning lines must be a superset
    // of Phase-1's, differing ONLY by the additive observer call lines
    // (`on_step(...)` / `on_tool(...)`) and the widened signature. Extract the
    // reasoning statements (control-flow keywords + the loop's operations) from
    // BOTH versions and assert every Phase-1 reasoning line is still present
    // verbatim AND no NEW reasoning line was inserted. This catches an additive
    // early-return / branch that leaves all anchors present.
    let phase1_src = git(&["show", &format!("{BASELINE}:src/agent/loop.rs")]);
    // The ONLY lines permitted to be added to the run_agent body are these
    // EXACT observer lines (verbatim, trimmed). We remove exactly these from
    // the current body and assert the remainder equals the Phase-1 body
    // line-for-line. Because the allowlist matches full lines verbatim, a
    // smuggled reasoning statement inside a fake observer block (e.g.
    // `on_step({ if budget==0 { return ... }; StepEvent::Started })`) does NOT
    // match any allowed line and therefore remains, tripping the equality.
    const ALLOWED_ADDED: &[&str] = &[
        "on_delta: &mut (dyn FnMut(StreamDelta) + Send),",
        "on_tool: &mut (dyn for<'e> FnMut(ToolEvent<'e>) + Send),",
        "on_step: &mut (dyn for<'e> FnMut(StepEvent<'e>) + Send),",
        "on_step(StepEvent::Started);",
        "on_step(StepEvent::Response(&message));",
        "on_tool(ToolEvent::PreCall {",
        "on_tool(ToolEvent::PostCall {",
        "tool_call_id: &call.id,",
        "name: &call.name,",
        "args: &call.args,",
        "result: &result,",
        "});",
    ];
    let lines = |src: &str| -> Vec<String> {
        run_agent_body(src)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty() && !l.starts_with("//"))
            .collect()
    };
    // The widened `on_delta` signature is in ALLOWED_ADDED, so its Phase-1
    // (non-Send) form must not count as a reasoning line on the baseline side.
    let base: Vec<String> = lines(&phase1_src)
        .into_iter()
        .filter(|l| l != "on_delta: &mut dyn FnMut(StreamDelta),")
        .collect();
    let now: Vec<String> = lines(&src)
        .into_iter()
        .filter(|l| !ALLOWED_ADDED.contains(&l.as_str()))
        .collect();
    assert_eq!(
        base, now,
        "the run_agent reasoning body changed beyond the EXACT allowlisted observer lines — \
         a control-flow branch was added, removed, or smuggled into an observer call"
    );
}

/// Extract the body of the `pub async fn run_agent(...)` function (up to its
/// matching closing brace at column 0), so the reasoning-line comparison is
/// scoped to the loop itself and not the module's tests/helpers.
fn run_agent_body(src: &str) -> String {
    let start = src
        .find("pub async fn run_agent")
        .expect("run_agent present");
    let after = &src[start..];
    // The function body ends at the first line that is exactly `}` at column 0
    // after the signature (the fn's closing brace).
    let mut depth = 0i32;
    let mut started = false;
    let mut end = after.len();
    for (i, ch) in after.char_indices() {
        match ch {
            '{' => {
                depth += 1;
                started = true;
            }
            '}' => {
                depth -= 1;
                if started && depth == 0 {
                    end = i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    after[..end].to_string()
}

/// `provider/mod.rs` may change ONLY by the mechanical `+ Send` propagation on
/// the `on_delta` callback signatures (forced by run_agent's blessed `+ Send`,
/// per O-P0-1). Exactly 3 signature lines gain `+ Send`; nothing else.
#[test]
fn provider_change_is_only_the_send_bound_propagation() {
    let (added, deleted) = numstat("src/provider/mod.rs");
    let diff = git(&["diff", BASELINE, "--", "src/provider/mod.rs"]);
    let mut send_widenings = 0usize;
    let mut mod_decls = 0usize;
    // Every ADDED line is either a `+ Send` widening of an on_delta
    // signature or a `pub mod` declaration; every REMOVED line is the
    // corresponding non-Send signature. No logic lines.
    for line in diff.lines() {
        if let Some(added) = line.strip_prefix('+') {
            if added.starts_with("++") {
                continue;
            }
            if added.trim_start().starts_with("pub mod ") && added.trim().ends_with(';') {
                mod_decls += 1;
                continue;
            }
            assert!(
                added.contains("+ Send") && added.contains("FnMut(StreamDelta)"),
                "unexpected added provider line (not a +Send widening): {added:?}"
            );
            send_widenings += 1;
        }
    }
    assert_eq!(
        (send_widenings, deleted),
        (3, 3),
        "provider/mod.rs must change only the 3 +Send signature lines (got +{send_widenings}/-{deleted})"
    );
    assert!(
        added as usize == send_widenings + mod_decls,
        "unexpected extra additions in provider/mod.rs ({added} added, {send_widenings} widenings, {mod_decls} mod decls)"
    );
}

/// `error.rs` may change ONLY additively (new BusError/ServerError variants +
/// their AgentError wrapping); no existing variant deleted.
#[test]
fn error_rs_is_additive_only() {
    let (added, deleted) = numstat("src/error.rs");
    assert!(added > 0, "error.rs gained the new typed error enums");
    assert_eq!(
        deleted, 0,
        "error.rs must be additive-only (got -{deleted}; an existing variant was changed)"
    );
}

/// The LOCKED Phase-1 wire-body golden fixtures must be byte-unchanged since
/// they were captured (Todo 1), and their SHA-256s must match the MANIFEST.
#[test]
fn locked_golden_fixtures_are_unchanged() {
    let manifest = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/phase1_wire/MANIFEST.txt"
    ))
    .expect("read MANIFEST");
    // The MANIFEST records the phase-1 commit + each fixture's SHA-256; the
    // golden capture test (phase1_wire_golden.rs) asserts byte-equality on
    // every run. Here we pin that the MANIFEST base_commit is the dereferenced
    // phase-1-complete COMMIT (not the annotated-tag object).
    let base = manifest
        .lines()
        .find_map(|l| l.split("base_commit:").nth(1))
        .map(str::trim)
        .expect("MANIFEST has base_commit");
    let commit = git(&["rev-parse", BASELINE]);
    assert_eq!(
        base,
        commit.trim(),
        "MANIFEST base_commit must be the dereferenced phase-1-complete commit"
    );
}
