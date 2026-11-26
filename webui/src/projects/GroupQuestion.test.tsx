// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ProjectsBody } from "../api/types";
import { byText, click, mount, type Mounted } from "../memory/testing";

vi.mock("../api/client", () => ({ api: { projects: vi.fn(), answerGroup: vi.fn(), createGroup: vi.fn() } }));
import { api } from "../api/client";
import { GroupQuestion } from "./GroupQuestion";

const mocked = vi.mocked(api);
const body = (answered: boolean): ProjectsBody => ({
  projects: [
    { id: "p1", name: "WithNav", group_answered: true, groups: [], aliases: [], sessions: [] },
    { id: "p9", name: "newrepo", group_answered: answered, groups: [], aliases: ["/dev/newrepo"], sessions: [] },
  ],
  groups: [{ id: "g1", name: "kyber", members: [], items: 0 }],
  history: [],
});
let m: Mounted | null = null;

beforeEach(() => {
  vi.clearAllMocks();
  mocked.projects.mockResolvedValue(body(false));
  mocked.answerGroup.mockResolvedValue({} as never);
});
afterEach(async () => { await m?.unmount(); m = null; });

describe("GroupQuestion", () => {
  it("asks nothing when every project has an answer", async () => {
    mocked.projects.mockResolvedValue(body(true));
    m = await mount(<GroupQuestion />);
    expect(m.host.querySelector("[role=dialog]")).toBeNull();
  });

  it("asks about the first unanswered project and records the chosen group", async () => {
    m = await mount(<GroupQuestion />);
    expect(m.host.querySelector("[role=dialog]")?.textContent).toContain("newrepo");
    await click(m.host.querySelector("input[type=radio][value='g1']"));
    await click(byText(m.host, "button", "Save"));
    expect(mocked.answerGroup).toHaveBeenCalledWith("p9", { group_id: "g1" });
  });

  it("'Ask me later' tells the register to keep the question open and hides it for now", async () => {
    m = await mount(<GroupQuestion />);
    await click(byText(m.host, "button", "Ask me later"));
    expect(mocked.answerGroup).toHaveBeenCalledWith("p9", { group_id: null, later: true });
    expect(m.host.querySelector("[role=dialog]")).toBeNull();
  });
});
