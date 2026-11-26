import { describe, expect, it, vi } from "vitest";
import type { WaitingEntry } from "../api/types";
import { createNotifier, notificationBody, notificationTag, readMuted, setMuted, type NotificationLike } from "./notify";

const entry = (over: Partial<WaitingEntry> = {}): WaitingEntry => ({
  session_id: "S1",
  workspace: "/w",
  title: "Release",
  turn_id: "T1",
  request_id: "R1",
  tool: "bash",
  summary: "bash git push origin v0.9.0",
  since: "2026-11-26T10:00:00Z",
  review_state: "reviewing",
  ...over,
});

function harness(over: { muted?: Set<string>; quiet?: (e: WaitingEntry) => boolean } = {}) {
  const created: { title: string; tag: string; body: string; note: NotificationLike & { close: ReturnType<typeof vi.fn> } }[] = [];
  const onOpen = vi.fn();
  const notifier = createNotifier({
    create: (title, options) => {
      const note = { close: vi.fn(), onclick: null as NotificationLike["onclick"] };
      created.push({ title, ...options, note });
      return note;
    },
    isMuted: (id) => over.muted?.has(id) ?? false,
    quiet: over.quiet,
    onOpen,
  });
  return { created, notifier, onOpen };
}

describe("waiting notifications", () => {
  it("one_notification_per_blocked_turn", () => {
    const { created, notifier } = harness();
    notifier.sync([entry()]);
    // The next poll lists the same turn again, its review now waiting: still one notification.
    notifier.sync([entry({ review_state: "wait" })]);
    notifier.sync([entry({ review_state: "wait" })]);
    expect(created).toHaveLength(1);
    expect(created[0]).toMatchObject({ title: "Nav is waiting for you", tag: "S1:T1", body: "Release: bash git push origin v0.9.0" });
    // A later turn of the same session blocks again: that is a new notification.
    notifier.sync([entry({ turn_id: "T2", request_id: "R2" })]);
    expect(created.map((c) => c.tag)).toEqual(["S1:T1", "S1:T2"]);
    expect(created[0]!.note.close).toHaveBeenCalledTimes(1);
  });

  it("a_notification_closes_when_the_entry_leaves_the_waiting_list", () => {
    const { created, notifier } = harness();
    notifier.sync([entry(), entry({ session_id: "S2", turn_id: "T9" })]);
    notifier.sync([entry({ session_id: "S2", turn_id: "T9" })]);
    expect(created[0]!.note.close).toHaveBeenCalledTimes(1);
    expect(created[1]!.note.close).not.toHaveBeenCalled();
    notifier.sync([]);
    expect(created[1]!.note.close).toHaveBeenCalledTimes(1);
    // Closed once, never again.
    notifier.sync([]);
    expect(created[0]!.note.close).toHaveBeenCalledTimes(1);
  });

  it("raises nothing for a muted session and closes the one it had showed", () => {
    const muted = new Set<string>();
    const { created, notifier } = harness({ muted });
    notifier.sync([entry({ session_id: "S2", turn_id: "T2" })]);
    muted.add("S1");
    notifier.sync([entry(), entry({ session_id: "S2", turn_id: "T2" })]);
    expect(created.map((c) => c.tag)).toEqual(["S2:T2"]);
    muted.add("S2");
    notifier.sync([entry({ session_id: "S2", turn_id: "T2" })]);
    expect(created[0]!.note.close).toHaveBeenCalledTimes(1);
  });

  it("waits while the Captain is looking at the request, then raises it", () => {
    let looking = true;
    const { created, notifier } = harness({ quiet: () => looking });
    notifier.sync([entry()]);
    expect(created).toHaveLength(0);
    looking = false;
    notifier.sync([entry()]);
    expect(created).toHaveLength(1);
  });

  it("opens the request's session when the notification is clicked", () => {
    const { created, notifier, onOpen } = harness();
    notifier.sync([entry()]);
    created[0]!.note.onclick!(new Event("click"));
    expect(onOpen).toHaveBeenCalledWith(expect.objectContaining({ session_id: "S1", turn_id: "T1" }));
  });

  it("closes every notification on closeAll", () => {
    const { created, notifier } = harness();
    notifier.sync([entry(), entry({ turn_id: "T2" })]);
    notifier.closeAll();
    expect(created.every((c) => c.note.close.mock.calls.length === 1)).toBe(true);
  });

  it("falls back to a neutral place when the session has no title, and never carries arguments", () => {
    expect(notificationBody(entry({ title: null }))).toBe("a conversation: bash git push origin v0.9.0");
    expect(notificationTag(entry())).toBe("S1:T1");
  });
});

describe("muted sessions", () => {
  const memory = () => {
    const data = new Map<string, string>();
    return { getItem: (k: string) => data.get(k) ?? null, setItem: (k: string, v: string) => void data.set(k, v) };
  };

  it("keeps a per-session mute in storage", () => {
    const storage = memory();
    expect([...readMuted(storage)]).toEqual([]);
    setMuted(storage, "S1", true);
    setMuted(storage, "S2", true);
    expect([...readMuted(storage)]).toEqual(["S1", "S2"]);
    expect([...setMuted(storage, "S1", false)]).toEqual(["S2"]);
    expect([...readMuted(storage)]).toEqual(["S2"]);
  });

  it("reads a damaged or blocked storage as nothing muted", () => {
    expect(readMuted({ getItem: () => "{not json" }).size).toBe(0);
    expect(readMuted({ getItem: () => '{"a":1}' }).size).toBe(0);
    expect(readMuted({ getItem: () => { throw new Error("blocked"); } }).size).toBe(0);
    const blocked = { getItem: () => null, setItem: () => { throw new Error("full"); } };
    expect([...setMuted(blocked, "S1", true)]).toEqual(["S1"]);
  });
});
