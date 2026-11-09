// @vitest-environment jsdom
import { act, StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import { SecretCard, SecretsList, SecretsMenu, type SecretActions } from "./SecretCard";
import { emptyView, reduce, waitingSecret, type Question } from "../session/reducer";
import type { Envelope } from "../api/types";

const VALUE = "sk-live-7f3a9c2e1d0b4a68";
const event = (seq: number, kind: string, payload: object): Envelope => ({ schema_version: 1, event_id: String(seq), causation_id: null, correlation_id: null, seq, kind, payload, occurred_at: "2026-10-10", session_id: "S", actor_id: "agent:main" });
const yieldPayload = { turn_id: "T", question_call_id: "sec_1", kind: "secret", question: "Nav asks for a secret",
  details: { name: "API_KEY", label: "Deploy key", reason: "the staging deploy", default_ttl: "1h", default_scope: "workspace" } };

it("a secret yield becomes a waiting card and its note settles it without a Captain bubble", () => {
  let view = reduce(emptyView(), event(1, "turn.start", { turn_id: "T", task_id: "K" }));
  view = reduce(view, event(2, "agent.yield", yieldPayload));
  const card = waitingSecret(view)!;
  expect(card.secret).toEqual({ name: "API_KEY", label: "Deploy key", reason: "the staging deploy", defaultTtl: "1h", defaultScope: "workspace" });
  view = reduce(view, event(3, "message.received", { command_id: "c", source: "user:http", target: "agent:main", mode: "next-turn", content: "stored as $API_KEY",
    secret: { request_id: "sec_1", name: "API_KEY", outcome: "stored", scope: "workspace", ttl: "1h", expires_at: "2099-01-01T00:00:00Z" } }));
  expect(waitingSecret(view)).toBeUndefined();
  expect(view.items.some((item) => item.kind === "captain")).toBe(false);
  // An `ask` cannot raise a secret card.
  const ask = reduce(reduce(emptyView(), event(1, "turn.start", { turn_id: "T" })), event(2, "agent.yield", { ...yieldPayload, kind: "ask" }));
  expect(waitingSecret(ask)).toBeUndefined();
});

function actions(overrides: Partial<SecretActions> = {}): SecretActions {
  return {
    list: [],
    submit: vi.fn(async () => ({ name: "API_KEY", scope: "workspace" as const, ttl: "1h" as const, expires_at: new Date(Date.now() + 3_600_000).toISOString(), created_at: "", use_count: 0, last4: "4a68" })),
    decline: vi.fn(async () => undefined),
    revoke: vi.fn(async () => undefined),
    ...overrides,
  };
}
const question: Question = { type: "question", text: "Nav asks", questionCallId: "sec_1", agent: "build",
  secret: { name: "API_KEY", label: "Deploy key", reason: "the staging deploy", defaultTtl: "24h", defaultScope: "session" } };

function type(input: HTMLInputElement, text: string) {
  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
  setter.call(input, text);
  input.dispatchEvent(new Event("input", { bubbles: true }));
}

it("masks the value, sends it once with the chosen availability and scope, then clears it", async () => {
  const host = document.createElement("div");
  const root = createRoot(host);
  const acts = actions();
  await act(async () => root.render(<SecretCard question={question} actions={acts} />));
  expect(host.querySelector(".secret-status")!.textContent).toBe("waiting for your answer");
  expect(host.textContent).toContain("For the staging deploy. Requested by build.");
  expect(host.textContent).toContain("Kept out of the chat. The agent receives $API_KEY, not the value.");
  const input = host.querySelector<HTMLInputElement>(".secret-field input")!;
  expect(input.type).toBe("password");
  const save = [...host.querySelectorAll<HTMLButtonElement>("button")].find((b) => b.textContent === "Save")!;
  expect(save.disabled).toBe(true);
  await act(async () => type(input, VALUE));
  expect(save.disabled).toBe(false);
  await act(async () => host.querySelector<HTMLButtonElement>('[aria-label="Show the value"]')!.click());
  expect(input.type).toBe("text");
  await act(async () => [...host.querySelectorAll<HTMLButtonElement>(".secret-seg button")].find((b) => b.textContent === "1 h")!.click());
  await act(async () => [...host.querySelectorAll<HTMLButtonElement>(".secret-seg button")].find((b) => b.textContent === "This workspace")!.click());
  await act(async () => save.click());
  expect(acts.submit).toHaveBeenCalledTimes(1);
  expect(acts.submit).toHaveBeenCalledWith("sec_1", { value: VALUE, ttl: "1h", scope: "workspace" });
  expect(host.innerHTML).not.toContain(VALUE);
  expect(host.querySelector(".secret-card")!.getAttribute("data-state")).toBe("saved");
  expect(host.textContent).toContain("$API_KEY ••••4a68");
  expect(host.textContent).toMatch(/expires in (60 min|1 h), this workspace/);
  await act(async () => [...host.querySelectorAll<HTMLButtonElement>("button")].find((b) => b.textContent === "Revoke")!.click());
  expect(acts.revoke).toHaveBeenCalledWith("API_KEY");
  expect(host.querySelector(".secret-card")!.getAttribute("data-state")).toBe("revoked");
  await act(async () => root.unmount());
});

it("declines, offers Provide anyway, and the header list revokes", async () => {
  const host = document.createElement("div");
  const root = createRoot(host);
  const acts = actions();
  await act(async () => root.render(<SecretCard question={question} actions={acts} />));
  await act(async () => [...host.querySelectorAll<HTMLButtonElement>("button")].find((b) => b.textContent === "Decline")!.click());
  expect(acts.decline).toHaveBeenCalledWith("sec_1");
  await act(async () => root.render(<SecretCard question={{ ...question, secretNote: { requestId: "sec_1", name: "API_KEY", outcome: "declined" } }} actions={acts} />));
  expect(host.textContent).toContain("Nav was told and continues without it.");
  await act(async () => [...host.querySelectorAll<HTMLButtonElement>("button")].find((b) => b.textContent === "Provide anyway")!.click());
  await act(async () => type(host.querySelector<HTMLInputElement>(".secret-field input")!, VALUE));
  await act(async () => [...host.querySelectorAll<HTMLButtonElement>("button")].find((b) => b.textContent === "Save")!.click());
  expect(acts.submit).toHaveBeenLastCalledWith("sec_1", { name: "API_KEY", value: VALUE, ttl: "24h", scope: "session" });
  const onRevoke = vi.fn();
  await act(async () => root.render(<SecretsList list={[{ name: "API_KEY", scope: "session", ttl: "24h", expires_at: new Date(Date.now() + 7_200_000).toISOString(), created_at: "", use_count: 3, last4: "4a68" }]} onRevoke={onRevoke} />));
  expect(host.textContent).toContain("$API_KEY ••••4a68");
  expect(host.textContent).toContain("expires in 2 h · this session · used 3×");
  await act(async () => host.querySelector<HTMLButtonElement>('[aria-label="Revoke API_KEY"]')!.click());
  expect(onRevoke).toHaveBeenCalledWith("API_KEY");
  await act(async () => root.unmount());
});

it("a list older than the stored note does not read as a revocation", async () => {
  const host = document.createElement("div");
  const root = createRoot(host);
  const stale = actions({ list: [] });
  const stored: Question = { ...question, secretNote: { requestId: "sec_1", name: "API_KEY", outcome: "stored", scope: "session", ttl: "1h", expiresAt: new Date(Date.now() + 3_600_000).toISOString() } };
  await act(async () => root.render(<SecretCard question={stored} actions={stale} />));
  expect(host.querySelector(".secret-card")!.getAttribute("data-state")).toBe("saved");
  // A list fetched afterwards without the name: the secret went.
  await act(async () => root.render(<SecretCard question={stored} actions={{ ...stale, list: [] }} />));
  expect(host.querySelector(".secret-card")!.getAttribute("data-state")).toBe("revoked");
  await act(async () => root.unmount());
});

const key = (target: Element, k: string) => target.dispatchEvent(new KeyboardEvent("keydown", { key: k, bubbles: true, cancelable: true }));

it("a radio group keeps only the chosen option in the tab order and moves the choice with the arrow keys", async () => {
  const host = document.body.appendChild(document.createElement("div"));
  const root = createRoot(host);
  const acts = actions();
  await act(async () => root.render(<SecretCard question={question} actions={acts} />));
  const group = host.querySelector('[role="radiogroup"][aria-label="Availability"]')!;
  const radios = () => [...group.querySelectorAll<HTMLButtonElement>('[role="radio"]')];
  const state = () => radios().map((r) => `${r.textContent}:${r.getAttribute("aria-checked")}:${r.tabIndex}`);
  expect(state()).toEqual(["1 h:false:-1", "24 h:true:0", "Session:false:-1"]);
  radios()[1]!.focus();
  await act(async () => key(document.activeElement!, "ArrowRight"));
  expect(state()).toEqual(["1 h:false:-1", "24 h:false:-1", "Session:true:0"]);
  expect(document.activeElement).toBe(radios()[2]);
  await act(async () => key(document.activeElement!, "ArrowDown"));
  expect(state()[0]).toBe("1 h:true:0");
  expect(document.activeElement).toBe(radios()[0]);
  await act(async () => key(document.activeElement!, "ArrowLeft"));
  expect(state()[2]).toBe("Session:true:0");
  await act(async () => key(document.activeElement!, "ArrowUp"));
  expect(state()[1]).toBe("24 h:true:0");
  await act(async () => type(host.querySelector<HTMLInputElement>(".secret-field input")!, VALUE));
  await act(async () => [...host.querySelectorAll<HTMLButtonElement>("button")].find((b) => b.textContent === "Save")!.click());
  expect(acts.submit).toHaveBeenCalledWith("sec_1", { value: VALUE, ttl: "24h", scope: "session" });
  await act(async () => root.unmount());
  host.remove();
});

it("a read-only card ignores the arrow keys", async () => {
  const host = document.body.appendChild(document.createElement("div"));
  const root = createRoot(host);
  await act(async () => root.render(<SecretCard question={question} actions={actions()} readOnly />));
  const radios = [...host.querySelectorAll<HTMLButtonElement>('[role="radiogroup"][aria-label="Availability"] [role="radio"]')];
  expect(radios.map((r) => r.disabled)).toEqual([true, true, true]);
  const before = document.activeElement;
  await act(async () => key(radios[1]!, "ArrowRight"));
  expect(radios.map((r) => r.getAttribute("aria-checked"))).toEqual(["false", "true", "false"]);
  expect(document.activeElement).toBe(before);
  await act(async () => root.unmount());
  host.remove();
});

it("the secrets list takes the focus when it opens and Escape returns it to the button", async () => {
  const host = document.body.appendChild(document.createElement("div"));
  const root = createRoot(host);
  const onOpen = vi.fn();
  const listed = [{ name: "API_KEY", scope: "session" as const, ttl: "24h" as const, expires_at: new Date(Date.now() + 7_200_000).toISOString(), created_at: "", use_count: 0, last4: "4a68" }];
  const render = (list: typeof listed | null) => root.render(<StrictMode><SecretsMenu list={list} onOpen={onOpen} onRevoke={vi.fn()} /></StrictMode>);
  await act(async () => render(null));
  const trigger = host.querySelector<HTMLButtonElement>('[aria-label="Secrets of this session"]')!;
  expect(trigger.getAttribute("aria-expanded")).toBe("false");
  trigger.focus();
  await act(async () => trigger.click());
  expect(onOpen).toHaveBeenCalledTimes(1);
  expect(trigger.getAttribute("aria-expanded")).toBe("true");
  const dialog = host.querySelector<HTMLElement>('[role="dialog"]')!;
  expect(dialog.textContent).toContain("Loading…");
  expect(document.activeElement).toBe(dialog);
  await act(async () => render(listed));
  const revoke = host.querySelector<HTMLButtonElement>('[aria-label="Revoke API_KEY"]')!;
  revoke.focus();
  // The revoked row leaves with its focused button: the focus stays in the list and Escape still works.
  await act(async () => render([]));
  expect(document.activeElement).toBe(dialog);
  await act(async () => render(listed));
  const again = host.querySelector<HTMLButtonElement>('[aria-label="Revoke API_KEY"]')!;
  again.focus();
  await act(async () => key(again, "Escape"));
  expect(host.querySelector('[role="dialog"]')).toBeNull();
  expect(trigger.getAttribute("aria-expanded")).toBe("false");
  expect(document.activeElement).toBe(trigger);
  await act(async () => root.unmount());
  host.remove();
});
