# Editor flow migration

Editor flows own an eligible trigger exclusively; absent or disabled flows leave the legacy
pipeline in charge. The default save, idle and conversation flows preserve the legacy
questions, state templates, branch ordering and independent wake cooldown.

Run `navai-lsp replay --flows --log /path/to/navai.jsonl` before retiring the legacy engine.
This is offline and never applies effects. Missing original question/state provenance is
reported as noncomparable, not inferred. The old pipeline engine remains until the driver
has checked equivalence against their own recorded decisions.
