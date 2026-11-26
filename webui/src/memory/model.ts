// Pure helpers of the memory and projects pages: wording of authority and scope, relative dates,
// the word diff between two versions, and reading the register's refusals.

import { HttpError } from "../api/http";
import type { MemoryGroup, MemoryItem, MemoryProject, MemoryScope, MemoryVersion, WideningBody } from "../api/types";

export const SCOPES: MemoryScope[] = ["global", "group", "project", "session"];
export const KINDS = ["decision", "convention", "preference", "environment", "pitfall"] as const;
export const STATUSES = ["active", "proposed", "invalidated", "rejected"] as const;

/** Who stands behind a wording, from the provenance the register stores with each version. */
export type Authority = "captain said" | "captain accepted" | "observed" | "assumed";

export function authority(v: Pick<MemoryVersion, "source_actor" | "drafted_by" | "confirmed_by">): Authority {
  if (v.source_actor === "captain" && v.drafted_by === "captain") return "captain said";
  if (v.confirmed_by === "captain") return "captain accepted";
  if (v.source_actor === "host") return "observed";
  return "assumed";
}

export const AUTHORITY_HINT: Record<Authority, string> = {
  "captain said": "The Captain wrote this himself.",
  "captain accepted": "Nav drafted this wording and the Captain accepted it.",
  observed: "WithNav saw this happen; nobody said it.",
  assumed: "Nav inferred this and the Captain has not confirmed it.",
};

/** The author of one version, in the words of the timeline. */
export function authorLine(v: Pick<MemoryVersion, "source_actor" | "drafted_by" | "confirmed_by">): string {
  if (v.drafted_by === "captain") return "the Captain";
  if (v.drafted_by === "host") return "WithNav";
  if (v.confirmed_by === "captain") return "proposed by Nav, accepted by the Captain";
  return "proposed by Nav";
}

export const shortId = (id: string): string => (id.length > 10 ? id.slice(-8) : id);

/** The name of what a scope points to, when the register's projects and groups name it. */
export function scopeLabel(scope: MemoryScope, ref: string, groups: MemoryGroup[], projects: MemoryProject[]): string {
  if (scope === "global") return "global";
  if (scope === "group") return `group ${groups.find((g) => g.id === ref)?.name ?? shortId(ref)}`;
  if (scope === "project") return `project ${projects.find((p) => p.id === ref)?.name ?? shortId(ref)}`;
  return `session ${shortId(ref)}`;
}

const UNITS: [number, string][] = [
  [60_000, "minute"],
  [3_600_000, "hour"],
  [86_400_000, "day"],
];

/** "just now", "5 minutes ago", "3 days ago"; older than 30 days is a date. */
export function ago(iso: string | null | undefined, now = Date.now()): string {
  if (!iso) return "";
  const at = new Date(iso).getTime();
  if (Number.isNaN(at)) return "";
  const diff = Math.max(0, now - at);
  if (diff < 60_000) return "just now";
  if (diff >= 30 * 86_400_000) return new Date(at).toISOString().slice(0, 10);
  let label = "";
  for (const [size, name] of UNITS) {
    if (diff >= size) {
      const n = Math.floor(diff / size);
      label = `${n} ${name}${n === 1 ? "" : "s"} ago`;
    }
  }
  return label;
}

export const exactTime = (iso: string): string => {
  const at = new Date(iso);
  return Number.isNaN(at.getTime()) ? iso : at.toISOString().replace("T", " ").slice(0, 16) + " UTC";
};

// ---------- diff ----------

export interface DiffPart {
  kind: "same" | "add" | "del";
  text: string;
}

const DIFF_CELLS = 250_000;

/** A word diff: what `after` removed from `before` and what it added. Whitespace is kept as tokens. */
export function diffText(before: string, after: string): DiffPart[] {
  const a = before.split(/(\s+)/).filter((t) => t !== "");
  const b = after.split(/(\s+)/).filter((t) => t !== "");
  const out: DiffPart[] = [];
  const push = (kind: DiffPart["kind"], text: string) => {
    const last = out[out.length - 1];
    if (last?.kind === kind) last.text += text;
    else out.push({ kind, text });
  };
  // A text too long for the table is shown as replaced whole, never skipped.
  if (a.length * b.length > DIFF_CELLS) {
    if (before) push("del", before);
    if (after) push("add", after);
    return out;
  }
  const lcs: number[][] = Array.from({ length: a.length + 1 }, () => new Array<number>(b.length + 1).fill(0));
  for (let i = a.length - 1; i >= 0; i--)
    for (let j = b.length - 1; j >= 0; j--) lcs[i]![j] = a[i] === b[j] ? lcs[i + 1]![j + 1]! + 1 : Math.max(lcs[i + 1]![j]!, lcs[i]![j + 1]!);
  let i = 0;
  let j = 0;
  while (i < a.length && j < b.length) {
    if (a[i] === b[j]) {
      push("same", a[i]!);
      i++;
      j++;
    } else if (lcs[i + 1]![j]! >= lcs[i]![j + 1]!) push("del", a[i++]!);
    else push("add", b[j++]!);
  }
  while (i < a.length) push("del", a[i++]!);
  while (j < b.length) push("add", b[j++]!);
  return out;
}

// ---------- refusals ----------

/** The server is not there: nothing answered. */
export const unreachable = (error: unknown): boolean => error instanceof HttpError && error.status === 0;

/** A stale write: the item moved on, to this version. */
export function versionConflict(error: unknown): { current: number } | null {
  if (error instanceof HttpError && error.status === 409 && error.reason === "version_conflict" && typeof error.body?.current_version === "number")
    return { current: error.body.current_version };
  return null;
}

/** A write that lets more projects read something: they are listed, and the Captain confirms. */
export function widening(error: unknown): MemoryProject[] | null {
  if (error instanceof HttpError && error.status === 409 && error.reason === "widening_requires_confirmation") {
    const body = error.body as Partial<WideningBody> | undefined;
    return body?.affected_projects ?? [];
  }
  return null;
}

export function describeError(error: unknown): string {
  if (error instanceof HttpError) {
    if (error.status === 0) return "The WithNav server is unreachable.";
    if (error.status === 503) return "The memory register is unavailable.";
    if (error.status === 404) return "That item no longer exists.";
    return error.body?.message ?? error.message;
  }
  return error instanceof Error ? error.message : String(error);
}

// ---------- list ----------

export const searchText = (item: MemoryItem): string => `${item.current.body} ${item.current.quote ?? ""} ${item.current.why ?? ""} ${item.kind}`;
