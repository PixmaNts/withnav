import type { ChildInfo } from "../chatProtocol";

// Pure helpers of the session header and the transcript's action lines. No DOM here, so the
// unit tests can exercise them directly.

import { ranked } from "./pickerSearch";

/** The crew an agent belongs to (its colour and glyph), if it has one. */
const CREW_ALIASES: Record<string, string> = { build: "nav", plan: "nav", explore: "vigie", librarian: "vigie", review: "lux", oracle: "lux", explorer: "mousse" };
const CREW = new Set(["nav", "vigie", "lux", "mousse", "cartographe", "sondeur"]);
export function crewOf(agent: string): string | undefined {
  const id = CREW_ALIASES[agent] ?? agent;
  return CREW.has(id) ? id : undefined;
}

/** The agent picker's rows: the search text filters and ranks, favourites come first, the rest keeps its order. */
export function orderAgents<T extends { id: string; description?: string }>(items: T[], query: string, favorites: ReadonlySet<string>): T[] {
  const matches = ranked(items, query, item => item.id, item => item.description ?? "");
  return [...matches.filter(item => favorites.has(item.id)), ...matches.filter(item => !favorites.has(item.id))];
}

export type ChildGroup = "running" | "error" | "done";
export type ChildFilter = "all" | "running" | "done";

/** How many subagent chips the strip shows before the rest go into the dropdown. */
export const MAX_STRIP_CHIPS = 3;

/** Running covers everything that is still alive, including a subagent waiting on the Captain. */
export function childGroup(state: ChildInfo["state"]): ChildGroup {
  switch (state) {
    case "failed": return "error";
    case "done": case "stopped": return "done";
    default: return "running";
  }
}

export const childStateLabels: Record<ChildInfo["state"], string> = {
  unknown: "Unknown", running: "Running", permission: "Waiting for permission", budget: "Out of steps",
  watchdog: "In doubt · watchdog paused", done: "Done", failed: "Failed", stopped: "Stopped",
};

/** The chips the strip shows: running ones first, then the most recent (the server lists children oldest first). */
export function stripChildren(items: ChildInfo[], max = MAX_STRIP_CHIPS): { shown: ChildInfo[]; hidden: number } {
  const ranked = items
    .map((child, index) => ({ child, index }))
    .sort((a, b) => Number(childGroup(a.child.state) !== "running") - Number(childGroup(b.child.state) !== "running") || b.index - a.index)
    .map(entry => entry.child);
  return { shown: ranked.slice(0, max), hidden: Math.max(0, ranked.length - max) };
}

const GROUP_ORDER: ChildGroup[] = ["running", "error", "done"];

/** The dropdown's rows grouped Running / Error / Done, newest first inside a group; empty groups are left out. */
export function filterChildren(items: ChildInfo[], filter: ChildFilter): { group: ChildGroup; items: ChildInfo[] }[] {
  return GROUP_ORDER
    .filter(group => filter === "all" || (filter === "running" ? group === "running" : group !== "running"))
    .map(group => ({ group, items: items.filter(child => childGroup(child.state) === group).reverse() }))
    .filter(entry => entry.items.length > 0);
}

export function countChildren(items: ChildInfo[]): { all: number; running: number; done: number } {
  const running = items.filter(child => childGroup(child.state) === "running").length;
  return { all: items.length, running, done: items.length - running };
}

export function footerText(items: ChildInfo[]): string {
  const { all, running } = countChildren(items);
  return `${all} ${all === 1 ? "agent" : "agents"} · ${running} running`;
}

/** The goal on one line, for chips and rows. */
export function oneLine(text: string, max = 200): string {
  const line = text.replace(/\s+/g, " ").trim();
  return line.length > max ? `${line.slice(0, max - 1)}…` : line;
}

const plural = (count: number, one: string, many: string): string => `${count} ${count === 1 ? one : many}`;

/** "3 actions · read 2 files · 1 search": what a run of consecutive tool calls did. */
export function actionsSummary(calls: { name: string }[]): string {
  const counts = { read: 0, search: 0, edit: 0, run: 0, other: 0 };
  for (const call of calls) {
    switch (call.name) {
      case "read": counts.read++; break;
      case "grep": case "glob": counts.search++; break;
      case "edit": case "write": counts.edit++; break;
      case "bash": counts.run++; break;
      default: counts.other++;
    }
  }
  const parts: string[] = [];
  if (counts.read) parts.push(`read ${plural(counts.read, "file", "files")}`);
  if (counts.search) parts.push(plural(counts.search, "search", "searches"));
  if (counts.edit) parts.push(`edited ${plural(counts.edit, "file", "files")}`);
  if (counts.run) parts.push(`ran ${plural(counts.run, "command", "commands")}`);
  if (counts.other) parts.push(plural(counts.other, "other", "other"));
  return [plural(calls.length, "action", "actions"), ...parts].join(" · ");
}

/** "8s", "1m 05s", "1h 02m". */
export function durationLabel(ms: number): string {
  const seconds = Math.max(0, Math.round(ms / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ${String(seconds % 60).padStart(2, "0")}s`;
  return `${Math.floor(minutes / 60)}h ${String(minutes % 60).padStart(2, "0")}m`;
}
