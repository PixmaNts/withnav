# Host integration status

The host surface is an initial implementation, not release-ready enforcement.

- Empty builtins are intentional. Production event subscription and effects wiring remain at
  integration; no agent-loop code is changed here.
- `ContextSnapshot` accepts a caller session snapshot and collects workspace git and intent inputs.
  Conversation/editor inputs remain absent until event integration. Tool-hook callers must pass
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
  records remain to be completed by the tool-hook integration.
- Redaction conservatively marks entire inputs containing common token prefixes and intent read
  from permission-sensitive paths. Additional collector implementations must carry path sensitivity.
- Payload enum/nested-schema validation and the complete requested HTTP/runtime regression matrix
  remain incomplete (provider sample/replay, router-level body limits, async isolation and outage reset).
- Studio and contract definitions agree; the engine record currently emits numeric `budget_ms` and
  node `duration_ms` rather than nullable values described by Studio. No engine files were changed.
- HTTP peer metadata is now installed by server listener wiring; network checks fail closed if the
  router is mounted without loopback ConnectInfo. Launch authorization currently uses WebConfig;
  non-WebUI launch-workspace registration requires an integration seam.
