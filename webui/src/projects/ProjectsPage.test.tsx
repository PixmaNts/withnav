// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { HttpError } from "../api/http";
import type { MembershipChange, ProjectsBody } from "../api/types";
import { byText, choose, click, mount, type, type Mounted } from "../memory/testing";
import { parseRoute } from "../router";

vi.mock("../memory/PageShell", () => ({ PageShell: ({ children }: { children: React.ReactNode }) => <div>{children}</div> }));
vi.mock("../api/client", () => ({
  api: {
    projects: vi.fn(),
    sessions: vi.fn(),
    addProjectToGroup: vi.fn(),
    removeProjectFromGroup: vi.fn(),
    sessionProject: vi.fn(),
    createGroup: vi.fn(),
    renameGroup: vi.fn(),
  },
}));
import { api } from "../api/client";
import { ProjectsPage } from "./ProjectsPage";

const mocked = vi.mocked(api);
const navigate = vi.fn();
let m: Mounted | null = null;

const change = (id: number, over: Partial<MembershipChange> = {}): MembershipChange => ({
  id, relation: "session_project", project_id: "p1", linked_session_id: "01HSESSIONLINKED0000000001", change: "add", status: "applied", actor: "captain", session_id: null, at: "2026-11-20T10:00:00Z", decided_at: null, ...over,
});

const body = (over: Partial<ProjectsBody> = {}): ProjectsBody => ({
  projects: [
    { id: "p1", name: "WithNav", group_answered: true, groups: [], aliases: ["/home/pixma/dev/Navai"], sessions: [{ session_id: "01HSESSIONLINKED0000000001", project_id: "p1", active: true }] },
    { id: "p2", name: "kymux", group_answered: true, groups: ["g1"], aliases: ["/home/pixma/dev/kymux"], sessions: [] },
    { id: "p3", name: "kysdk", group_answered: true, groups: ["g1"], aliases: [], sessions: [] },
  ],
  groups: [{ id: "g1", name: "kyber", members: ["p2", "p3"], items: 4 }, { id: "g2", name: "infra", members: [], items: 0 }],
  history: [change(1)],
  ...over,
});

const open = async (hash = "#/projects?ws=%2Frepo&item=p1") => (m = await mount(<ProjectsPage route={parseRoute(hash)} navigate={navigate} />));

beforeEach(() => {
  vi.clearAllMocks();
  mocked.projects.mockResolvedValue(body());
  mocked.sessions.mockResolvedValue([
    { id: "01HSESSIONLINKED0000000001", title: "Fix the watcher", created_at: "", updated_at: "", message_count: 3, agent: "build", model: "m", variant: null, live: false, read_only: false, resume_disabled_reason: null },
    { id: "01HSESSIONOTHER00000000002", title: "Write the book page", created_at: "", updated_at: "", message_count: 1, agent: "build", model: "m", variant: null, live: false, read_only: false, resume_disabled_reason: null },
  ]);
});
afterEach(async () => { await m?.unmount(); m = null; });

describe("groups column", () => {
  it("counts the projects of each group and of no group, and narrows the project list", async () => {
    const { host } = await open();
    const groups = [...host.querySelectorAll(".pj-group")].map((g) => g.textContent);
    expect(groups.some((t) => t?.includes("kyber") && t.includes("2") && t.includes("4 items"))).toBe(true);
    expect(groups.some((t) => t?.includes("No group") && t.includes("1"))).toBe(true);
    expect(host.querySelectorAll(".pj-project")).toHaveLength(3);
    await click(byText(host, ".pj-group", /kyber/));
    expect([...host.querySelectorAll(".pj-project .mm-title")].map((t) => t.textContent)).toEqual(["kymux", "kysdk"]);
    await click(byText(host, ".pj-group", /No group/));
    expect([...host.querySelectorAll(".pj-project .mm-title")].map((t) => t.textContent)).toEqual(["WithNav"]);
  });

  it("creates a group by its name and reads the register again", async () => {
    const { host } = await open();
    mocked.createGroup.mockResolvedValue({ id: "g3", name: "perso", members: [], items: 0 });
    await type(host.querySelector<HTMLInputElement>("input[aria-label='New group name']")!, " perso ");
    await click(byText(host, "button", "Create group"));
    expect(mocked.createGroup).toHaveBeenCalledWith("perso");
    expect(mocked.projects.mock.calls.length).toBeGreaterThan(1);
  });

  it("renames the selected group", async () => {
    const { host } = await open();
    await click(byText(host, ".pj-group", /kyber/));
    await click(byText(host, "button", "Rename"));
    mocked.renameGroup.mockResolvedValue({ id: "g1", name: "kyber-labs", members: [], items: 0 });
    const input = host.querySelector<HTMLInputElement>("input[aria-label='Group name']")!;
    expect(input.value).toBe("kyber");
    await type(input, "kyber-labs");
    await click(byText(host, "button", "Save name"));
    expect(mocked.renameGroup).toHaveBeenCalledWith("g1", "kyber-labs");
  });
});

