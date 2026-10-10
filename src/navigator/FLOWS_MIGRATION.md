# Editor flow migration

The legacy engine was removed on 2026-10-08 after the Captain replayed their real decision log:
18 comparable triggers, 0 changed actions. Four older entries lacked question provenance; a
separate input-limit correction addresses the 25.7 KiB context capture. This removal does not
change that limit.

Built-in save, idle and conversation flows are enabled by default and retain the original state
and prompts. No enabled flow for a trigger means nothing runs; there is no legacy fallback.

Custom workspace `.withnav/pipeline.toml` and global `navigator/pipeline.toml` files are
converted once into their corresponding flow directories, with a warning and without replacing
Captain-authored flows. The originals and referenced prompts remain untouched. Only a private
read-only TOML schema survives in `src/flows/migrate/legacy.rs`, not an evaluator.

`nav lsp replay --flows --log /path/to/withnav.jsonl` remains offline; `--flows` is optional and
accepted for compatibility. `--pipeline` and `--reask` are removed. Missing provenance is
reported as not comparable, never inferred.
