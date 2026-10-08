# The decision pipeline is data, not code

Decision models like Jev are new and there is almost no prior art on which questions to ask or which
thresholds to use, so the pipeline will be changed by trial and error far more often than the rest
of the system. It therefore lives in files, not in Rust: a TOML pipeline (questions, then ordered
branches whose conditions are short expressions such as `worth_attention > 0.7 && route ==
"metacognition"`) plus Markdown prompts. A project file overrides a user-wide default. Saving either
reloads it without restarting the editor; a file that fails validation is reported and the previous
pipeline keeps running.

Tuning is done by replay rather than by living with a setting for a day: every trigger is logged
with the full state sent to the instinct, its answers and the driver's verdict, so a candidate
pipeline can be run over past history. Changing only thresholds replays offline; changing a
question re-asks the instinct.

## Considered Options

- Structured TOML conditions (`{question, above}`): rejected, combining two answers is the common
  case while experimenting and becomes verbose.
- An embedded scripting language (Rhai, Lua): rejected, errors would surface while coding instead
  of at load time, and the power is not needed yet.

Naming status: the human role is now called the Captain and the product WithNav;
see [ADR 0014](0014-rebrand-withnav.md). Original decision wording is retained.
