# Host integration status

The runtime is shared by root sessions and Studio; remaining limitations are listed below.

- Empty builtins are intentional in this branch; builtin flows and editor event adapters are
  delivered by the editor lane. The replaceable `FlowRuntime::set_effects` sink handles remarks.
- `ContextSnapshot` accepts a caller session snapshot and collects workspace git and intent inputs.
  Conversation/editor inputs remain absent. Root tool hooks and message/turn adapters pass
  the actual session snapshot; the instinct model is not the session model. Intent collection
  refuses files over 16 KiB; the existing intent reader still has a metadata/read race.
- Health uses independent private hourly aggregates for the latest 24 buckets, not retained runs.
  This is an hour-aligned approximation, not an exact rolling 24-hour window. An unavailable
  private directory prevents persisting both health and capture-failure counters.
- Descriptor traversal refuses ancestor symlinks. Ownership is checked on the final directory
  and files, not system-owned ancestors such as `/home`. Capture workspace directories require
  0700; trust grants use RFC3339 and writes use 0600.
- Multi-file save errors currently do not enumerate already-written template files. Template deletion
  is conservative: templates are never deleted. Directory fsync and cross-process write locking remain.
- Blocking effects are applied after combination, ordered by flow name; native verdict application
  records do not yet describe the agent's eventual permission-resolution outcome.
- Redaction conservatively marks entire inputs containing common token prefixes and intent read
  from permission-sensitive paths. Additional collector implementations must carry path sensitivity.
- Payload enum/nested-schema validation remains incomplete. HTTP body limits, sample/replay with
  an injected client, asynchronous isolation, outage reset, persisted non-driver provenance and
  message routing effects have regression coverage. A real provider HTTP sample/replay test and
  end-to-end root-versus-child tool-hook/pending-permission isolation tests remain.
- Flow provenance is persisted as `{source:"flow",flow,run_id}` and safe-boundary delivery never
  admits a driver command. Historical note provenance in compaction summaries needs explicit
  preservation in the compaction lane; recent transcript messages retain their typed provenance.
- Studio and contract definitions agree; the engine record currently emits numeric `budget_ms` and
  node `duration_ms` rather than nullable values described by Studio. No engine files were changed.
- HTTP peer metadata is installed by server listener wiring; network checks fail closed if the
  router is mounted without loopback ConnectInfo. Launch authorization uses WebConfig;
  non-WebUI launch-workspace registration remains to be added.
