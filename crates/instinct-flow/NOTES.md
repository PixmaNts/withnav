# InstinctFlow implementation resolutions

- The engine applies the stricter image resolution: decision image inputs are refused even
  when a custom client advertises images, because the v1 state/protocol has no image encoding.
- A deadline is never extended for fallback input collection. On timeout, newly requested
  fallback context is absent; cached context and event/constants remain available. A typed
  absent fallback parameter produces no fallback action rather than an ill-typed proposal.
- CLI input files reject final-component symlinks. Descriptor-relative ancestor traversal,
  workspace trust, capture ownership and retention remain the host's filesystem responsibility.
- Replay never calls a provider or input source. Missing capture fails closed. A failed batch
  can reproduce its recorded failure only when its captured question/state/identity match.
- `InputValue.redacted` defaults to false for older captures. Hosts supply the real live value
  with this marker to withhold it from captures. Selected inputs store null with redacted=true;
  retained states substitute `<redacted>`. Replay readers reject redacted evidence.
- No provider pricing is inferred. Cost stays null, including replay. SHA-256 was added as
  a direct dependency because no root direct hash dependency existed.
- Runtime error messages supplied by the included provider transport are fixed/sanitized;
  host implementations remain responsible for excluding secrets from their context captures.
- CLI JSONL replay currently reads a bounded file (32 MiB) before evaluating records rather
  than streaming them; no capture writer or persistence layer is part of the crate.
