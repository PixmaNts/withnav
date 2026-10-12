# Glossary

**Agent**
: A role Cognition plays: its prompt, tools, access and an optional model and variant. `build`,
  `plan`, `vigie`, `lux`, `mousse`, `cartographe`, `sondeur` or a [custom one](agents.md).

**Beacon**
: A short remark from the Metacognition layer, tied to a line, signed by Nav. It never changes your
  code.

**Captain**
: You. The human writing the code, and the default author of every line.

**Catalogue**
: `models.json`: the gateways and models available. See [Gateways and models](models.md).

**Delegation**
: You explicitly asking Nav to do a piece of work itself, instead of guiding you through it.

**Default agent**
: The agent a new session starts with. `default_agent`, or `build` when absent.

**Editor session**
: A session started by an editor client. It adds the pair-programmer prompt and the `intent` tool.

**Gateway**
: An endpoint declared under `providers` in `models.json`. The first part of a model's name.

**Intent record**
: `.withnav/intent.md` in a project: what you are trying to do. Metacognition compares your code
  with it.

**Layer**
: One of four roles: Reflex, Instinct, Metacognition and Cognition. See [Layers](layers.md).

**Nav**
: Your single point of contact. It runs the `build` and `plan` agents. WithNav is the whole
  system.

**Remark**
: See Beacon.

**Subagent**
: An agent that another agent hands a task to. See [Agents](agents.md#subagents).

**Suggestion**
: Ghost text from the Reflex layer. It becomes code only when you accept it.

**Variant**
: A named set of request fields, such as `reasoning_effort`, that a model offers. See
  [Variants](models.md#variants).

**Verdict**
: Your rating of a remark: useful, noise or dismissed.
