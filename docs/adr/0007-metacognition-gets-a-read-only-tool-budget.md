# The metacognition gets read-only tools under a small budget

A metacognition that cannot look beyond the edited file can only comment on local syntax, but one
that chains tool calls stops being fast. It receives pre-built context (repository map, symbol
outline, shared context) plus read-only tools (read, grep, symbol lookup) capped at about two calls
per remark. Only the cognition has the full toolset (ADR 0001).

Naming status: the human role is now called the Captain and the product WithNav;
see [ADR 0014](0014-rebrand-withnav.md). Original decision wording is retained.
