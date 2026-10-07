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
- The contracted `InputValue { value, cut }` seam has no redaction marker. Unknown capture
  input fields (including `redacted`) are rejected rather than silently permitting replay;
  host adapters must withhold redacted captures until that seam gains explicit provenance.
- No provider pricing is inferred. Cost stays null, including replay. SHA-256 was added as
  a direct dependency because no root direct hash dependency existed.
- Runtime error messages supplied by the included provider transport are fixed/sanitized;
  host implementations remain responsible for excluding secrets from their context captures.
