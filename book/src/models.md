# Gateways and models

`models.json` is a **catalogue**. It declares **gateways** (endpoints you can call) and the
**models** each one serves.

## Model names

Every model has a qualified name, `<gateway>/<upstream id>`. In `omniroute/combo/gpt-6-astra`:

- `omniroute` is the gateway, declared in `models.json`.
- `combo/gpt-6-astra` is the upstream id. It is sent to the gateway verbatim.

The qualified name keeps two models with the same upstream id on different gateways apart. Names
may not contain whitespace. The old alias entries `default`, `fast` and `cognition` no longer
exist and are not accepted as compatibility names.

## A gateway

```json
"providers": {
  "omniroute": {
    "base_url": "http://localhost:20128/v1",
    "api_key_env": "OMNIROUTE_API_KEY",
    "catalog_format": "omniroute",
    "headers": {},
    "connect_timeout_ms": 5000,
    "request_timeout_ms": 600000
  }
}
```

| Key | Meaning |
|---|---|
| `base_url` | The OpenAI-compatible endpoint. |
| `api_key_env` | The name of the environment variable that holds the key. Never put the key itself in `models.json`. |
| `headers` | Extra HTTP headers. |
| `connect_timeout_ms` | Default `5000`. |
| `request_timeout_ms` | Default `600000`. |
| `catalog_format` | Optional. `"omniroute"` means the gateway's model list reports reasoning-effort tiers, and a model that omits `variants` gets one variant per tier. |

## A model

```json
"models": {
  "omniroute/combo/gpt-6-astra": {
    "provider": "omniroute",
    "model": "combo/gpt-6-astra",
    "api": "chat",
    "supports_tools": true,
    "supports_parallel_tool_calls": true,
    "variants": {
      "low":    { "reasoning_effort": "low" },
      "medium": { "reasoning_effort": "medium" },
      "high":   { "reasoning_effort": "high" }
    },
    "reasoning_history": "preserve"
  }
}
```

The key must equal `<provider>/<model>`. WithNav refuses a catalogue where they disagree.

| Key | Meaning |
|---|---|
| `provider` | The gateway name. |
| `model` | The upstream id. |
| `api` | `chat` (default), `fim` or `systemone`. Each [layer](layers.md) needs a specific one. |
| `context_window` | Tokens the model accepts. A usable chat model needs one, from here or from the gateway's own model list. |
| `max_input_tokens`, `max_output_tokens` | Optional limits. |
| `max_message_tokens` | Optional. Bound for one serialised compaction message. Must be positive. |
| `supports_tools` | Whether the model can call tools. Default `false`. |
| `supports_parallel_tool_calls` | Optional. |
| `images` | Whether the model accepts image input. Absent means `false`: the model gets a text placeholder where an image would go. |
| `reasoning_history` | `preserve` (default) or `omit`. |
| `default_variant` | The variant a new conversation starts with. If you leave it out, there is no variant. |
| `variants` | Named request overrides; see below. |

`thinking` is deprecated and ignored. Write `default_variant` instead.

### Variants

A **variant** is a named set of extra fields merged into the request body, for example
`reasoning_effort`. The example catalogue declares `low`, `medium` and `high` as the request-body
field `reasoning_effort`. It never adds a `-high` suffix to the model name.

A variant cannot set the fields the request owns: `model`, `messages`, `tools`, `tool_choice`,
`parallel_tool_calls`, `functions`, `function_call`, `stream`, `stream_options`, `max_tokens`,
`max_completion_tokens`, `n` and `temperature`.

If `variants` is left out, a model on a gateway with `catalog_format: "omniroute"` gets its
variants from the gateway. `"variants": {}` means none.

## API keys

For each gateway, WithNav looks for a key in this order:

1. The key stored in `~/.config/withnav/auth.json`. Write it with `nav auth set <provider>`.
2. The environment variable named by `api_key_env`.

A stored key wins over the environment. See [`nav auth`](cli.md#nav-auth).

## Example setup

Start from
[`models.example.json`](https://github.com/PixmaNts/withnav/blob/develop/models.example.json) and
[`config.example.json`](https://github.com/PixmaNts/withnav/blob/develop/config.example.json).
Adapt endpoints and assignments before copying them to `~/.config/withnav/`. No real keys are
included. Provide them with `nav auth set omniroute` and `nav auth set inception`, or with the
example's `OMNIROUTE_API_KEY` and `INCEPTION_API_KEY` environment variables.

Things to know about the example:

- Chat models leave their context window out on purpose, so WithNav reads it from the
  gateway. A usable chat model needs a window it can resolve.
- Reflex (FIM) and Instinct (System One) models carry fixed sample windows and output caps. Check
  those values against your own deployment.
- Combo models declare `low`, `medium` and `high` as request-body `reasoning_effort`.
