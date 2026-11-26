# Layers

Reflex, Instinct and Metacognition each use **one model at a time**. You assign it in
`config.json` under `layers`. Cognition is different: it runs [agents](agents.md), each with its
own model.

| Layer | Assignment | Required `api` | Job |
|---|---|---|---|
| Reflex | one model, editor only | `fim` | Inline completion as you type (Mercury in the example). |
| Instinct | one model | `systemone` | Typed decisions and supervision: when to speak, what the whole call changes and whether the current request covers it, and progress checks. |
| Metacognition | one model | `chat` | Remarks beside your code, the parallel review of a permission question when `permissions.review` is `on` or `shadow` ([Permissions](permissions.md#parallel-review-permissionsreview-on)), and the watchdog's doubts about a long turn: the review reads your session goal (or your last message) and the session ledger, and may give a short reason. Reading the ledger and the review share one 60-second limit; past it, Nav asks you. Prefers the `low` variant when the model offers one, otherwise the model's default. |
| Cognition | agents, each with its own role and model | `chat` | The agent loop you talk to. |

## Assign a model

A layer takes a model name, or an object with a variant:

```json
{
  "layers": {
    "reflex": "inception/mercury-edit-2",
    "instinct": "omniroute/example/decision-model",
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
| Instinct | The permission flow cannot judge: your rules apply, and a call with no rule asks you. Supervision of long turns falls back to a visible, renewable 500-step cap. |
| Metacognition | No remarks, no parallel review of permission questions (you answer every card). Compaction summaries use the session's model. |

`nav doctor` reports each layer and why it is off. See
[Troubleshooting](troubleshooting.md#check-your-setup-with-nav-doctor).

## What Instinct was asked

Flow runs keep their own records. The other Instinct callers, the subagent and root watchdogs and
the compaction judge, write one line per call to `instinct-calls.jsonl` in the data directory
(`~/.local/share/withnav/` on Linux):

| Field | Meaning |
|---|---|
| `ts`, `caller`, `session_id` | When, who asked (`child_watchdog`, `root_watchdog`, `prune`), for which conversation. |
| `model` | The Instinct model identifier sent on the wire. |
| `state_bytes`, `state_sha256` | The size of the state and its SHA-256. The state itself is never stored. |
| `questions`, `answers` | The question names (never their text) and the answers. |
| `latency_ms`, `error` | How long the call took, and on failure `unavailable`, `invalid`, `cancelled`, or `abandoned` when the caller stopped waiting (its deadline passed). |

The state and the question text are left out because they quote tool output. The line has no
token counts: these callers receive answers only.

## Where you see the choices

The terminal's right panel and the VS Code chat header tooltip show the model of each lower layer
and of the current agent, marked when overridden. In the editor clients, assignments are
read-only; change them in `config.json` or a Settings screen.
