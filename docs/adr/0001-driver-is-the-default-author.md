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

The cognition has no direct write tool: `write` and `edit` become a `propose` tool whose diff is
applied only when the driver accepts it, and `bash` is withheld because it can write files behind
that mechanism (filtering commands is not a reliable guard). The one autonomous write is the intent
record: it is the navigator's working memory, not code, and the driver can read and revert it.
