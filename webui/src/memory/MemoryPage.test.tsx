// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { HttpError } from "../api/http";
import type { MemoryGroup, MemoryProject, ProjectsBody } from "../api/types";
import { parseRoute } from "../router";
import { byText, choose, click, detail, item, mount, type, version, type Mounted } from "./testing";

vi.mock("./PageShell", () => ({ PageShell: ({ children }: { children: React.ReactNode }) => <div>{children}</div> }));
vi.mock("../api/client", () => ({
  api: {
    memoryItems: vi.fn(),
    memoryItem: vi.fn(),
    reviseMemory: vi.fn(),
    invalidateMemory: vi.fn(),
    restoreMemory: vi.fn(),
    rescopeMemory: vi.fn(),
    needsYou: vi.fn(),
    decideChange: vi.fn(),
    projects: vi.fn(),
    session: vi.fn(),
  },
}));
import { api } from "../api/client";
import { MemoryPage } from "./MemoryPage";

const mocked = vi.mocked(api);
const project = (id: string, name: string, groups: string[] = []): MemoryProject => ({ id, name, group_answered: true, groups, aliases: [], sessions: [] });
const group = (id: string, name: string, members: string[] = []): MemoryGroup => ({ id, name, members, items: 0 });
const registerProjects: ProjectsBody = { projects: [project("p1", "WithNav"), project("p2", "kymux", ["g1"])], groups: [group("g1", "kyber", ["p2"])], history: [] };

const items = [
  item("01AAAAAAAAAAAAAAAAAAAAAAAA", {}, { title: "Inspect configs over Tailscale only", body: "Reach OmniRoute through Tailscale." }),
  item("01BBBBBBBBBBBBBBBBBBBBBBBB", { kind: "pitfall", scope: "project", scope_ref: "p1" }, { title: "cargo test needs the test-support feature", body: "Run cargo test --features test-support.", scope: "project", scope_ref: "p1", quote: "it never compiles without the feature" }),
];

let m: Mounted | null = null;
const navigate = vi.fn();

async function open(hash = "#/memory") {
  m = await mount(<MemoryPage route={parseRoute(hash)} navigate={navigate} />);
  return m;
}

beforeEach(() => {
  vi.clearAllMocks();
  mocked.memoryItems.mockResolvedValue(items);
  mocked.needsYou.mockResolvedValue([]);
  mocked.projects.mockResolvedValue(registerProjects);
});
afterEach(async () => { await m?.unmount(); m = null; });

describe("filters and list", () => {
  it("reads active items first and shows scope, authority and version on each row", async () => {
    const { host } = await open();
    expect(mocked.memoryItems).toHaveBeenLastCalledWith({ status: "active" });
    const rows = host.querySelectorAll(".mm-row");
    expect(rows).toHaveLength(2);
    expect(rows[0]?.textContent).toContain("Inspect configs over Tailscale only");
    expect(rows[0]?.textContent).toContain("global");
    expect(rows[0]?.textContent).toContain("captain said");
    expect(rows[0]?.textContent).toContain("v1");
    expect(rows[1]?.textContent).toContain("project WithNav");
    expect(host.textContent).toContain("2 items");
  });

  it("sends scope, kind and status choices to the register, and a second click clears one", async () => {
    const { host } = await open();
    await click(byText(host, ".mm-chips[aria-label='Scope'] button", "project"));
    expect(mocked.memoryItems).toHaveBeenLastCalledWith({ status: "active", scope: "project" });
    await click(byText(host, ".mm-chips[aria-label='Kind'] button", "pitfall"));
    expect(mocked.memoryItems).toHaveBeenLastCalledWith({ status: "active", scope: "project", kind: "pitfall" });
    await click(byText(host, ".mm-chips[aria-label='Status'] button", "invalidated"));
    expect(mocked.memoryItems).toHaveBeenLastCalledWith({ status: "invalidated", scope: "project", kind: "pitfall" });
    await click(byText(host, ".mm-chips[aria-label='Scope'] button", "project"));
    expect(mocked.memoryItems).toHaveBeenLastCalledWith({ status: "invalidated", kind: "pitfall" });
  });

  it("searches title, wording and quote without asking the server again", async () => {
    const { host } = await open();
    const calls = mocked.memoryItems.mock.calls.length;
    await type(host.querySelector<HTMLInputElement>("input[type=search]")!, "never compiles");
    expect([...host.querySelectorAll(".mm-row .mm-title")].map((t) => t.textContent)).toEqual(["cargo test needs the test-support feature"]);
    await type(host.querySelector<HTMLInputElement>("input[type=search]")!, "zzz");
    expect(host.querySelector(".mm-row")).toBeNull();
    expect(host.textContent).toContain("No item matches these filters");
    expect(mocked.memoryItems.mock.calls.length).toBe(calls);
  });

  it("opens an item by its address", async () => {
    const { host } = await open();
    await click(host.querySelectorAll(".mm-row")[1]);
    expect(navigate.mock.lastCall?.[0]).toEqual({ item: "01BBBBBBBBBBBBBBBBBBBBBBBB" });
  });
});

