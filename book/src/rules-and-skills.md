# Rules and skills

## Rules

WithNav reads instruction files when a session starts and sends them to the model. Later text is
closer to your work and wins. The order is:

1. Your global file: `~/.agents/AGENTS.md`, or `~/.claude/CLAUDE.md` if the first is missing.
2. The optional global supplement `~/.agents/rules.md`.
3. For each directory from the repository root down to the workspace: `AGENTS.md`, or `CLAUDE.md`
   if there is no `AGENTS.md`, then the optional `rules.md`.

The repository root is the nearest parent folder with a `.git` directory or file. Without git, it
is the workspace itself, so nothing above it is read. Each file is read once, and the size of each
file and of the total is capped.

## Skills

A skill is a folder with a `SKILL.md` file. WithNav finds skills when a session starts, lists them
to the model and adds the `skill` tool to load one by id. In the chat, `/skills` lets you pick a
skill to load for your next message.

It looks in these places, nearest first:

1. For each directory from the workspace up to the repository root: `.agents/skills`,
   `.claude/skills`, `.opencode/skills`.
2. Your `~/.agents/skills`, then `~/.claude/skills`.

If two skills share an id, the nearer scope wins. At the same distance, `.agents` beats `.claude`,
which beats `.opencode`, and your own `.agents` beats your `.claude`. If two skills of equal rank
have the same id, neither is offered.

A skill loads only once you have validated its current content; a skill that changes asks again.
Its scripts are ordinary commands: they go through your [permission rules](permissions.md) like any
other call. See [Validated skills and flows](permissions.md#validated-skills-and-flows).
