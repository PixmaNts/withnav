// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { parseRoute } from "../router";
import { click, mount, type Mounted } from "./testing";

vi.mock("../api/client", () => ({ api: { needsYou: vi.fn() } }));
import { api } from "../api/client";
import { MemoryNav } from "./MemoryNav";

const navigate = vi.fn();
let m: Mounted | null = null;
beforeEach(() => vi.clearAllMocks());
afterEach(async () => { await m?.unmount(); m = null; });

describe("MemoryNav", () => {
  it("badges Memory with the changes waiting for the Captain", async () => {
    vi.mocked(api.needsYou).mockResolvedValue([{ id: 1 }, { id: 2 }] as never);
    m = await mount(<MemoryNav route={parseRoute("#/chat")} navigate={navigate} />);
    expect(m.host.querySelector(".mm-badge")?.textContent).toBe("2");
    expect(m.host.querySelector("[aria-label='2 waiting for you']")).toBeTruthy();
  });

  it("shows no badge when nothing waits, and opens the pages", async () => {
    vi.mocked(api.needsYou).mockResolvedValue([]);
    m = await mount(<MemoryNav route={parseRoute("#/projects")} navigate={navigate} />);
    expect(m.host.querySelector(".mm-badge")).toBeNull();
    expect(m.host.querySelector("[aria-current=page]")?.textContent).toContain("Projects");
    await click(m.host.querySelectorAll("button")[0]);
    expect(navigate).toHaveBeenLastCalledWith({ page: "memory", item: null, view: null });
  });

  it("stays quiet when the register cannot be read", async () => {
    vi.mocked(api.needsYou).mockRejectedValue(new Error("down"));
    m = await mount(<MemoryNav route={parseRoute("#/chat")} navigate={navigate} />);
    expect(m.host.querySelector(".mm-badge")).toBeNull();
  });
});
