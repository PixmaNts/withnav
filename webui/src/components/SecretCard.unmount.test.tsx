// @vitest-environment jsdom
// A secret card removed while a save, a decline, a revoke or a clipboard read is in flight makes no
// state update once it is gone. React ignores such updates silently, so every state setter of the
// card is counted here to see them.
import { act, StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, expect, it, vi } from "vitest";
import { SecretCard, type SecretActions } from "./SecretCard";
import type { Question } from "../session/reducer";

const tracked = vi.hoisted(() => ({ gone: false, lateUpdates: 0 }));
vi.mock("react", async (original) => {
  const react = await original<typeof import("react")>();
  return {
    ...react,
    useState: (initial: unknown) => {
      const [value, set] = react.useState(initial);
      return [value, (next: unknown) => { if (tracked.gone) tracked.lateUpdates++; set(next); }];
    },
  };
});

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
afterEach(() => { tracked.gone = false; tracked.lateUpdates = 0; vi.restoreAllMocks(); });

const question: Question = { type: "question", text: "Nav asks", questionCallId: "sec_1", agent: "build",
  secret: { name: "API_KEY", label: "Deploy key", reason: "the staging deploy", defaultTtl: "24h", defaultScope: "session" } };
const saved = { name: "API_KEY", scope: "session" as const, ttl: "1h" as const, created_at: "", use_count: 0, last4: "4a68", expires_at: "2099-01-01T00:00:00Z" };

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((ok, fail) => { resolve = ok; reject = fail; });
  return { promise, resolve, reject };
}

async function mount(actions: SecretActions, q: Question = question) {
  const host = document.createElement("div");
  const root = createRoot(host);
  await act(async () => root.render(<StrictMode><SecretCard question={q} actions={actions} /></StrictMode>));
  const button = (label: string) => [...host.querySelectorAll<HTMLButtonElement>("button")].find((b) => b.textContent === label || b.getAttribute("aria-label") === label)!;
  const gone = async () => { tracked.gone = true; await act(async () => root.unmount()); };
  return { host, button, gone };
}

function typeValue(host: HTMLElement, text: string) {
  const input = host.querySelector<HTMLInputElement>(".secret-field input")!;
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, text);
  input.dispatchEvent(new Event("input", { bubbles: true }));
}

const actions = (overrides: Partial<SecretActions>): SecretActions => ({
  list: [], submit: vi.fn(async () => saved), decline: vi.fn(async () => undefined), revoke: vi.fn(async () => undefined), ...overrides,
});

it("a save that finishes or fails after the card is gone updates nothing", async () => {
  for (const outcome of ["resolve", "reject"] as const) {
    const answer = deferred<typeof saved>();
    const { host, button, gone } = await mount(actions({ submit: vi.fn(() => answer.promise) }));
    await act(async () => typeValue(host, "sk-live-7f3a9c2e1d0b4a68"));
    await act(async () => button("Save").click());
    await gone();
    await act(async () => { if (outcome === "resolve") answer.resolve(saved); else answer.reject(new Error("refused")); });
    expect(tracked.lateUpdates).toBe(0);
    tracked.gone = false;
  }
});

it("a clipboard read that answers after the card is gone updates nothing", async () => {
  for (const outcome of ["resolve", "reject"] as const) {
    const read = deferred<string>();
    Object.defineProperty(navigator, "clipboard", { value: { readText: () => read.promise }, configurable: true });
    const focus = vi.spyOn(HTMLInputElement.prototype, "focus");
    const { button, gone } = await mount(actions({}));
    await act(async () => button("Paste from the clipboard").click());
    await gone();
    await act(async () => { if (outcome === "resolve") read.resolve("pasted-value"); else read.reject(new Error("denied")); });
    expect(tracked.lateUpdates).toBe(0);
    expect(focus).not.toHaveBeenCalled();
    tracked.gone = false;
  }
});

it("a decline or a revoke that settles after the card is gone updates nothing", async () => {
  const declined = deferred<void>();
  const card = await mount(actions({ decline: vi.fn(() => declined.promise) }));
  await act(async () => card.button("Decline").click());
  await card.gone();
  await act(async () => declined.reject(new Error("gone")));
  expect(tracked.lateUpdates).toBe(0);
  tracked.gone = false;

  const revoked = deferred<void>();
  const stored: Question = { ...question, secretNote: { requestId: "sec_1", name: "API_KEY", outcome: "stored", scope: "session", expiresAt: "2099-01-01T00:00:00Z" } };
  const savedCard = await mount(actions({ list: [saved], revoke: vi.fn(() => revoked.promise) }), stored);
  await act(async () => savedCard.button("Revoke").click());
  await savedCard.gone();
  await act(async () => revoked.resolve());
  expect(tracked.lateUpdates).toBe(0);
});

it("a save and a paste that finish while the card is shown still update it", async () => {
  const answer = deferred<typeof saved>();
  const read = deferred<string>();
  Object.defineProperty(navigator, "clipboard", { value: { readText: () => read.promise }, configurable: true });
  const focus = vi.spyOn(HTMLInputElement.prototype, "focus");
  const { host, button, gone } = await mount(actions({ submit: vi.fn(() => answer.promise) }));
  await act(async () => button("Paste from the clipboard").click());
  await act(async () => read.resolve("sk-live-7f3a9c2e1d0b4a68\n"));
  expect(host.querySelector<HTMLInputElement>(".secret-field input")!.value).toBe("sk-live-7f3a9c2e1d0b4a68");
  expect(focus).toHaveBeenCalledTimes(1);
  await act(async () => button("Save").click());
  await act(async () => answer.resolve(saved));
  expect(host.querySelector(".secret-status")!.textContent).toBe("saved");
  expect(host.textContent).toContain("$API_KEY ••••4a68");
  await gone();
});