describe("empty and unreachable", () => {
  it("explains how to start when the register is empty", async () => {
    mocked.memoryItems.mockResolvedValue([]);
    const { host } = await open();
    expect(host.textContent).toContain("Nothing is remembered yet");
    expect(host.textContent).toContain("/remember");
    expect(host.querySelector(".mm-row")).toBeNull();
  });

  it("says the server is unreachable, offers a retry, and recovers", async () => {
    mocked.memoryItems.mockRejectedValue(new HttpError(0, "The WithNav server is unreachable (fetch failed)."));
    const { host } = await open();
    expect(host.querySelector("[role=alert]")?.textContent).toContain("unreachable");
    expect(host.querySelector(".mm-row")).toBeNull();
    mocked.memoryItems.mockResolvedValue(items);
    await click(byText(host, "button", "Try again"));
    expect(host.querySelectorAll(".mm-row")).toHaveLength(2);
    expect(host.querySelector("[role=alert]")).toBeNull();
  });

  it("tells an unavailable register from an unreachable server", async () => {
    mocked.memoryItems.mockRejectedValue(new HttpError(503, "memory_unavailable", "memory_unavailable"));
    const { host } = await open();
    expect(host.querySelector("[role=alert]")?.textContent).toContain("register is unavailable");
  });
});

describe("item detail", () => {
  const id = "01BBBBBBBBBBBBBBBBBBBBBBBB";
  const base = items[1]!;
  const v1 = version(1, { title: base.current.title, body: "Run cargo test.", scope: "project", scope_ref: "p1" });
  const v2 = base.current;
  beforeEach(() => {
    mocked.memoryItem.mockResolvedValue(detail({ ...base, current_version: 2, current: { ...v2, version: 2 } }, [v1, { ...v2, version: 2 }]));
  });

  it("shows the wording, the exact quote, the timeline and the memory-is-not-permission note", async () => {
    const { host } = await open(`#/memory?item=${id}`);
    expect(mocked.memoryItem).toHaveBeenCalledWith(id);
    const pane = host.querySelector(".mm-detail")!;
    expect(pane.textContent).toContain("Run cargo test --features test-support.");
    expect(pane.textContent).toContain("it never compiles without the feature");
    expect(pane.textContent).toContain("never grants permission");
    expect(pane.querySelectorAll(".mm-version")).toHaveLength(2);
  });

  it("keeps the draft and shows the newer version when the item moved on", async () => {
    const { host } = await open(`#/memory?item=${id}`);
    await click(byText(host, ".mm-actions button", "Edit"));
    const text = host.querySelector<HTMLTextAreaElement>("textarea[aria-label='Decision']")!;
    await type(text, "My careful rewrite");
    await type(host.querySelector<HTMLInputElement>("input[aria-label='Reason']")!, "clearer");

    mocked.reviseMemory.mockRejectedValueOnce(new HttpError(409, "version_conflict", "version_conflict", { reason: "version_conflict", current_version: 3 }));
    const newer = version(3, { title: base.current.title, body: "Written from another client", scope: "project", scope_ref: "p1", change: "updated" });
    mocked.memoryItem.mockResolvedValue(detail({ ...base, current_version: 3, current: newer }, [v1, { ...v2, version: 2 }, newer]));
    await click(byText(host, "button", "Save as a new version"));

    expect(mocked.reviseMemory).toHaveBeenCalledWith(id, { expected_version: 2, text: "My careful rewrite", reason: "clearer" });
    const alert = host.querySelector(".mm-conflict")!;
    expect(alert.textContent).toContain("version 3");
    expect(alert.textContent).toContain("Written from another client");
    // The draft is untouched.
    expect(host.querySelector<HTMLTextAreaElement>("textarea[aria-label='Decision']")!.value).toBe("My careful rewrite");
    expect(host.querySelector<HTMLInputElement>("input[aria-label='Reason']")!.value).toBe("clearer");

    // Applying on top writes against the newer version, with the same draft.
    mocked.reviseMemory.mockResolvedValueOnce({ ...base, current_version: 4 });
    await click(byText(host, "button", "Apply my edit on top of version 3"));
    expect(mocked.reviseMemory).toHaveBeenLastCalledWith(id, { expected_version: 3, text: "My careful rewrite", reason: "clearer" });
    expect(host.querySelector(".mm-conflict")).toBeNull();
    expect(host.querySelector("textarea[aria-label='Decision']")).toBeNull();
  });

  it("invalidates with a reason and undoes it by restoring the version", async () => {
    const { host } = await open(`#/memory?item=${id}`);
    await click(byText(host, ".mm-actions button", "Invalidate"));
    await type(host.querySelector<HTMLInputElement>("input[aria-label='Reason']")!, "no longer true");
    mocked.invalidateMemory.mockResolvedValueOnce({ ...base, status: "invalidated", current_version: 3 });
    await click(byText(host, "button", "Invalidate item"));
    expect(mocked.invalidateMemory).toHaveBeenCalledWith(id, { expected_version: 2, reason: "no longer true" });
    const toast = host.querySelector(".mm-toast")!;
    expect(toast.textContent).toContain("Invalidated");
    mocked.restoreMemory.mockResolvedValueOnce({ ...base, current_version: 4 });
    await click(byText(toast, "button", "Undo"));
    expect(mocked.restoreMemory).toHaveBeenCalledWith(id, { expected_version: 3, reason: "Undone from the toast" });
    expect(host.querySelector(".mm-toast")?.textContent).not.toContain("Undo");
  });

  it("asks before widening who reads an item, listing the projects that will", async () => {
    const { host } = await open(`#/memory?item=${id}`);
    await click(byText(host, ".mm-actions button", "Change scope"));
    await click(byText(host, ".mm-chips[aria-label='New scope'] button", "group"));
    await choose(host.querySelector<HTMLSelectElement>("select[aria-label='Group']")!, "g1");
    mocked.rescopeMemory.mockRejectedValueOnce(new HttpError(409, "widening", "widening_requires_confirmation", { reason: "widening_requires_confirmation", affected_projects: [project("p2", "kymux")] }));
    await click(byText(host, "button", "Move item"));
    expect(mocked.rescopeMemory).toHaveBeenLastCalledWith(id, { expected_version: 2, scope: "group", scope_ref: "g1" });
    const dialog = document.querySelector("[role=alertdialog]")!;
    expect(dialog.textContent).toContain("kymux");
    mocked.rescopeMemory.mockResolvedValueOnce({ ...base, scope: "group", scope_ref: "g1", current_version: 3 });
    await click(byText(dialog, "button", "Share"));
    expect(mocked.rescopeMemory).toHaveBeenLastCalledWith(id, { expected_version: 2, scope: "group", scope_ref: "g1", confirm: true });
  });
});

