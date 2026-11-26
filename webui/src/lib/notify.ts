// Browser notifications for permission requests the Captain has not answered.
//
// `GET /v1/waiting` lists one entry per blocked turn. A notification is keyed on
// `session_id` + `turn_id`: a review changing state, or the same card re-listed on the next poll,
// never raises a second one, and a notification closes as soon as its entry leaves the list
// (answered in any client, cancelled, or the session ended).
import type { WaitingEntry } from "../api/types";

/** The part of a browser `Notification` this module uses. */
export interface NotificationLike {
  close(): void;
  onclick: ((event: Event) => unknown) | null;
}

export interface NotifierDeps {
  create: (title: string, options: { tag: string; body: string }) => NotificationLike;
  /** A muted session raises none, and loses the one it showed. */
  isMuted: (sessionId: string) => boolean;
  /** True while the Captain is already looking at the request: nothing is raised yet. */
  quiet?: (entry: WaitingEntry) => boolean;
  /** The Captain clicked a notification. */
  onOpen?: (entry: WaitingEntry) => void;
}

/** One notification per blocked turn: the browser also replaces a notification carrying the same tag. */
export const notificationTag = (entry: Pick<WaitingEntry, "session_id" | "turn_id">) => `${entry.session_id}:${entry.turn_id}`;

/** What the notification says: where, and the first words of the call. The arguments never appear. */
export function notificationBody(entry: WaitingEntry): string {
  const where = entry.title?.trim() || "a conversation";
  return `${where}: ${entry.summary}`;
}

export function createNotifier(deps: NotifierDeps) {
  const shown = new Map<string, NotificationLike>();
  return {
    /** Reconciles the notifications with the current waiting list. */
    sync(entries: WaitingEntry[]): void {
      const wanted = new Set<string>();
      for (const entry of entries) {
        if (deps.isMuted(entry.session_id)) continue;
        const tag = notificationTag(entry);
        wanted.add(tag);
        if (shown.has(tag) || deps.quiet?.(entry)) continue;
        const note = deps.create("Nav is waiting for you", { tag, body: notificationBody(entry) });
        note.onclick = () => deps.onOpen?.(entry);
        shown.set(tag, note);
      }
      for (const [tag, note] of shown) {
        if (wanted.has(tag)) continue;
        note.close();
        shown.delete(tag);
      }
    },
    /** Closes every notification still showing. */
    closeAll(): void {
      for (const note of shown.values()) note.close();
      shown.clear();
    },
  };
}

const MUTE_KEY = "withnav.notify.muted";

/** The sessions the Captain muted. A blocked storage reads as nothing muted. */
export function readMuted(storage: Pick<Storage, "getItem">): Set<string> {
  try {
    const parsed: unknown = JSON.parse(storage.getItem(MUTE_KEY) ?? "[]");
    return new Set(Array.isArray(parsed) ? parsed.filter((id): id is string => typeof id === "string") : []);
  } catch {
    return new Set();
  }
}

/** Mutes or unmutes one session and returns the new set; the set is kept when the write fails. */
export function setMuted(storage: Pick<Storage, "getItem" | "setItem">, sessionId: string, muted: boolean): Set<string> {
  const next = readMuted(storage);
  if (muted) next.add(sessionId);
  else next.delete(sessionId);
  try {
    storage.setItem(MUTE_KEY, JSON.stringify([...next]));
  } catch {
    /* Storage is blocked or full: the choice lasts until the page reloads. */
  }
  return next;
}
