# Host integration status

Each process owns its workspace host. Event-local editor effects share that process's activation
snapshot, provider and captures. Successful explicit reload and trust changes atomically advance
a private workspace-hashed generation marker. Other Navai processes reload all-or-nothing before
their next event; invalid definitions retain their previous snapshot and expose reload diagnostics.
External file edits alone do not activate changes. Trust is checked fail-closed on every event,
including when a propagated reload fails, so revocation still excludes workspace flows immediately.

- Provider/config/auth initialization failure keeps a runtime with an unavailable decision client:
  deterministic rules still execute, decisions fail through provider-error fallback, and Studio
  list/health expose a sanitized diagnostic. Untrusted directories are excluded before reload IO.
- Captures use a bounded background writer (32 pending records); queue overflow omits capture and
  increments `capture_drops` in health. This process-local counter is not durable. Capture/health
  filesystem IO is not part of blocking admission; capture visibility is eventually consistent.
  Host shutdown closes admission and drains pending captures for at most two seconds, joins finished
  writers, and counts queued records dropped on timeout. An in-flight filesystem call is detached
  on timeout rather than cancelled; it may finish afterward.
- Trust/capture writers share canonical-resource mutexes and stable sibling advisory flocks across
  atomic replacements. External editors not taking those locks are not serialized.
- Template maps are validated before writes; `.toml` and unsafe sibling names are refused. Existing
  files outside the checked document revision cannot change; shared templates cannot be modified.
- Host redaction covers event payloads, selected inputs, batch states and all persisted/returned
  record text. Selected evidence retains engine redaction markers for missing-capture replay.
- Migration validates the full conversion before installation and uses no-replace renames. Reads
  always exclude expired captures. Physical startup pruning and freezing git/intent sources across
  separate collection frontiers remain follow-ups.

- `ContextSnapshot` accepts a caller session snapshot and collects workspace git and intent inputs.
  Conversation/editor inputs remain absent. Root tool hooks and message/turn adapters pass
  the actual session snapshot; the instinct model is not the session model. Intent collection
  refuses files over 32 KiB; the existing intent reader still has a metadata/read race.
- Health uses independent private hourly aggregates for the latest 24 buckets, not retained runs.
  This is an hour-aligned approximation, not an exact rolling 24-hour window. An unavailable
  private directory prevents persisting both health and capture-failure counters.
- Descriptor traversal refuses ancestor symlinks. Ownership is checked on the final directory
  and files, not system-owned ancestors such as `/home`. Capture workspace directories require
  0700; trust grants use RFC3339 and writes use 0600.
- Multi-file save errors currently do not enumerate already-written template files. Template deletion
  is conservative: templates are never deleted. Directory fsync remains.
- Blocking effects are applied after combination, ordered by flow name; native verdict application
  records do not yet describe the agent's eventual permission-resolution outcome.
- Redaction conservatively marks entire inputs containing common token prefixes and intent read
  from permission-sensitive paths. Additional collector implementations must carry path sensitivity.
- Payload enum/nested-schema validation remains incomplete. HTTP body limits, sample/replay with
  an injected client, asynchronous isolation, outage reset, persisted non-driver provenance and
  message routing effects have regression coverage. A real provider HTTP sample/replay test and
  end-to-end root-versus-child tool-hook/pending-permission isolation tests remain.
- Flow provenance is persisted as `{source:"flow",flow,run_id}` and safe-boundary delivery never
  admits a driver command. Compaction labels flow notes as untrusted flow data in its summary
  input, so a compacted conversation never turns one into a driver instruction.
- Studio and contract definitions agree; the engine record currently emits numeric `budget_ms` and
  node `duration_ms` rather than nullable values described by Studio. No engine files were changed.
- HTTP peer metadata is installed by server listener wiring; network checks fail closed if the
  router is mounted without loopback ConnectInfo. Launch authorization uses WebConfig;
  non-WebUI launch-workspace registration remains to be added.
