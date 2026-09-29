# The driver is the default author of the code

The whole point of rust-agent is that the human keeps a mental model of their own codebase, which
agentic "vibe coding" destroys: the agent writes everything, planning always misses something, and
the early speed turns into long iteration loops. So no layer writes to a file on its own. The reflex
offers suggestions the driver accepts keystroke by keystroke, the metacognition only produces
remarks, and the cognition may produce code only as a proposal (a diff) on the driver's explicit
request, applied once the driver accepts it.

## Considered Options

- Driver only, no generated code beyond suggestions: rejected, generating pure plumbing on request
  proved useful while building the first prototype.
- Cognition writes freely, as in classic coding agents: rejected, it is exactly the workflow this
  project exists to replace.

## Consequences

The cognition's toolset keeps write tools, but their output is routed to the driver as a proposal
instead of being applied directly.
