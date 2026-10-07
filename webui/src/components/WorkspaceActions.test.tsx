// @vitest-environment jsdom
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { HttpError } from "../api/http";
import type { WorkspaceEntry } from "../api/types";
import { Sidebar } from "./Sidebar";
import { nextWorkspace, RemoveWorkspaceDialog, WorkspaceMenu, type RemoveRequest } from "./WorkspaceActions";

vi.mock("../api/client", () => ({
  api: {
    workspaces: vi.fn(async () => []),
    sessions: vi.fn(async () => []),
    hideWorkspace: vi.fn(async () => undefined),
    deleteWorkspaceSessions: vi.fn(async () => ({ deleted: 12 })),
    unhideWorkspace: vi.fn(async () => undefined),
  },
}));
import { api } from "../api/client";

const entry = (name: string, extra: Partial<WorkspaceEntry> = {}): WorkspaceEntry => ({ path: `/work/${name}`, name, launched: false, session_count: 12, updated_at: null, branch: null, dirty: null, ...extra });
const alpha = entry("alpha", { launched: true });
const beta = entry("beta");
const gamma = entry("gamma");
const route = (workspace: string | null) => ({ page: "chat" as const, workspace, session: workspace ? "S1" : null, subagent: null, section: null });

let container: HTMLDivElement;
let root: Root;
let qc: QueryClient;
const render = (node: ReactNode) => act(() => root.render(<QueryClientProvider client={qc}>{node}</QueryClientProvider>));
const $ = <T extends Element = HTMLElement>(sel: string) => container.querySelector<T & HTMLElement>(sel)!;
const items = () => [...container.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')];
const key = (el: Element, k: string) => act(() => el.dispatchEvent(new KeyboardEvent("keydown", { key: k, bubbles: true })));
const flush = () => act(async () => { await new Promise((r) => setTimeout(r, 0)); });

beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  vi.clearAllMocks();
  qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});
afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("workspace menu", () => {
  it("opens from the keyboard as a menu and hands the chosen action over", () => {
    const onRemove = vi.fn();
    render(<WorkspaceMenu ws={beta} count={3} onRemove={onRemove} />);
    const trigger = $<HTMLButtonElement>(".ws-more");
    expect(trigger.getAttribute("aria-haspopup")).toBe("menu");
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    act(() => trigger.focus());
    key(trigger, "ArrowDown");
    expect($('[role="menu"]')).toBeTruthy();
    expect(trigger.getAttribute("aria-expanded")).toBe("true");
    expect(document.activeElement).toBe(items()[0]);
    key(items()[0]!, "ArrowDown");
    expect(document.activeElement).toBe(items()[1]);
    key(items()[1]!, "ArrowDown");
    expect(document.activeElement).toBe(items()[0]);
    key(items()[0]!, "Escape");
    expect(container.querySelector('[role="menu"]')).toBeNull();
    expect(document.activeElement).toBe(trigger);

    act(() => trigger.click());
    act(() => items()[1]!.click());
    expect(onRemove).toHaveBeenCalledWith({ ws: beta, kind: "delete", count: 3 });
    expect(container.querySelector('[role="menu"]')).toBeNull();
  });

  it("disables both actions on a folder navai web was started with, and says why", () => {
    const onRemove = vi.fn();
    render(<WorkspaceMenu ws={alpha} count={3} onRemove={onRemove} />);
    act(() => $<HTMLButtonElement>(".ws-more").click());
    for (const item of items()) {
      expect(item.getAttribute("aria-disabled")).toBe("true");
      expect(item.title).toMatch(/navai web was started with this folder/);
      act(() => item.click());
    }
    expect(onRemove).not.toHaveBeenCalled();
    expect($(".ws-menu-note").textContent).toMatch(/stays in the list while the server runs/);
  });

  it("offers no deletion for a folder without sessions", () => {
    render(<WorkspaceMenu ws={entry("empty", { session_count: 0 })} count={0} onRemove={vi.fn()} />);
    act(() => $<HTMLButtonElement>(".ws-more").click());
    expect(items()[0]!.getAttribute("aria-disabled")).toBeNull();
    expect(items()[1]!.getAttribute("aria-disabled")).toBe("true");
  });
});

