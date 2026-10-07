// @vitest-environment jsdom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { afterEach, expect, it, vi } from "vitest";
import { emptyView, reduce } from "./reducer";
import type { Envelope } from "../api/types";
import { useChat, type Chat } from "./useChat";
import { api } from "../api/client";

const summary = { session_id: "root", actor_state: "idle", workspace: "/work", bypass: false, bypass_seq: 10, read_only: false };
vi.mock("../api/queries", () => ({
  keys: { session: (id: string) => ["session", id], sessions: (ws: string) => ["sessions", ws] },
  useSessionSummary: () => ({ data: summary }), useAgents: () => ({}), useModels: () => ({}), useContextSnapshot: () => ({}),
}));
vi.mock("./useSessionStream", () => ({ useSessionStream: () => ({ view: emptyView() }) }));
const event = (seq: number, enabled: boolean): Envelope => ({ seq, kind: "session.bypass", payload: { enabled, by: "user:http" } } as Envelope);
afterEach(() => { vi.restoreAllMocks(); summary.read_only = false; });

it("renders historical notices without relighting a snapshot; end clears live bypass", () => {
  let view = { ...emptyView(), bypassSeq: 10 };
  view = reduce(view, event(8, true));
  expect(view.bypass).toBe(false);
  expect(view.items[0]).toMatchObject({ kind: "notice", text: expect.stringContaining("Bypass on: ordinary permission prompts are skipped") });
  view = reduce(view, event(11, true));
  expect(view.bypass).toBe(true);
  view = reduce(view, { seq: 12, kind: "session.end" } as Envelope);
  expect(view.bypass).toBe(false);
  expect(reduce(view, event(11, true)).bypass).toBe(false);
});

it("does not enable optimistically and guards repeat submission", async () => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  const qc = new QueryClient();
  qc.setQueryData(["session", "root"], summary);
  let finish!: (value: { enabled: boolean; changed: boolean; bypass_seq: number }) => void;
  const setter = vi.spyOn(api, "setBypass").mockImplementation(() => new Promise(resolve => { finish = resolve; }));
  let chat!: Chat;
  function Harness() { chat = useChat("/work", "root", () => {}); return null; }
  const root = createRoot(document.createElement("div"));
  act(() => root.render(<QueryClientProvider client={qc}><Harness /></QueryClientProvider>));
  let pending!: Promise<void>;
  act(() => { pending = chat.setBypass(true); void chat.setBypass(true); });
  expect(chat.bypass).toBe(false);
  expect(chat.bypassPending).toBe(true);
  expect(setter).toHaveBeenCalledTimes(1);
  await act(async () => { finish({ enabled: true, changed: true, bypass_seq: 11 }); await pending; });
  expect(qc.getQueryData(["session", "root"])).toMatchObject({ bypass: true, bypass_seq: 11 });
  act(() => root.unmount());
});

it("refuses a read-only child setter", async () => {
  summary.read_only = true;
  const setter = vi.spyOn(api, "setBypass");
  let chat!: Chat;
  function Harness() { chat = useChat("/work", "root", () => {}); return null; }
  const root = createRoot(document.createElement("div"));
  act(() => root.render(<QueryClientProvider client={new QueryClient()}><Harness /></QueryClientProvider>));
  await act(async () => { await chat.setBypass(true); });
  expect(setter).not.toHaveBeenCalled();
  expect(chat.bypassReason).toMatch(/read-only/);
  act(() => root.unmount());
});
