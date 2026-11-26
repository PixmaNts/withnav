// @vitest-environment jsdom
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { api } from "../api/client";
import { GoalNoteBlock, RememberedLine } from "./SessionPanelLines";

vi.mock("../api/client", () => ({ api: { invalidateMemory: vi.fn(), restoreMemory: vi.fn() } }));

const item = { kind: "remembered" as const, key: "n:1", itemId: "abc123", version: 2, title: "Use conventional commits", scope: "project WithNav" };
let container: HTMLDivElement;
let root: Root;
const render = (node: React.ReactNode) => act(() => root.render(<QueryClientProvider client={new QueryClient()}>{node}</QueryClientProvider>));
const button = () => container.querySelector("button") as HTMLButtonElement;

beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  vi.clearAllMocks();
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});
afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("the Remembered line", () => {
  it("names the title and scope, and undo invalidates the item at the version it stored", async () => {
    vi.mocked(api.invalidateMemory).mockResolvedValue({ current_version: 3 } as never);
    render(<RememberedLine item={item} />);
    expect(container.textContent).toBe("Remembered: “Use conventional commits” · project WithNav · undo");
    await act(async () => button().click());
    expect(api.invalidateMemory).toHaveBeenCalledWith("abc123", { expected_version: 2 });
    expect(container.textContent).toBe("Removed from memory: “Use conventional commits” · project WithNav · redo");
    vi.mocked(api.restoreMemory).mockResolvedValue({ current_version: 4 } as never);
    await act(async () => button().click());
    expect(api.restoreMemory).toHaveBeenCalledWith("abc123", { expected_version: 3 });
    expect(container.textContent).toContain("Remembered:");
  });

  it("keeps the line and shows the refusal when the item moved on", async () => {
    vi.mocked(api.invalidateMemory).mockRejectedValue(new Error("the item is at version 3"));
    render(<RememberedLine item={item} />);
    await act(async () => button().click());
    expect(container.textContent).toContain("Remembered:");
    expect(container.querySelector('[role="alert"]')?.textContent).toBe("the item is at version 3");
  });

  it("offers no undo in a read-only conversation", () => {
    render(<RememberedLine item={item} readOnly />);
    expect(container.querySelector("button")).toBeNull();
  });
});

describe("the goal note", () => {
  it("says it is an automatic observation, not a Captain message", () => {
    render(<GoalNoteBlock item={{ kind: "goal-note", key: "n:2", text: "Goal check (revision 3): criterion 2 is contradicted.", revision: 3, at: "2026-11-25T22:17:00.000Z" }} />);
    expect(container.textContent).toContain("Goal note");
    expect(container.textContent).toContain("automatic observation, not a Captain message");
    expect(container.textContent).toContain("Goal check (revision 3): criterion 2 is contradicted.");
  });
});
