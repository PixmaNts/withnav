import type { CommandInfo } from "./chatProtocol";

export const CLIENT_COMMANDS: CommandInfo[] = [
  { name: "new", description: "Start a new conversation on an agent's assignment", args_hint: "[agent]" },
  { name: "help", description: "List available commands", args_hint: "" },
  { name: "skills", description: "Choose a skill for your next message", args_hint: "" },
  { name: "skill", description: "Send a request with a selected skill", args_hint: "<name> [request]" },
  { name: "stop", description: "Stop generation", args_hint: "" },
];

export type Route = { kind: "message"; text: string } | { kind: "command"; name: string; arguments: string };
/** Match the server's first-column rule; escaping never selects a skill. */
export function routeComposer(text: string): Route {
  if (text.startsWith("//")) return { kind: "message", text: text.slice(1) };
  if (!text.startsWith("/")) return { kind: "message", text };
  const match = /^\/([^\s]*)(?:\s+([\s\S]*))?$/.exec(text)!;
  return { kind: "command", name: match[1], arguments: (match[2] ?? "").trim() };
}

export function commandRows(commands: CommandInfo[], text: string): CommandInfo[] {
  if (!/^\/[^\s/]*$/.test(text)) return [];
  const filter = text.slice(1).toLowerCase();
  return commands.filter(command => command.name.toLowerCase().startsWith(filter));
}
