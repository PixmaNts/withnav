// @vitest-environment jsdom
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { HttpError } from "../api/http";
import type { SessionListEntry } from "../api/types";
import type { Route } from "../router";
import { deleteQuestion, DeleteSessionDialog, nextSession } from "./SessionActions";
import { WorkspaceRow } from "./Sidebar";

vi.mock("../api/client", async (original) => {
  const real = (await original()) as typeof import("../api/client");
  return { api: { ...real.api, sessions: vi.fn(async () => []), remove: vi.fn(async () => undefined) } };
});
import { api } from "../api/client";

const WS = "/work/alpha";
const row = (id: string, extra: Partial<SessionListEntry> = {}): SessionListEntry => ({
  id,
  title: `Session ${id}`,
  created_at: "2026-10-09T08:00:00Z",
  updated_at: "2026-10-09T08:00:00Z",
  message_count: 4,
  agent: "build",
  model: "m",
  variant: null,
  live: false,
  read_only: false,
  resume_disabled_reason: null,
  ...extra,
});
// Three roots; B has three subagents, one of them still running.
const rows = [
  row("A"),
  row("B"),
  row("B1", { parent_session_id: "B", agent: "explore", live: true, child_status: "running" }),
  row("B2", { parent_session_id: "B", agent: "explore", child_status: "completed" }),
  row("B3", { parent_session_id: "B", agent: "review", child_status: "completed" }),
  row("C"),
];
const at = (session: string | null, subagent: string | null = null): Route => ({ page: "chat", workspace: WS, session, subagent, section: null });

let container: HTMLDivElement;
let root: Root;
let qc: QueryClient;
const render = (node: ReactNode) => act(() => root.render(<QueryClientProvider client={qc}>{node}</QueryClientProvider>));
const $ = <T extends HTMLElement = HTMLElement>(sel: string) => container.querySelector<T>(sel)!;
const flush = () => act(async () => { await new Promise((r) => setTimeout(r, 0)); });
/** Waits until the session list query has answered and the tree re-rendered. */
const loaded = async () => {
  for (let i = 0; i < 50 && !qc.getQueryData(["sessions", WS]); i++) await flush();
  await flush();
};
const confirm = () => [...container.querySelectorAll<HTMLButtonElement>(".confirm-actions .btn")].at(-1)!;

beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  vi.clearAllMocks();
  vi.mocked(api.sessions).mockResolvedValue(rows);
  qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});
afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.unstubAllGlobals();
});

describe("api.remove", () => {
  it("sends DELETE /v1/sessions/{id} with the id encoded", async () => {
    const { api: real } = await vi.importActual<typeof import("../api/client")>("../api/client");
    const fetch = vi.fn(async () => new Response(null, { status: 204 }));
    vi.stubGlobal("fetch", fetch);
    await expect(real.remove("a/b c")).resolves.toBeUndefined();
    expect(fetch).toHaveBeenCalledWith("/v1/sessions/a%2Fb%20c", expect.objectContaining({ method: "DELETE" }));
  });

  it("turns a 404 into an HttpError the dialog can recognise", async () => {
    const { api: real } = await vi.importActual<typeof import("../api/client")>("../api/client");
    vi.stubGlobal("fetch", vi.fn(async () => new Response(JSON.stringify({ error: "session not found" }), { status: 404 })));
    await expect(real.remove("gone")).rejects.toMatchObject({ status: 404 });
  });
});

describe("next session after a deletion", () => {
  it("picks the next root, else the previous one, else none", () => {
    expect(nextSession(rows, "A")).toBe("B");
    expect(nextSession(rows, "B")).toBe("C");
    expect(nextSession(rows, "C")).toBe("B");
    expect(nextSession([row("A")], "A")).toBeNull();
  });
});

