# Layers, catalog models and durable agent overrides

Date: 2026-10-05

Status: Accepted — implemented for v0.5.0

Naming status: the human role is now called the Captain and the product WithNav;
see [ADR 0014](0014-rebrand-withnav.md). Original decision wording is retained.

Model aliases mixed catalog identities with the driver's choices and made a role look like a
model. We separate those concepts so catalog refreshes cannot change assignments, and a driver
can change one agent's model in one conversation without changing other work.
The vocabulary is in [CONTEXT.md](../../CONTEXT.md).

## Decision

- **Catalog and choices are separate.** Generated/refreshed `models.json` describes gateways,
  models, limits, capabilities, API kinds and variants. `config.json` holds layer assignments,
  agent assignments and `default_agent` (`build` when absent). Keeping choices in a generated
  catalog was rejected: importing newly available models must not overwrite the driver's choices.
- **A model has a canonical `gateway/upstream-id` identity.** The gateway names a configured
  endpoint; the rest is sent verbatim, including provider segments such as `cc` or `combo` inside
  OmniRoute. Gateway and upstream provider are distinct levels, not interchangeable names.
  Bare upstream ids were rejected because different gateways can expose the same id. Nickname
  aliases, including `default`, `fast` and `cognition`, are removed rather than maintained as a
  second naming system.
- **Layers are roles, not catalog entries.** Reflex, instinct and metacognition each have one
  configured model at a time, with FIM, System One and chat APIs respectively. Cognition runs
  agents in parallel sessions rather than having a singleton layer model. Configurable flows
  remain planned for v0.6. Reflex is
  editor-only. Treating all four layers as interchangeable model aliases was rejected: it hides
  the lower layers' contracts and cognition's independent agent work.
- **Agent assignment and session choice have different scopes.** An assignment in `config.json`
  wins over an agent markdown header. A primary agent without an assignment uses the default
  agent's model; an unassigned subagent uses its parent's effective model, including overrides.
  A subagent with its own assignment does not inherit the parent's override. `/model` and
  `/variant` choices persist per `(session, agent)`; switching away and back or reopening restores
  them. Global assignment edits and a single session-wide override were rejected: neither keeps
  an experiment local while preserving each agent's choice.
- **Variants are request settings, not model names.** Selection distinguishes `inherit`, `off`
  and a named variant. Changing model without specifying a variant uses the new model's default;
  metacognition prefers `low` when offered, otherwise the model default. OmniRoute combos forward
  body parameters such as `reasoning_effort` and adapt them to the selected provider, but their
  catalog does not advertise effort tiers. The client import therefore declares `low`, `medium`
  and `high` for `combo/*` through `reasoning_effort`, preserving manually declared variants when
  merging. A `-high` model-name suffix was rejected: it does not select combo reasoning effort.
- **Editor context is orthogonal to model selection.** An editor session adds the pair-programmer
  prompt, `intent` tool and navigator conversation attachment to the chosen agent. It starts on
  the default agent like any session. A navigator profile that implicitly selects a model was
  rejected; an editor attachment is not an agent role or model choice. This updates the profile
  and model-selection wording of [ADR 0008](0008-primary-agents-and-resumable-tool-authorization.md),
  not its separation of roles and permissions or the ownership rules of
  [ADR 0009](0009-background-subagents-and-durable-delivery.md).
- **Migration is manual.** There is one active development user, so the driver's configuration
  will be migrated by hand. Automatic old-config support was rejected: maintaining two resolution
  schemes would add ambiguity for no current compatibility need.

## Consequences

An invalid nondefault agent assignment falls back to the default agent's model with a visible
warning naming the agent and model. An unknown default agent or one without a usable model
prevents server and LSP startup with a clear error: there is no valid fallback. A missing,
unknown or API-incompatible lower-layer model disables that layer with a clear message;
existing permission and watchdog fallbacks remain available.

Clients select an agent for a new conversation and may add a local model override. Catalog
presentation groups gateway first, then provider within that gateway; refreshing the catalog
does not change the driver's assignments. Persisting overrides adds durable state but prevents
reopening or agent switching from silently losing a choice.

This decision does not claim v0.5.0 is deployed and does not perform a live configuration
migration. Runtime and client selection are implemented; manual visual acceptance remains a
release check. Configuration editors and flow design for v0.6 are outside this decision.
