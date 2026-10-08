# The instinct speaks the System One API

The instinct answers bounded, typed questions (`choice`, `score`, `noul`) over one state, instead of
asking an LLM to emit JSON. We use the System One decisions API (`POST /v1/systemone`, request
`{model, state, questions}`, typed `answers`) as the contract of the instinct layer, not a
home-made schema, because more models are appearing behind the same API and staying on it keeps the
provider swappable. Jev (`typesafe/jev-1.13`) is the first provider, reached through the local
OmniRoute gateway: a probe on 2026-09-29 returned a calibrated `noul` answer in 0.35 s for about
$0.00001.

## Consequences

Calls stay off the hot keystroke path budget-wise (hosted, ~0.35 s), so the instinct is evaluated on
triggers, not on every keystroke. The code sent as state leaves the machine.

Naming status: the human role is now called the Captain and the product WithNav;
see [ADR 0014](0014-rebrand-withnav.md). Original decision wording is retained.