describe("delete confirmation", () => {
  const dialog = async (id: string, route: Route) => {
    const navigate = vi.fn();
    const onClose = vi.fn();
    render(<DeleteSessionDialog request={{ workspace: WS, id, title: null }} route={route} navigate={navigate} onClose={onClose} />);
    await loaded();
    return { navigate, onClose };
  };

  it("counts the subagent sessions, says it cannot be undone, and deletes only once confirmed", async () => {
    const spy = vi.spyOn(qc, "invalidateQueries");
    const { navigate, onClose } = await dialog("B", at("A"));
    const text = $('[role="alertdialog"]').textContent;
    expect(text).toContain("Delete this session and its 3 subagent sessions? This cannot be undone.");
    expect(text).toContain("running · will be stopped");
    expect(document.activeElement?.textContent).toBe("Cancel");
    expect(api.remove).not.toHaveBeenCalled();
    await act(async () => confirm().click());
    await flush();
    expect(api.remove).toHaveBeenCalledWith("B");
    expect(onClose).toHaveBeenCalled();
    // Another session is open: the route stays.
    expect(navigate).not.toHaveBeenCalled();
    const invalidated = spy.mock.calls.map((c) => JSON.stringify(c[0]?.queryKey));
    expect(invalidated).toEqual(expect.arrayContaining(['["workspaces"]', '["sessions"]']));
  });

  it("cancel deletes nothing", async () => {
    const { onClose } = await dialog("A", at("A"));
    expect($('[role="alertdialog"]').textContent).toContain(deleteQuestion(0));
    act(() => (document.activeElement as HTMLButtonElement).click());
    expect(onClose).toHaveBeenCalled();
    expect(api.remove).not.toHaveBeenCalled();
  });

  it("deleting the open session shows the next one", async () => {
    const { navigate } = await dialog("A", at("A"));
    await act(async () => confirm().click());
    await flush();
    expect(navigate).toHaveBeenCalledWith({ workspace: WS, session: "B", subagent: null }, true);
  });

  it("deleting the parent of the open subagent view leaves the view", async () => {
    const { navigate } = await dialog("B", at("B", "B2"));
    await act(async () => confirm().click());
    await flush();
    expect(navigate).toHaveBeenCalledWith({ workspace: WS, session: "C", subagent: null }, true);
  });

  it("deleting the last session goes to New session", async () => {
    vi.mocked(api.sessions).mockResolvedValue([row("A")]);
    const { navigate } = await dialog("A", at("A"));
    await act(async () => confirm().click());
    await flush();
    expect(navigate).toHaveBeenCalledWith({ workspace: WS, session: null, subagent: null }, true);
  });

  it("a session already gone (404) refreshes silently", async () => {
    vi.mocked(api.remove).mockRejectedValueOnce(new HttpError(404, "session not found"));
    const spy = vi.spyOn(qc, "invalidateQueries");
    const { navigate, onClose } = await dialog("A", at("A"));
    await act(async () => confirm().click());
    await flush();
    expect(container.querySelector('[role="alert"]')).toBeNull();
    expect(onClose).toHaveBeenCalled();
    expect(navigate).toHaveBeenCalledWith({ workspace: WS, session: "B", subagent: null }, true);
    expect(spy.mock.calls.map((c) => JSON.stringify(c[0]?.queryKey))).toContain('["sessions"]');
  });

  it("any other refusal stays open with the reason", async () => {
    vi.mocked(api.remove).mockRejectedValueOnce(new HttpError(500, "store is locked"));
    const { navigate, onClose } = await dialog("A", at("A"));
    await act(async () => confirm().click());
    await flush();
    expect($('[role="alert"]').textContent).toContain("store is locked");
    expect(onClose).not.toHaveBeenCalled();
    expect(navigate).not.toHaveBeenCalled();
    expect(confirm().disabled).toBe(false);
  });
});

describe("sidebar delete action", () => {
  const ws = { path: WS, name: "alpha", launched: false, session_count: 3, updated_at: null, branch: null, dirty: null };

  it("offers a labelled delete button on each root session only, and the Delete key on the row", async () => {
    const onDelete = vi.fn();
    render(<WorkspaceRow ws={ws} route={at("A")} navigate={vi.fn()} expanded setOpen={vi.fn()} onDelete={onDelete} />);
    await loaded();
    const buttons = [...container.querySelectorAll<HTMLButtonElement>(".sess-del")];
    expect(buttons.map((b) => b.getAttribute("aria-label"))).toEqual(["Delete session Session A", "Delete session Session B", "Delete session Session C"]);
    act(() => buttons[1]!.click());
    expect(onDelete).toHaveBeenCalledWith({ workspace: WS, id: "B", title: "Session B" });
    const rowC = [...container.querySelectorAll<HTMLButtonElement>(".sess:not(.child)")].at(-1)!;
    act(() => rowC.dispatchEvent(new KeyboardEvent("keydown", { key: "Delete", bubbles: true })));
    expect(onDelete).toHaveBeenLastCalledWith({ workspace: WS, id: "C", title: "Session C" });
  });

  it("shows no delete action without a handler", async () => {
    render(<WorkspaceRow ws={ws} route={at("A")} navigate={vi.fn()} expanded setOpen={vi.fn()} />);
    await loaded();
    expect(container.querySelector(".sess-del")).toBeNull();
  });
});
