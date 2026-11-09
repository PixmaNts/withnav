// @vitest-environment jsdom
// The secret list of a session: answers that arrive late (after a newer refresh, a mutation or a
// session change) never replace the list the Captain should see.
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { emptyView } from "./reducer";
import { useChat, type Chat } from "./useChat";
import { api } from "../api/client";
import type { SecretView } from "../api/types";

vi.mock("../api/queries", () => ({
  keys: { session: (id: string) => ["session", id], sessions: (ws: string) => ["sessions", ws] },
  useSessionSummary: (id: string | null) => ({ data: id ? { session_id: id, actor_state: "idle", workspace: "/work", bypass: false, bypass_seq: 0, read_only: false } : undefined }),
  useAgents: () => ({}), useModels: () => ({}), useContextSnapshot: () => ({}),
}));
vi.mock("./useSessionStream", () => ({ useSessionStream: () => ({ view: emptyView() }) }));

const secret = (name: string): SecretView => ({ name, scope: "session", ttl: "1h", created_at: "", use_count: 0, last4: name.slice(-4) });
type Pending = { session: string; resolve: (list: SecretView[]) => void; reject: (e: Error) => void };
let pending: Pending[] = [];
let chat!: Chat;
let seen: { session: string; list: SecretView[] | null | undefined }[] = [];
let root: Root;

function Harness({ session }: { session: string }) {
  chat = useChat("/work", session, () => {});
  seen.push({ session, list: chat.secrets?.list });
  return null;
}
const render = (session: string) => act(() => root.render(<QueryClientProvider client={new QueryClient()}><Harness session={session} /></QueryClientProvider>));
const names = () => chat.secrets?.list?.map((s) => s.name) ?? null;

beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  pending = [];
  seen = [];
  vi.spyOn(api, "secrets").mockImplementation((session) => new Promise((resolve, reject) => pending.push({ session, resolve, reject })));
  root = createRoot(document.createElement("div"));
});
afterEach(() => {
  act(() => root.unmount());
  vi.restoreAllMocks();
});

it("drops a previous session's late answer and never renders its list under the next session", async () => {
  render("A");
  const refreshA = chat.refreshSecrets;
  await act(async () => { void chat.refreshSecrets(); });
  await act(async () => { pending[0]!.resolve([secret("A_KEY")]); });
  expect(names()).toEqual(["A_KEY"]);
  void act(() => { void refreshA(); });
  render("B");
  expect(seen.find((r) => r.session === "B")!.list).toBeNull();
  await act(async () => { void chat.refreshSecrets(); });
  const fromB = pending.find((p) => p.session === "B")!;
  await act(async () => { fromB.resolve([secret("B_KEY")]); });
  expect(names()).toEqual(["B_KEY"]);
  // A refresh started under A, before or after the switch, lands too late.
  for (const p of pending.filter((p) => p.session === "A")) await act(async () => { p.resolve([secret("A_LATE")]); });
  expect(names()).toEqual(["B_KEY"]);
  // A retained A callback called under B cannot take B's ticket.
  await act(async () => { void refreshA(); });
  expect(pending.filter((p) => p.session === "A")).toHaveLength(2);
  expect(seen.every((r) => r.session !== "B" || !r.list?.some((s) => s.name.startsWith("A_")))).toBe(true);
});

it("drops a late error from a previous session", async () => {
  render("A");
  await act(async () => { void chat.refreshSecrets(); });
  render("B");
  await act(async () => { pending[0]!.reject(new Error("A failed")); });
  expect(chat.secretError).toBeNull();
});

it("keeps the newest refresh when an older one answers last, list and error alike", async () => {
  render("A");
  await act(async () => { void chat.refreshSecrets(); void chat.refreshSecrets(); });
  await act(async () => { pending[1]!.resolve([secret("NEW")]); });
  await act(async () => { pending[0]!.resolve([secret("OLD")]); });
  expect(names()).toEqual(["NEW"]);
  await act(async () => { void chat.refreshSecrets(); void chat.refreshSecrets(); });
  await act(async () => { pending[3]!.resolve([secret("NEWER")]); });
  await act(async () => { pending[2]!.reject(new Error("stale failure")); });
  expect(names()).toEqual(["NEWER"]);
  expect(chat.secretError).toBeNull();
});

it("a refresh that left before a save cannot hide the saved secret", async () => {
  vi.spyOn(api, "submitSecret").mockResolvedValue({ accepted: true, secret: secret("API_KEY") });
  render("A");
  await act(async () => { void chat.refreshSecrets(); });
  await act(async () => { pending[0]!.resolve([secret("OTHER")]); });
  await act(async () => { void chat.refreshSecrets(); });
  await act(async () => { await chat.secrets!.submit("sec_1", { value: "v", ttl: "1h", scope: "session" }); });
  expect(names()).toEqual(["OTHER", "API_KEY"]);
  await act(async () => { pending[1]!.resolve([secret("OTHER")]); });
  expect(names()).toEqual(["OTHER", "API_KEY"]);
  // The save asked for the server's list, which is the one kept.
  await act(async () => { pending[2]!.resolve([secret("OTHER"), secret("API_KEY")]); });
  expect(names()).toEqual(["OTHER", "API_KEY"]);
});

it("a save before the first list arrives does not pass for the whole list", async () => {
  vi.spyOn(api, "submitSecret").mockResolvedValue({ accepted: true, secret: secret("API_KEY") });
  render("A");
  await act(async () => { void chat.refreshSecrets(); });
  await act(async () => { await chat.secrets!.submit("sec_1", { value: "v", ttl: "1h", scope: "session" }); });
  expect(names()).toBeNull();
  await act(async () => { pending[0]!.resolve([secret("OTHER")]); });
  expect(names()).toBeNull();
  await act(async () => { pending[1]!.resolve([secret("OTHER"), secret("API_KEY")]); });
  expect(names()).toEqual(["OTHER", "API_KEY"]);
});

it("a revoke that finishes after a session change leaves the new session alone", async () => {
  let finish!: () => void;
  vi.spyOn(api, "revokeSecret").mockImplementation(() => new Promise<void>((resolve) => { finish = resolve; }));
  render("A");
  await act(async () => { void chat.refreshSecrets(); });
  await act(async () => { pending[0]!.resolve([secret("A_KEY")]); });
  let revoking!: Promise<void>;
  act(() => { revoking = chat.secrets!.revoke("A_KEY"); });
  render("B");
  await act(async () => { void chat.refreshSecrets(); });
  await act(async () => { finish(); await revoking; });
  await act(async () => { pending[1]!.resolve([secret("B_KEY")]); });
  expect(names()).toEqual(["B_KEY"]);
  expect(pending).toHaveLength(2);
});
