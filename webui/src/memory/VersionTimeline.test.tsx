// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { VersionTimeline } from "./VersionTimeline";
import { diffText } from "./model";
import { click, mount, version, type Mounted } from "./testing";

let m: Mounted | null = null;
afterEach(async () => { await m?.unmount(); m = null; });

const versions = [
  version(1, { title: "Judge permissions", body: "Escalate when the score is under 0.8.", why: "Fixed threshold" }),
  version(2, { body: "Escalate when the consequence is not covered by the conversation.", why: "Judged against the conversation", reason: "Threshold was arbitrary", scope: "project", scope_ref: "p1", drafted_by: "agent", source_actor: "captain" }),
  version(3, { change: "invalidated", body: "Escalate when the consequence is not covered by the conversation.", reason: "Moved to a flow" }),
];

describe("diffText", () => {
  it("separates what was removed from what was added, word by word", () => {
    const parts = diffText("keep the old rule", "keep the new rule");
    expect(parts.filter((p) => p.kind === "del").map((p) => p.text)).toEqual(["old"]);
    expect(parts.filter((p) => p.kind === "add").map((p) => p.text)).toEqual(["new"]);
    expect(parts.filter((p) => p.kind === "same").map((p) => p.text).join("")).toBe("keep the  rule");
  });

  it("is all same for equal texts and handles an empty side", () => {
    expect(diffText("a b", "a b")).toEqual([{ kind: "same", text: "a b" }]);
    expect(diffText("", "a b")).toEqual([{ kind: "add", text: "a b" }]);
    expect(diffText("a b", "")).toEqual([{ kind: "del", text: "a b" }]);
  });
});

describe("VersionTimeline", () => {
  it("lists every version newest first with its change, author, reason and session", async () => {
    m = await mount(<VersionTimeline versions={versions} currentVersion={3} groups={[]} projects={[]} />);
    const rows = [...m.host.querySelectorAll(".mm-version")];
    expect(rows.map((r) => r.querySelector(".mm-vnum")?.textContent)).toEqual(["v3", "v2", "v1"]);
    expect(rows[0]?.textContent).toContain("invalidated");
    expect(rows[0]?.textContent).toContain("Moved to a flow");
    expect(rows[1]?.textContent).toContain("proposed by Nav, accepted by the Captain");
    expect(rows[2]?.textContent).toContain("the Captain");
    expect(rows[2]?.textContent).toContain("AAAA");
  });

  it("shows no diff until two versions are picked, then the changes between them, older to newer", async () => {
    m = await mount(<VersionTimeline versions={versions} currentVersion={3} groups={[]} projects={[]} />);
    expect(m.host.querySelector(".mm-diff")).toBeNull();
    await click(m.host.querySelector('input[aria-label="Compare version 2"]'));
    expect(m.host.querySelector(".mm-diff")).toBeNull();
    await click(m.host.querySelector('input[aria-label="Compare version 1"]'));
    const diff = m.host.querySelector(".mm-diff")!;
    expect(diff.textContent).toContain("v1 → v2");
    expect([...diff.querySelectorAll("del")].map((d) => d.textContent).join(" ")).toContain("score");
    expect([...diff.querySelectorAll("ins")].map((d) => d.textContent).join(" ")).toContain("conversation");
    // The reason of the later version and its scope change are part of the comparison.
    expect(diff.textContent).toContain("global → project");
  });

  it("replaces the oldest pick when a third is chosen and clears on unpick", async () => {
    m = await mount(<VersionTimeline versions={versions} currentVersion={3} groups={[]} projects={[]} />);
    for (const n of [1, 2, 3]) await click(m.host.querySelector(`input[aria-label="Compare version ${n}"]`));
    expect(m.host.querySelector(".mm-diff")?.textContent).toContain("v2 → v3");
    await click(m.host.querySelector('input[aria-label="Compare version 3"]'));
    expect(m.host.querySelector(".mm-diff")).toBeNull();
  });

  it("offers to use an older wording again", async () => {
    const use = vi.fn();
    m = await mount(<VersionTimeline versions={versions} currentVersion={3} groups={[]} projects={[]} onUse={use} />);
    const buttons = [...m.host.querySelectorAll<HTMLButtonElement>("button")].filter((b) => /Use this wording/.test(b.textContent ?? ""));
    expect(buttons).toHaveLength(2);
    await click(buttons[1]);
    expect(use).toHaveBeenCalledWith(1);
  });
});
