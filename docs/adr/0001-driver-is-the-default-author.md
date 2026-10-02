# The driver is the default author; the cognition assists unless asked to do the work

The point of Navai is that the human keeps a mental model of their own codebase, which agentic
"vibe coding" destroys: the agent writes everything, planning always misses something, and the early
speed turns into long iteration loops. The layers around the cognition exist to make pair
programming possible, not to take capabilities away: the cognition stays a classic coding agent with
its full toolset (`read`, `write`, `edit`, `bash`...). What changes is its role, set by the system
prompt: by default it assists and guides the driver, who writes the code; when the driver explicitly
asks it to do the work, it does it like any coding agent. The reflex only offers suggestions the
driver accepts, and the metacognition only produces remarks.

## Considered Options

- Hard restrictions (withholding `bash`, routing every write through an accept-this-diff mechanism):
  rejected, it cripples a coding agent for builds, tests and exploration, and it forbids the
  legitimate "do it for me" case. The driver is the one deciding when to delegate.
- Cognition writes freely by default, as in classic coding agents: rejected, it is exactly the
  workflow this project exists to replace as the default.

## Consequences

Authorship relies on the prompt and on the driver's review, not on a sandbox. The intent record is
written by the cognition on its own: it is the navigator's working memory, not code.
