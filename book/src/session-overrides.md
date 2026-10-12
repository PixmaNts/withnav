# Editor context and session overrides

## Editor sessions

A session started by an editor client is an **editor session**. It adds three things to its agent:
the pair-programmer prompt, the `intent` tool and the feed of what the navigator sees.

- It starts on the same default agent as any other session. Editor context does not choose a
  model.
- Subagents do not inherit the editor attachment.

## Change the model in a conversation

Clients choose an **agent** first, then optionally a local model override for it. These commands
work in every client:

| Command | Effect |
|---|---|
| `/model` | Report the effective model, where the choice comes from, and the available models. |
| `/model <gateway/id>` | Switch the current agent's model in this session. If the agent is busy, the switch happens after the turn. The new model's default variant is used. The history must fit the new model's budget; WithNav compacts it when it can. |
| `/variant` | Report the effective variant, its source and the choices. |
| `/variant <name>` | Set a named variant. |
| `/variant off` | Disable variants explicitly. |
| `/variant inherit` | Remove the override and return to the assignment or model default. |
| `/agent <id>` | Switch to that agent, using its assignment or its saved override. |
| `/new [agent]` | Start a fresh conversation on that agent's assignment. It does not carry over the previous session's overrides. |

Overrides are saved **per (session, agent)**. Switch away and back, or reopen the session, and
they come back. They never change the global `config.json`.

A subagent with its own model uses that assignment. An unassigned subagent inherits its parent's
**effective** model and variant, including local overrides.

## Pickers

Model pickers group models by gateway, then by provider within it (`omniroute`, then `cc`,
`combo`, and so on). Groups collapse, but search includes hidden groups. In VS Code only the first
50 matches are shown. "Usable" means a valid configuration, not an online check.
