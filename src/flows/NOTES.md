# Host integration status

The host surface is an initial implementation, not release-ready enforcement.

- Empty builtins are intentional. Production event subscription and effects wiring remain at
  integration; no agent-loop code is changed here.
- `ContextSnapshot` is supplied by the caller. Conversation/editor/intent/git collectors are not
  yet wired; unavailable context remains absent, including Studio sample context. A non-null
  sample session is refused as unavailable rather than using another workspace's context.
- Captures are bounded and private, but independent 24-hour health persistence and capture-failure
  counters are not implemented. The health endpoint derives its 24-hour window from retained live
  records, so eviction can undercount health; sample/replay are excluded.
- Descriptor traversal refuses ancestor symlinks. Ownership is checked on the final directory
  and files, not system-owned ancestors such as `/home`. Capture workspace directories require
  0700; trust grants use RFC3339 and writes use 0600.
- Multi-file save errors currently do not enumerate already-written template files. Template deletion
  is conservative: templates are never deleted. Directory fsync and cross-process write locking remain.
- Blocking effects are applied after combination, ordered by flow name; native verdict application
  records remain to be completed by the tool-hook integration.
- Secret redaction, payload enum/nested-schema validation,
  and the complete requested HTTP/runtime regression matrix remain to be implemented.
- Studio and contract definitions agree; the engine record currently emits numeric `budget_ms` and
  node `duration_ms` rather than nullable values described by Studio. No engine files were changed.
- HTTP peer metadata is now installed by server listener wiring; network checks fail closed if the
  router is mounted without loopback ConnectInfo. Launch authorization currently uses WebConfig;
  non-WebUI launch-workspace registration requires an integration seam.
