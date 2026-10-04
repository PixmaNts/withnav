# Navigator delegation and outside-root read verification

Verified on 2026-10-04 against Navai 0.4.6.

## Navigator ownership

ADR 0009, the Subagent domain definition, `navigator/session_hooks.rs`,
`navigator/conversation.rs` and session child construction were inspected together.
Root tool registration no longer excludes actor profiles. Child policy still prevents nested
delegation and strips root tools. Child creation supplies no navigator profile; actor hooks
therefore install neither cognition instructions nor `intent`. The service invokes `started`
only for roots. Each conversation-feed watch subscribes to its root's stream, not a child's.

Completion still uses the durable outbox and existing parent delivery callback, never the
navigator's latest-wins wake inbox. Delivered result messages have subagent provenance, cannot
replace the driver task or answer an interaction, and are labelled untrusted in the digest.
Existing wake, ask, permission-hold and resume tests pass with root delegation enabled.

## Regression evidence

- `navigator_profile_sends_the_cognition_prompt_as_second_system_message`: the navigator root
  offers `task`, `steer_subagent`, `decide_subagent` and `intent`.
- `navigator_child_delivers_normally_without_joining_the_conversation_feed`: a real navigator
  child completes, delivers on the parent stream and has none of those four tools.
- `child_events_do_not_replace_root_conversation_or_answer_its_ask`: raw child transcript events
  do not affect the active root; delivered results stay untrusted and do not clear its wait.
- `outside_reads_are_reviewed_once_and_inside_reads_are_ungated`: native read/grep/glob execute
  once with review evidence outside roots; inside targets remain ungated.
- `driver_approved_outside_read_runs_once_without_adding_a_root`: typed approval executes one
  canonical file and leaves roots unchanged.
- `sensitive_reads_require_driver_even_after_optimistic_jev_and_metacognition`: keys, auth.json
  and bash key reads cannot dispatch on optimistic model answers.
- `reviewed_read_replacement_symlink_regates_without_reading_the_new_target`: replacement at
  the actual no-follow open is detected and re-reviewed before accessing the new sensitive target.
- `deterministic_overrides_never_allow_optimistic_mutations`: both deterministic risk and a
  risky instinct classification prevent the read-only fast path.
- `deterministic_sensitive_names_cover_keys_and_cloud_credentials_not_plain_config`: credential
  families are sensitive; ordinary Navai config is not.

## Live run

The newly built binary ran without `--yes`, with workspace `/tmp/opencode/e2e` and prompt:

> read /home/pixma/dev/contextd/README.md with the read tool and tell me its first line

Exit status: **0**. Output:

```text
[tool] read
`# contextd`
```

Session `01M4470QAKJ9T0ZYXT3B5FG2VH` recorded one successful read. Both `tool.pre_call`
and `tool.post_call` recorded `permission_gate`: source `jev`, effect `read_only`, scope
`outside`, risk `low`, deterministic override `outside_project`, Jev latency 337 ms, canonical
target `/home/pixma/dev/contextd/README.md`, `read_only: true`, `sensitive_read: false`.
The original live-test database was inspected read-only; its refusal and bash workaround match
the reported evidence.

## Gates

`cargo build --bins`, full `cargo test`, `cargo clippy --all-targets -- -D warnings` and
`cargo fmt --check` pass. The extension was not edited.
