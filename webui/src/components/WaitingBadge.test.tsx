// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { WaitingEntry } from "../api/types";
import { WaitingBadge, WaitingContext, WaitingDot, useWaitingNotifications } from "./WaitingBadge";

const entry = (over: Partial<WaitingEntry> = {}): WaitingEntry => ({
  session_id: "S1",
  workspace: "/w",
  title: "Release",
  turn_id: "T1",
  request_id: "R1",
  tool: "bash",
  summary: "bash git push",
  since: "2026-11-26T10:00:00Z",
  review_state: null,
  ...over,
});

let container: HTMLDivElement;
let root: Root;
const render = (node: React.ReactNode) => act(async () => root.render(node));

beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  localStorage.clear();
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});
afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
  vi.unstubAllGlobals();
});

describe("waiting badge", () => {
  it("shows nothing when nothing waits", async () => {
    await render(<WaitingContext.Provider value={[]}><WaitingBadge open={vi.fn()} /></WaitingContext.Provider>);
    expect(container.textContent).toBe("");
  });

  it("counts the waiting sessions and opens one from the list", async () => {
    const open = vi.fn();
    await render(<WaitingContext.Provider value={[entry(), entry({ session_id: "S2", turn_id: "T2", title: "Docs" })]}><WaitingBadge open={open} /></WaitingContext.Provider>);
    const badge = container.querySelector<HTMLButtonElement>("[aria-label='2 waiting for you']")!;
    expect(badge.textContent).toContain("2 waiting");
    await act(async () => badge.click());
    expect([...container.querySelectorAll(".waiting-open")].map((row) => row.textContent)).toEqual(["Releasebash git push", "Docsbash git push"]);
    await act(async () => container.querySelectorAll<HTMLButtonElement>(".waiting-open")[1]!.click());
    expect(open).toHaveBeenCalledWith(expect.objectContaining({ session_id: "S2" }));
  });

  it("mutes one session in local storage and unmutes it", async () => {
    await render(<WaitingContext.Provider value={[entry()]}><WaitingBadge open={vi.fn()} /></WaitingContext.Provider>);
    await act(async () => container.querySelector<HTMLButtonElement>(".bypass-badge")!.click());
    await act(async () => container.querySelector<HTMLButtonElement>("[aria-label='Mute notifications for this session']")!.click());
    expect(JSON.parse(localStorage.getItem("withnav.notify.muted")!)).toEqual(["S1"]);
    const unmute = container.querySelector<HTMLButtonElement>("[aria-label='Notify me again for this session']")!;
    expect(unmute.getAttribute("aria-pressed")).toBe("true");
    await act(async () => unmute.click());
    expect(JSON.parse(localStorage.getItem("withnav.notify.muted")!)).toEqual([]);
  });

  it("asks for the browser's permission only from a click", async () => {
    const request = vi.fn(async () => "granted");
    vi.stubGlobal("Notification", Object.assign(vi.fn(), { permission: "default", requestPermission: request }));
    await render(<WaitingContext.Provider value={[entry()]}><WaitingBadge open={vi.fn()} /></WaitingContext.Provider>);
    expect(request).not.toHaveBeenCalled();
    await act(async () => container.querySelector<HTMLButtonElement>(".bypass-badge")!.click());
    await act(async () => [...container.querySelectorAll("button")].find((b) => b.textContent === "Notify me when I am away")!.click());
    expect(request).toHaveBeenCalledTimes(1);
  });
});

describe("waiting dot", () => {
  it("marks only the waiting session", async () => {
    await render(
      <WaitingContext.Provider value={[entry()]}>
        <WaitingDot sessionId="S1" />
        <WaitingDot sessionId="S2" />
      </WaitingContext.Provider>,
    );
    expect(container.querySelectorAll(".waiting-dot")).toHaveLength(1);
    expect(container.querySelector(".waiting-dot")!.getAttribute("aria-label")).toBe("Waiting for you");
  });
});

describe("browser notifications", () => {
  function Probe({ entries, session = null }: { entries: WaitingEntry[]; session?: string | null }) {
    useWaitingNotifications(entries, { session }, vi.fn());
    return null;
  }

  it("raises one per blocked turn, closes it when the request leaves the list, and keeps quiet for a muted session", async () => {
    const made: { tag: string; close: ReturnType<typeof vi.fn> }[] = [];
    const Fake = vi.fn(function (this: { tag: string; close: ReturnType<typeof vi.fn> }, _title: string, options: { tag: string }) {
      this.tag = options.tag;
      this.close = vi.fn();
      made.push(this);
    });
    vi.stubGlobal("Notification", Object.assign(Fake, { permission: "granted" }));
    vi.spyOn(document, "hasFocus").mockReturnValue(false);
    localStorage.setItem("withnav.notify.muted", JSON.stringify(["S3"]));
    await render(<Probe entries={[entry(), entry({ session_id: "S3", turn_id: "T3" })]} />);
    await render(<Probe entries={[entry({ review_state: "wait" }), entry({ session_id: "S3", turn_id: "T3" })]} />);
    expect(made.map((n) => n.tag)).toEqual(["S1:T1"]);
    await render(<Probe entries={[]} />);
    expect(made[0]!.close).toHaveBeenCalledTimes(1);
  });

  it("raises none for the session the Captain is looking at", async () => {
    const Fake = vi.fn();
    vi.stubGlobal("Notification", Object.assign(Fake, { permission: "granted" }));
    vi.spyOn(document, "hasFocus").mockReturnValue(true);
    await render(<Probe entries={[entry()]} session="S1" />);
    expect(Fake).not.toHaveBeenCalled();
  });

  it("raises none without the browser's permission", async () => {
    const Fake = vi.fn();
    vi.stubGlobal("Notification", Object.assign(Fake, { permission: "denied" }));
    vi.spyOn(document, "hasFocus").mockReturnValue(false);
    await render(<Probe entries={[entry()]} />);
    expect(Fake).not.toHaveBeenCalled();
  });
});
