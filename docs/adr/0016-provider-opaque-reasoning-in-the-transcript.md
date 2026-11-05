# Provider-opaque reasoning lives on the assistant message, replayed only to its producer

Date: 2026-10-21

Status: Accepted — storage, replay rule and provider seam implemented; no wire captures data yet.

The vocabulary is in [CONTEXT.md](../../CONTEXT.md).

## Context

The transcript is a list of `ChatMessage`, the OpenAI Chat Completions shape. An assistant
message holds text, readable reasoning (`reasoning_content`) and tool calls. The same JSON is
stored in `sessions.transcript`.

Several providers return reasoning data that only they can read and that they expect back
unchanged on the next request: Anthropic's signed `thinking` and `redacted_thinking` blocks,
Gemini's thought signatures, OpenAI Responses reasoning items with `encrypted_content`,
OpenRouter's `reasoning_details`. On 2026-10-20, through OmniRoute's Anthropic Messages
endpoint, a forged thinking signature was refused with `400 Invalid signature in thinking
block`; a dropped block was accepted, but Anthropic documents the block as required within a
tool-use turn, and the model then loses its reasoning between two tool calls. `ChatMessage`
had nowhere to keep that data, which blocked any native adapter from keeping it.

## Decision

### An optional field, not a content-block transcript

`ChatMessage::Assistant` gains `opaque: Option<OpaqueReasoning>`
(`withnav-provider`'s `opaque.rs`): the producer identity and the ordered items, each a `kind`
chosen by the adapter that captured it and `data`, the provider's JSON text exactly as received.
Absent when empty, so stored rows and request bodies without it are byte-identical to before.

Replacing `ChatMessage` with a list of content blocks was considered. It touches about 460
references in 44 files, changes the stored format of every row and every compaction projection,
and re-derives the Chat Completions body, to serve adapters that do not exist yet. Nothing here
prevents it later: the field would become one block kind among others.

`data` is a string holding JSON text, not `serde_json::value::RawValue`: `ChatMessage` is an
internally tagged enum, which buffers its fields while reading the tag and loses the raw-value
marker (reproduced 2026-10-21: `invalid type: newtype struct`). The string round-trips byte for
byte. A dialect that sends it back writes it as JSON, not as a string.

No position field. Where the order of reasoning relative to text and tool calls matters
(interleaved thinking, signatures attached to Gemini parts), the adapter keeps its own ordered
native representation inside `data`. A shared position model waits for an adapter that needs it.

### Producer identity

`wire + endpoint + model`: the dialect of the bytes, the base URL without credentials, query or
fragment, and the upstream model id sent in the request. Exact equality; an empty or
unparseable endpoint is not an identity. This is deliberately not
`ModelResolution::same_identity`, which compares the provider name and API and treats an empty
endpoint as a wildcard. Equal endpoint and model do not prove equal routing behind a gateway; it
is the conservative baseline. Variant settings are not part of it: an adapter checks the
configuration compatibility its wire needs when it renders.

### Replay rule

One predicate, `opaque::retain_for`: keep the items only when the producer equals the
destination and the destination dialect can send that kind; otherwise remove them. It runs in
the request projection (`compaction::project_reasoning`, index-preserving, after the
`reasoning_floor` rule, before compaction) and again in `ModelClient::chat_turn` (which `chat` goes through), so a caller that
bypasses the agent actor cannot send another model's bytes. Removal never turns opaque data
into text. An assistant message is "empty" and dropped from the request only when nothing
sendable remains; the context gauge counts only what is sent.

### Storage

Reading is lenient for `opaque` only: a bad producer drops the field, an item whose `data` is
not JSON is dropped alone, unknown kinds are kept, nothing is logged from the payload. The rest
of the transcript is strict: a stored transcript that does not parse fails the resume and leaves
the row untouched (before, it resumed as an empty conversation whose next snapshot overwrote the
stored history). An older build reading a newer row ignores the field and saves it back without
it: the reasoning is lost, the conversation is not.

### What reaches clients

Nothing. `AssistantMessage`, `StreamDelta` and every event are unchanged. Opaque data stays
between the provider, the transcript and the database.

## Consequences

- Each native adapter maps its own blocks to and from `OpaqueItem`, marks its kinds sendable for
  its wire, and states the replay unit it needs (block, message, tool-use turn). Providers can
  bind a signature to the preceding messages; compaction and the skill projection change those.
  An adapter must not replay an item whose required context changed. That is its acceptance
  requirement, not this decision's.
- Metacognition keeps its own multi-round history without opaque data. A native adapter used for
  metacognition migrates it first.
- Summaries never contain opaque data; everything before a compaction boundary loses it.
- Encrypted reasoning can be tens of kilobytes per step, written with each transcript snapshot.
  No cap: a truncated item cannot be replayed.

## Prior art

Provider documentation for the data this stores:
[Anthropic, preserving thinking blocks](https://platform.claude.com/docs/en/build-with-claude/thinking#preserving-thinking-blocks),
[Gemini, thought signatures](https://ai.google.dev/gemini-api/docs/thought-signatures),
[OpenAI, reasoning items](https://developers.openai.com/api/docs/guides/reasoning),
[OpenRouter, reasoning tokens](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens).
