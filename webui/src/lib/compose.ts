// How a typed line is sent, with the server's rule (same as the terminal
// client, `crates/withnav-tui/src/commands.rs::route`): only a line whose very first
// character is `/` is a command, `//text` sends the literal `/text`, the name
// runs to the first whitespace and the rest, trimmed, is the arguments.
// `/skill <name> [request]` is a message carrying a skill selection.

import type { ContentBlock } from "../api/types";

/** Client commands: opened locally rather than sent to the server. */
export const CLIENT_COMMANDS = ["new", "model", "skills", "theme"] as const;
export type ClientCommand = (typeof CLIENT_COMMANDS)[number];

export type Routed =
  | { type: "message"; content: ContentBlock[]; text: string }
  | { type: "client"; name: ClientCommand; arguments: string }
  | { type: "server"; name: string; arguments: string }
  | { type: "refused"; reason: string };

/**
 * The first line of a delegated message. Cognition guides by default and only
 * implements when the Captain explicitly delegates (`src/navigator/cognition.md`);
 * the Delegate toggle states that in so many words, and the transcript reads it
 * back to label the message.
 */
export const DELEGATION_LINE = "Delegation: do this work yourself.";

export function route(line: string, delegate = false): Routed {
  const message = (text: string, skill?: string): Routed => {
    const body = delegate ? `${DELEGATION_LINE}\n\n${text}` : text;
    const content: ContentBlock[] = [{ type: "text", text: body }];
    if (skill) content.push({ type: "skill", name: skill });
    return { type: "message", content, text: body };
  };
  if (line.startsWith("//")) {
    const rest = line.slice(2);
    // `//skill …` stays escaped all the way: it must never become a selection.
    if (rest === "skill" || /^skill\s/.test(rest)) return message(line);
    return message(`/${rest}`);
  }
  if (!line.startsWith("/")) return message(line);
  const rest = line.slice(1);
  const split = /\s/.exec(rest);
  const name = split ? rest.slice(0, split.index) : rest;
  const args = split ? rest.slice(split.index).trim() : "";
  if (!name) return message(line);
  if (name === "skill") {
    if (!args) return { type: "refused", reason: "usage: /skill <name> [request]" };
    const at = /\s/.exec(args);
    const skill = at ? args.slice(0, at.index) : args;
    const request = at ? args.slice(at.index).trim() : "";
    return message(request || `Apply the ${skill} skill.`, skill);
  }
  if ((CLIENT_COMMANDS as readonly string[]).includes(name)) {
    // With an argument `/model <id>` switches straight away on the server.
    if (name === "model" && args) return { type: "server", name, arguments: args };
    if (name === "new" && args) return { type: "client", name: "new", arguments: args };
    if (args) return { type: "refused", reason: `/${name} takes no arguments` };
    return { type: "client", name: name as ClientCommand, arguments: "" };
  }
  return { type: "server", name, arguments: args };
}

/**
 * The title to show for a session. The server titles a session after its first
 * message, so a delegated first message would otherwise show the delegation line.
 */
export function sessionTitle(title: string | null | undefined): string {
  const text = title ? readDelegation(title).text : "";
  return text || "Untitled session";
}

/** Splits a stored Captain message back into its delegation flag and text. */
export function readDelegation(text: string): { delegated: boolean; text: string } {
  if (text.startsWith(DELEGATION_LINE)) return { delegated: true, text: text.slice(DELEGATION_LINE.length).replace(/^\s*\n/, "").trimStart() };
  return { delegated: false, text };
}
