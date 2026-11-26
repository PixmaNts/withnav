// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import type { MemoryGroup, MemoryProject } from "../api/types";
import { byText, click, mount, type, type Mounted } from "../memory/testing";
import { GroupDialog } from "./GroupDialog";

const project: MemoryProject = { id: "p9", name: "newrepo", group_answered: false, groups: [], aliases: ["/home/pixma/dev/newrepo"], sessions: [] };
const groups: MemoryGroup[] = [{ id: "g1", name: "kyber", members: [], items: 2 }, { id: "g2", name: "infra", members: [], items: 0 }];

let m: Mounted | null = null;
afterEach(async () => { await m?.unmount(); m = null; });

async function open(over: Partial<React.ComponentProps<typeof GroupDialog>> = {}) {
  const props = { project, groups, busy: false, error: null, onAnswer: vi.fn(), onCreate: vi.fn(), onLater: vi.fn(), ...over };
  m = await mount(<GroupDialog {...props} />);
  return { ...props, host: m.host };
}

describe("GroupDialog", () => {
  it("asks which group the repository belongs to and says what sharing means", async () => {
    const { host } = await open();
    const dialog = host.querySelector("[role=dialog]")!;
    expect(dialog.textContent).toContain("Which group does newrepo belong to?");
    expect(dialog.textContent).toContain("/home/pixma/dev/newrepo");
    expect(dialog.textContent).toContain("Items scoped to the group are shared with its projects; global items are shared everywhere.");
    expect([...dialog.querySelectorAll("input[type=radio]")].map((r) => r.closest("label")?.textContent?.trim())).toEqual(["kyber", "infra", "No group", "New group"]);
  });

  it("'Ask me later' leaves the question open and answers nothing", async () => {
    const { host, onAnswer, onCreate, onLater } = await open();
    await click(byText(host, "button", "Ask me later"));
    expect(onLater).toHaveBeenCalledTimes(1);
    expect(onAnswer).not.toHaveBeenCalled();
    expect(onCreate).not.toHaveBeenCalled();
  });

  it("Escape also asks later, never answers", async () => {
    const { host, onAnswer, onLater } = await open();
    host.querySelector("[role=dialog]")!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(onLater).toHaveBeenCalledTimes(1);
    expect(onAnswer).not.toHaveBeenCalled();
  });

  it("answers with the chosen group, with no group, or with a new group's name", async () => {
    const a = await open();
    expect(byText(a.host, "button", "Save")!.hasAttribute("disabled")).toBe(true);
    await click(a.host.querySelector("input[type=radio][value='g1']"));
    await click(byText(a.host, "button", "Save"));
    expect(a.onAnswer).toHaveBeenLastCalledWith("g1");
    await click(a.host.querySelector("input[type=radio][value='none']"));
    await click(byText(a.host, "button", "Save"));
    expect(a.onAnswer).toHaveBeenLastCalledWith(null);
    await click(a.host.querySelector("input[type=radio][value='new']"));
    expect(byText(a.host, "button", "Save")!.hasAttribute("disabled")).toBe(true);
    await type(a.host.querySelector<HTMLInputElement>("input[aria-label='New group name']")!, " perso ");
    await click(byText(a.host, "button", "Save"));
    expect(a.onCreate).toHaveBeenCalledWith("perso");
    expect(a.onAnswer).toHaveBeenCalledTimes(2);
  });

  it("shows a failure and stops while busy", async () => {
    const { host } = await open({ error: "The memory register is unavailable.", busy: true });
    expect(host.querySelector("[role=alert]")?.textContent).toContain("unavailable");
    expect(byText(host, "button", "Save")!.hasAttribute("disabled")).toBe(true);
  });
});