describe("remove confirmation", () => {
  const dialog = (request: RemoveRequest, current: string | null, list = [alpha, beta, gamma]) => {
    const navigate = vi.fn();
    const onClose = vi.fn();
    render(<RemoveWorkspaceDialog request={request} workspaces={list} route={route(current)} navigate={navigate} onClose={onClose} />);
    return { navigate, onClose, confirm: () => [...container.querySelectorAll<HTMLButtonElement>(".confirm-actions .btn")].at(-1)! };
  };

  it("states the count before deleting, and deletes only once confirmed", async () => {
    const spy = vi.spyOn(qc, "invalidateQueries");
    const { navigate, onClose, confirm } = dialog({ ws: beta, kind: "delete", count: 12 }, "/work/gamma");
    expect($('[role="alertdialog"]').textContent).toContain("Delete 12 sessions and their subagents? This cannot be undone.");
    // The safe answer has the focus.
    expect(document.activeElement?.textContent).toBe("Cancel");
    expect(api.deleteWorkspaceSessions).not.toHaveBeenCalled();
    await act(async () => confirm().click());
    await flush();
    expect(api.deleteWorkspaceSessions).toHaveBeenCalledWith("/work/beta");
    expect(api.hideWorkspace).not.toHaveBeenCalled();
    expect(onClose).toHaveBeenCalled();
    // Not the open workspace: the route stays.
    expect(navigate).not.toHaveBeenCalled();
    const invalidated = spy.mock.calls.map((c) => JSON.stringify(c[0]?.queryKey));
    expect(invalidated).toEqual(expect.arrayContaining(['["workspaces"]', '["sessions"]']));
  });

  it("cancel deletes nothing", () => {
    const { onClose } = dialog({ ws: beta, kind: "delete", count: 12 }, "/work/beta");
    act(() => (document.activeElement as HTMLButtonElement).click());
    expect(onClose).toHaveBeenCalled();
    expect(api.deleteWorkspaceSessions).not.toHaveBeenCalled();
  });

  it("removing the open workspace moves to the next one", async () => {
    const { navigate, confirm } = dialog({ ws: beta, kind: "hide", count: 12 }, "/work/beta");
    expect($('[role="alertdialog"]').textContent).toContain("Its 12 sessions are kept.");
    await act(async () => confirm().click());
    await flush();
    expect(api.hideWorkspace).toHaveBeenCalledWith("/work/beta");
    expect(navigate).toHaveBeenCalledWith({ workspace: "/work/gamma", session: null, subagent: null });
  });

  it("removing the last open workspace falls back to the empty state", async () => {
    const { navigate, confirm } = dialog({ ws: beta, kind: "hide", count: 12 }, "/work/beta", [beta]);
    await act(async () => confirm().click());
    await flush();
    expect(navigate).toHaveBeenCalledWith({ workspace: null, session: null, subagent: null });
  });

  it("shows a refusal inline and stays open", async () => {
    vi.mocked(api.hideWorkspace).mockRejectedValueOnce(new HttpError(409, "cannot be hidden", "launched"));
    const { navigate, onClose, confirm } = dialog({ ws: beta, kind: "hide", count: 12 }, "/work/beta");
    await act(async () => confirm().click());
    await flush();
    expect($('[role="alert"]').textContent).toMatch(/navai web was started with this folder/);
    expect(onClose).not.toHaveBeenCalled();
    expect(navigate).not.toHaveBeenCalled();
    expect(confirm().disabled).toBe(false);
  });

  it("picks the next workspace, else the previous one, else none", () => {
    expect(nextWorkspace([alpha, beta, gamma], "/work/beta")).toBe("/work/gamma");
    expect(nextWorkspace([alpha, beta, gamma], "/work/gamma")).toBe("/work/beta");
    expect(nextWorkspace([beta], "/work/beta")).toBeNull();
  });
});

describe("open a folder", () => {
  it("shows a hidden folder again before opening it", async () => {
    vi.mocked(api.workspaces).mockResolvedValue([alpha]);
    const navigate = vi.fn();
    const expansion = { expanded: new Set<string>(), setOpen: vi.fn(), collapseAll: vi.fn(), expandAll: vi.fn() };
    render(<Sidebar route={route(null)} navigate={navigate} expansion={expansion} onRemove={vi.fn()} />);
    act(() => $<HTMLButtonElement>('[aria-label="Open a folder"]').click());
    const input = $<HTMLInputElement>(".open-folder input");
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
    act(() => {
      setter.call(input, "/work/beta/");
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    await act(async () => $<HTMLButtonElement>('.open-folder button[type="submit"]').click());
    await flush();
    expect(api.unhideWorkspace).toHaveBeenCalledWith("/work/beta");
    expect(navigate).toHaveBeenCalledWith({ workspace: "/work/beta", session: null, subagent: null });
    expect(vi.mocked(api.unhideWorkspace).mock.invocationCallOrder[0]!).toBeLessThan(navigate.mock.invocationCallOrder[0]!);
  });

  it("keeps the form open with the error when the server refuses the path", async () => {
    vi.mocked(api.unhideWorkspace).mockRejectedValueOnce(new HttpError(400, "workspace must be an existing directory"));
    const navigate = vi.fn();
    const expansion = { expanded: new Set<string>(), setOpen: vi.fn(), collapseAll: vi.fn(), expandAll: vi.fn() };
    render(<Sidebar route={route(null)} navigate={navigate} expansion={expansion} />);
    act(() => $<HTMLButtonElement>('[aria-label="Open a folder"]').click());
    const input = $<HTMLInputElement>(".open-folder input");
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
    act(() => {
      setter.call(input, "/nope");
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    await act(async () => $<HTMLButtonElement>('.open-folder button[type="submit"]').click());
    await flush();
    expect($('[role="alert"]').textContent).toContain("workspace must be an existing directory");
    expect(navigate).not.toHaveBeenCalled();
  });
});
