// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { WorkspaceRow } from "./Sidebar";
import { EXPANDED_WORKSPACES_KEY, readExpandedWorkspaces, useWorkspaceExpansion } from "../lib/workspaceExpansion";

vi.mock("../api/queries", () => ({ useSessions: vi.fn(() => ({ data: undefined })), useWorkspaces: vi.fn() }));
import { useSessions } from "../api/queries";

const ws = { path: "/work/alpha", name: "alpha", launched: true, session_count: 0, updated_at: null, branch: null, dirty: null };
const route = { workspace: ws.path, session: null, subagent: null, page: "chat" as const, section: null };
const navigate = vi.fn();
let container: HTMLDivElement;
let root: Root;

function Harness() {
  const state = useWorkspaceExpansion(ws.path);
  return <WorkspaceRow ws={ws} route={route} navigate={navigate} expanded={state.expanded.has(ws.path)} setOpen={(open) => state.setOpen(ws.path, open)} />;
}

beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  localStorage.clear();
  vi.clearAllMocks();
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});
afterEach(() => { act(() => root.unmount()); container.remove(); });

describe("yolo badge", () => {
  it("shows a YOLO badge on a session, with the bypass badge's markup", () => {
    const row = (id: string, yolo: boolean) => ({ id, title: id, created_at: "2026-01-01T00:00:00Z", updated_at: "2026-01-01T00:00:00Z", message_count: 1, agent: "build", model: "m", live: true, yolo });
    vi.mocked(useSessions).mockReturnValue({ data: [row("on", true), row("off", false)] } as never);
    act(() => root.render(<WorkspaceRow ws={ws} route={route} navigate={navigate} expanded setOpen={vi.fn()} />));
    const badges = [...container.querySelectorAll(".bypass-badge")];
    expect(badges.map((badge) => badge.textContent)).toEqual(["⚠ YOLO"]);
    expect(badges[0]?.closest(".sess")?.textContent).toContain("on");
  });
});

describe("workspace expansion", () => {
  it("expands a non-current workspace without changing the route", () => {
    const setOpen = vi.fn();
    act(() => root.render(<WorkspaceRow ws={ws} route={{ ...route, workspace: "/work/beta" }} navigate={navigate} expanded={false} setOpen={setOpen} />));
    act(() => (container.querySelector(".ws-toggle") as HTMLButtonElement).click());
    expect(setOpen).toHaveBeenCalledWith(true);
    expect(navigate).not.toHaveBeenCalled();
    act(() => (container.querySelector(".ws-select") as HTMLButtonElement).click());
    expect(navigate).toHaveBeenCalledWith({ workspace: ws.path, session: null, subagent: null });
    expect(setOpen).toHaveBeenCalledTimes(1);
  });

  it("toggles the current workspace without navigating; selecting does not reopen it", () => {
    act(() => root.render(<Harness />));
    act(() => (container.querySelector(".ws-toggle") as HTMLButtonElement).click());
    expect(navigate).not.toHaveBeenCalled();
    expect(container.querySelector(".ws-row")?.getAttribute("aria-expanded")).toBe("false");
    expect(useSessions).toHaveBeenLastCalledWith(null);
    act(() => (container.querySelector(".ws-select") as HTMLButtonElement).click());
    expect(navigate).toHaveBeenCalledWith({ workspace: ws.path, session: null, subagent: null });
    expect(container.querySelector(".ws-row")?.getAttribute("aria-expanded")).toBe("false");
    expect(container.querySelector(".ws-select")?.getAttribute("aria-current")).toBe("page");
  });

  it("persists the expanded set including an all-collapsed set across remounts", () => {
    localStorage.setItem(EXPANDED_WORKSPACES_KEY, JSON.stringify(["/work/beta", ws.path]));
    act(() => root.render(<Harness />));
    act(() => (container.querySelector(".ws-toggle") as HTMLButtonElement).click());
    expect(readExpandedWorkspaces(ws.path, localStorage)).toEqual(new Set(["/work/beta"]));
    act(() => root.unmount());
    root = createRoot(container);
    act(() => root.render(<Harness />));
    expect(container.querySelector(".ws-row")?.getAttribute("aria-expanded")).toBe("false");
    localStorage.setItem(EXPANDED_WORKSPACES_KEY, "[]");
    expect(readExpandedWorkspaces(ws.path, localStorage).size).toBe(0);
  });

  it("uses left and right arrows to collapse and expand without navigating", () => {
    act(() => root.render(<Harness />));
    const row = container.querySelector(".ws-row")!;
    act(() => row.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft", bubbles: true })));
    expect(row.getAttribute("aria-expanded")).toBe("false");
    act(() => row.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true })));
    expect(row.getAttribute("aria-expanded")).toBe("true");
    expect(navigate).not.toHaveBeenCalled();
  });

  it("ignores malformed persisted paths", () => {
    localStorage.setItem(EXPANDED_WORKSPACES_KEY, '[42,"relative"]');
    expect(readExpandedWorkspaces(ws.path, localStorage)).toEqual(new Set([ws.path]));
  });
});

it("shows unavailable permission rules with the shared warning badge and error detail", () => {
  const row = { id: "degraded", title: "degraded", created_at: "2026-01-01T00:00:00Z", updated_at: "2026-01-01T00:00:00Z", message_count: 1, agent: "build", model: "m", live: true, permissions_unavailable: "database open error" };
  vi.mocked(useSessions).mockReturnValue({ data: [row] } as never);
  act(() => root.render(<WorkspaceRow ws={ws} route={route} navigate={navigate} expanded setOpen={vi.fn()} />));
  const badge = container.querySelector(".bypass-badge");
  expect(badge?.textContent).toContain("Permission rules unavailable");
  expect(badge?.getAttribute("title")).toBe("database open error");
});