describe("project detail", () => {
  it("shows its paths, groups, linked sessions by title, and what stays in history", async () => {
    const { host } = await open();
    const pane = host.querySelector(".pj-detail")!;
    expect(pane.textContent).toContain("/home/pixma/dev/Navai");
    expect(pane.textContent).toContain("Fix the watcher");
    expect(pane.textContent).toContain("Detaching keeps the link in the history");
  });

  it("adds a project to a group straight away when nobody would newly read anything", async () => {
    const { host } = await open();
    mocked.addProjectToGroup.mockResolvedValue(change(2, { relation: "project_group", group_id: "g2" }));
    await choose(host.querySelector<HTMLSelectElement>("select[aria-label='Add to a group']")!, "g2");
    await click(byText(host, "button", "Add to group"));
    expect(mocked.addProjectToGroup).toHaveBeenCalledTimes(1);
    expect(mocked.addProjectToGroup).toHaveBeenCalledWith("p1", "g2", false);
    expect(document.querySelector("[role=alertdialog]")).toBeNull();
  });

  it("asks before widening, listing the projects that would read the group's items, and confirms once", async () => {
    const { host } = await open();
    mocked.addProjectToGroup.mockRejectedValueOnce(
      new HttpError(409, "widening", "widening_requires_confirmation", {
        reason: "widening_requires_confirmation",
        affected_projects: [{ id: "p1", name: "WithNav", group_answered: true, groups: [], aliases: ["/home/pixma/dev/Navai"], sessions: [] }],
      }),
    );
    await choose(host.querySelector<HTMLSelectElement>("select[aria-label='Add to a group']")!, "g1");
    await click(byText(host, "button", "Add to group"));
    expect(mocked.addProjectToGroup).toHaveBeenLastCalledWith("p1", "g1", false);
    const dialog = document.querySelector("[role=alertdialog]")!;
    expect(dialog.textContent).toContain("WithNav");
    expect(dialog.textContent).toContain("kyber");
    mocked.addProjectToGroup.mockResolvedValue(change(3, { relation: "project_group", group_id: "g1" }));
    await click(byText(dialog, "button", "Share"));
    expect(mocked.addProjectToGroup).toHaveBeenLastCalledWith("p1", "g1", true);
    expect(document.querySelector("[role=alertdialog]")).toBeNull();
  });

  it("writes nothing when the widening is declined", async () => {
    const { host } = await open();
    mocked.addProjectToGroup.mockRejectedValueOnce(new HttpError(409, "widening", "widening_requires_confirmation", { reason: "widening_requires_confirmation", affected_projects: [] }));
    await choose(host.querySelector<HTMLSelectElement>("select[aria-label='Add to a group']")!, "g1");
    await click(byText(host, "button", "Add to group"));
    await click(byText(document.querySelector("[role=alertdialog]")!, "button", "Cancel"));
    expect(mocked.addProjectToGroup).toHaveBeenCalledTimes(1);
    expect(document.querySelector("[role=alertdialog]")).toBeNull();
  });

  it("removes a project from a group", async () => {
    const { host } = await open("#/projects?item=p2");
    mocked.removeProjectFromGroup.mockResolvedValue(change(4, { relation: "project_group", group_id: "g1", change: "remove" }));
    await click(host.querySelector("button[aria-label='Remove kymux from kyber']"));
    expect(mocked.removeProjectFromGroup).toHaveBeenCalledWith("p2", "g1");
  });

  it("detaches a session and keeps it in the history, shown as detached", async () => {
    const { host } = await open();
    mocked.sessionProject.mockResolvedValue(change(5, { change: "remove" }));
    // After the detach the register reports the link as inactive and the change in the history.
    mocked.projects.mockResolvedValue(
      body({
        projects: body().projects.map((p) => (p.id === "p1" ? { ...p, sessions: [{ session_id: "01HSESSIONLINKED0000000001", project_id: "p1", active: false }] } : p)),
        history: [change(5, { change: "remove" }), change(1)],
      }),
    );
    await click(byText(host, "button", "Detach"));
    expect(mocked.sessionProject).toHaveBeenCalledWith("01HSESSIONLINKED0000000001", null);
    const pane = host.querySelector(".pj-detail")!;
    expect(pane.querySelector(".pj-session[data-active]")).toBeNull();
    expect(pane.querySelector(".pj-session[data-detached]")?.textContent).toContain("Fix the watcher");
    expect(byText(pane, "button", "Detach")).toBeUndefined();
    const history = [...pane.querySelectorAll(".pj-history li")].map((li) => li.textContent);
    expect(history.some((t) => t?.includes("Detached") && t.includes("Fix the watcher"))).toBe(true);
    expect(history.some((t) => t?.includes("Attached"))).toBe(true);
  });

  it("attaches a session of the open workspace, and offers only those not linked yet", async () => {
    const { host } = await open();
    const select = host.querySelector<HTMLSelectElement>("select[aria-label='Attach a session']")!;
    expect([...select.options].map((o) => o.textContent)).toEqual(["Choose a session…", "Write the book page"]);
    mocked.sessionProject.mockResolvedValue(change(6, { linked_session_id: "01HSESSIONOTHER00000000002" }));
    await choose(select, "01HSESSIONOTHER00000000002");
    await click(byText(host, "button", "Attach"));
    expect(mocked.sessionProject).toHaveBeenCalledWith("01HSESSIONOTHER00000000002", "p1");
  });
});

describe("states", () => {
  it("says the server is unreachable", async () => {
    mocked.projects.mockRejectedValue(new HttpError(0, "The WithNav server is unreachable (x)."));
    const { host } = await open();
    expect(host.querySelector("[role=alert]")?.textContent).toContain("unreachable");
  });

  it("explains an empty register", async () => {
    mocked.projects.mockResolvedValue({ projects: [], groups: [], history: [] });
    const { host } = await open("#/projects");
    expect(host.textContent).toContain("No project yet");
  });
});
