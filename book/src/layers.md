# Layers

Reflex, Instinct and Metacognition each use **one model at a time**. You assign it in
`config.json` under `layers`. Cognition is different: it runs [agents](agents.md), each with its
own model.

| Layer | Assignment | Required `api` | Job |
|---|---|---|---|
| Reflex | one model, editor only | `fim` | Inline completion as you type (Mercury in the example). |
| Instinct | one model | `systemone` | Typed decisions and supervision: when to speak, permission review, progress checks. |
| Metacognition | one model | `chat` | Remarks beside your code, and review of risky calls. Prefers the `low` variant when the model offers one, otherwise the model's default. |
| Cognition | agents, each with its own role and model | `chat` | The agent loop you talk to. |

## Assign a model

A layer takes a model name, or an object with a variant:

```json
{
  "layers": {
    "reflex": "inception/mercury-edit-2",
    "instinct": "omniroute/typesafe/jev-1.13",
    "metacognition": {
      "model": "omniroute/combo/deepseek-v4-flash",
      "variant": "low"
    }
  }
}
```

- The model must exist in the catalogue and use the `api` the layer requires.
- `variant` is optional. Leave it out for the model's default. Use `null` or `"off"` for no
  variant. Any other value must name a variant the model offers.

## What happens when a layer is not usable

A missing, unknown or API-incompatible assignment turns that layer **off**, with a clear
diagnostic. It does not switch to another model silently.

| Layer off | Effect |
|---|---|
| Reflex | No inline suggestions. |
| Instinct | Permission review asks you when it cannot decide. Supervision of long turns falls back to a visible, renewable 500-step cap. |
| Metacognition | No remarks. Compaction summaries use the session's model. |

`nav doctor` reports each layer and why it is off. See
[Troubleshooting](troubleshooting.md#check-your-setup-with-nav-doctor).

## Where you see the choices

The terminal's right panel and the VS Code chat header tooltip show the model of each lower layer
and of the current agent, marked when overridden. In the editor clients, assignments are
read-only; change them in `config.json` or a Settings screen.
