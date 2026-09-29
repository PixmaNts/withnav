# The shared context is split into a durable intent record and an ephemeral live state

Fast layers are only useful if they know what the driver is trying to do, and a prototype on
2026-09-29 showed the metacognition guessing intent from a code comment because it had nothing else.
The shared context is therefore split: the intent record (intent, plan, decisions) is a
version-controlled file the driver can read and correct, so what the system believes is never
opaque; the live state (recent edits, diagnostics, conversation summary) stays in memory, because it
changes every second and has no business in git. The cognition writes the intent record as the
conversation settles things; every layer reads both parts.

The intent record is one file per project, `.navigator/intent.md`, linking to the active feature spec
rather than being that spec: the work of the moment and a feature's specification do not live as long.