describe("Needs you", () => {
  const change = (id: number, over = {}) => ({ id, relation: "project_group" as const, project_id: "p1", group_id: "g1", change: "add", status: "proposed", actor: "agent", session_id: "01HSESSION000000000000AAAA", at: "2026-11-20T10:00:00Z", decided_at: null, ...over });

  it("lists the waiting changes with what they would do, and accepts or rejects each", async () => {
    mocked.needsYou.mockResolvedValue([change(7), change(8, { relation: "session_project", group_id: undefined, linked_session_id: "01HLINKED00000000000000ZZ", change: "add" })]);
    const { host } = await open("#/memory?view=needs");
    const rows = host.querySelectorAll(".mm-need");
    expect(rows).toHaveLength(2);
    expect(rows[0]?.textContent).toContain("Add WithNav to group kyber");
    expect(rows[1]?.textContent).toContain("Attach session");
    mocked.decideChange.mockResolvedValue(change(7, { status: "applied" }));
    await click(byText(rows[0]!, "button", "Accept"));
    expect(mocked.decideChange).toHaveBeenCalledWith(7, "accept");
    await click(byText(rows[1]!, "button", "Reject"));
    expect(mocked.decideChange).toHaveBeenCalledWith(8, "reject");
  });

  it("has a calm empty state", async () => {
    const { host } = await open("#/memory?view=needs");
    expect(host.querySelector(".mm-need")).toBeNull();
    expect(host.textContent).toContain("Nothing needs you");
  });

  it("says the list is capped when it is full", async () => {
    mocked.needsYou.mockResolvedValue([1, 2, 3, 4, 5].map((n) => change(n)));
    const { host } = await open("#/memory?view=needs");
    expect(host.textContent).toContain("up to 5");
  });

  it("badges the header with the count and opens the view", async () => {
    mocked.needsYou.mockResolvedValue([change(7)]);
    const { host } = await open();
    const button = byText(host, "button", /Needs you/)!;
    expect(button.textContent).toContain("1");
    await click(button);
    expect(navigate.mock.lastCall?.[0]).toEqual({ view: "needs", item: null });
  });
});
