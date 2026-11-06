// @vitest-environment jsdom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import { SecretCard, SecretsList, type SecretActions } from "./SecretCard";
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
