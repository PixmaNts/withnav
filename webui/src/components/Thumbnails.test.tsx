// @vitest-environment jsdom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";

const attachment = vi.hoisted(() => vi.fn(async (_session: string, _id: string) => new Blob(["x"])));
vi.mock("../api/client", () => ({ api: { attachment } }));
import { ImageProvider, THUMB_MARGIN, Thumbnails } from "./Images";
import type { ImageRef } from "../api/types";

interface Observed {
  callback: IntersectionObserverCallback;
  options: IntersectionObserverInit | undefined;
  targets: Element[];
  disconnected: boolean;
}

let observers: Observed[] = [];

class FakeObserver {
  observed: Observed;
  constructor(callback: IntersectionObserverCallback, options?: IntersectionObserverInit) {
    this.observed = { callback, options, targets: [], disconnected: false };
    observers.push(this.observed);
  }
  observe(target: Element) {
    this.observed.targets.push(target);
  }
  disconnect() {
    this.observed.disconnected = true;
  }
  unobserve() {}
  takeRecords() {
    return [];
  }
}

const images: ImageRef[] = ["a", "b", "c"].map((id) => ({ attachment_id: id, name: `${id}.png` }) as ImageRef);

// The URL cache outlives a test: each test shows its own session so none starts with a cached picture.
async function mount(sessionId: string) {
  const transcript = document.createElement("div");
  transcript.className = "transcript";
  document.body.append(transcript);
  const root = createRoot(transcript);
  await act(async () => root.render(<ImageProvider sessionId={sessionId}><Thumbnails images={images} /></ImageProvider>));
  return { transcript, unmount: () => act(async () => root.unmount()) };
}

function cross(observed: Observed, isIntersecting: boolean) {
  const entries = observed.targets.map((target) => ({ target, isIntersecting }) as IntersectionObserverEntry);
  return act(async () => observed.callback(entries, {} as IntersectionObserver));
}

beforeEach(() => {
  observers = [];
  attachment.mockClear();
  vi.stubGlobal("URL", { createObjectURL: () => "blob:x", revokeObjectURL: () => {} });
});
afterEach(() => {
  vi.unstubAllGlobals();
  document.body.replaceChildren();
});

// Given a conversation showing three thumbnails, none of them near the visible part yet
// Then nothing is fetched; the one that comes near is fetched alone
it("fetches a thumbnail only once it comes near the visible part of the conversation", async () => {
  vi.stubGlobal("IntersectionObserver", FakeObserver);
  const { transcript, unmount } = await mount("s1");
  expect(attachment).not.toHaveBeenCalled();
  expect(observers).toHaveLength(3);
  for (const o of observers) expect(o.options).toEqual({ root: transcript, rootMargin: THUMB_MARGIN });

  await cross(observers[1]!, false);
  expect(attachment).not.toHaveBeenCalled();

  await cross(observers[1]!, true);
  expect(attachment.mock.calls).toEqual([["s1", "b"]]);
  expect(observers[1]!.disconnected).toBe(true);
  await unmount();
});

// Given a browser without IntersectionObserver
// Then every thumbnail loads at once, as before
it("loads every thumbnail when the browser cannot observe", async () => {
  vi.stubGlobal("IntersectionObserver", undefined);
  const { unmount } = await mount("s2");
  expect(attachment.mock.calls).toEqual([["s2", "a"], ["s2", "b"], ["s2", "c"]]);
  await unmount();
});
