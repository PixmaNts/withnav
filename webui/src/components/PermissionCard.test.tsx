// @vitest-environment jsdom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";
import { PermissionCard, PermissionHoldContext } from "./PermissionCard";
import type { Permission } from "../session/reducer";
import { api, permissionAnswerError } from "../api/client";

const perm: Permission = { requestId: "r", toolCallId: "c", tool: "bash", agent: "build", reason: "review", scope: { kind: "folder", value: "/workspace" }, arguments: "{}" };

for (const [label, decision] of [["Once", "allow_once"], ["Always (project): bash git push *", "allow_always"], ["Reject", "reject"]]) {
  it(`sends the ${label} answer`, async () => {
    const host = document.createElement("div");
    const root = createRoot(host);
    const answer = vi.fn(async () => {});
    await act(async () => root.render(<PermissionCard perm={{ ...perm, alwaysLabel: "Always (project): bash git push *" }} onAnswer={answer} />));
    expect([...host.querySelectorAll("button")].map(button => button.textContent)).toEqual(["Once", "Always (project): bash git push *", "Reject"]);
    await act(async () => [...host.querySelectorAll("button")].find(button => button.textContent === label)!.click());
    expect(answer).toHaveBeenCalledWith(decision, undefined);
    await act(async () => root.unmount());
  });
}

it("hides Always without a label, even for legacy offers", async () => {
  const host = document.createElement("div");
  const root = createRoot(host);
  await act(async () => root.render(<PermissionCard perm={{ ...perm, tool: "fetch", skillName: "sync" }} onAnswer={async () => {}} />));
  expect([...host.querySelectorAll("button")].map(button => button.textContent)).toEqual(["Once", "Reject"]);
  await act(async () => root.unmount());
});

it("sends an optional reject message", async () => {
  const host = document.createElement("div");
  const root = createRoot(host);
  const answer = vi.fn(async () => {});
  await act(async () => root.render(<PermissionCard perm={perm} onAnswer={answer} />));
  const input = host.querySelector("input")!;
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, " Use cargo clean ");
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await act(async () => [...host.querySelectorAll("button")].find(button => button.textContent === "Reject")!.click());
  expect(answer).toHaveBeenCalledWith("reject", "Use cargo clean");
  await act(async () => root.unmount());
});

it("caps reject input by UTF-8 bytes and reports overflow", async () => {
  const host = document.createElement("div");
  const root = createRoot(host);
  await act(async () => root.render(<PermissionCard perm={perm} onAnswer={async () => {}} />));
  const input = host.querySelector("input")!;
  const type = async (value: string) => act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await type("é".repeat(1024));
  expect(input.value).toBe("é".repeat(1024));
  expect(host.textContent).toContain("2048 / 2048 UTF-8 bytes");
  for (const value of ["a".repeat(2049), "a".repeat(2047) + "é"]) {
    await type(value);
    expect(input.value).toBe("é".repeat(1024));
    expect(host.textContent).toContain("Reject message must be at most 2048 UTF-8 bytes");
  }
  await act(async () => root.unmount());
});

it("says when an Always answer could not save its rules", async () => {
  const host = document.createElement("div");
  const root = createRoot(host);
  await act(async () => root.render(<PermissionCard perm={{ ...perm, resolution: "allow_always", alwaysSaved: false, message: "database is locked" }} onAnswer={async () => {}} />));
  expect(host.querySelector(".outcome")!.textContent).toBe("✕ Always not saved: database is locked; ran once");
  await act(async () => root.render(<PermissionCard perm={{ ...perm, resolution: "allow_always" }} onAnswer={async () => {}} />));
  expect(host.querySelector(".outcome")!.textContent).toBe("✓ Allowed always.");
  await act(async () => root.unmount());
});

describe("parallel review", () => {
  async function show(over: Partial<Permission>, extra: { hold?: (requestId: string) => Promise<void>; onAnswer?: (d: string, m?: string) => Promise<void> } = {}) {
    const host = document.createElement("div");
    const root = createRoot(host);
    const card = <PermissionCard perm={{ ...perm, ...over }} onAnswer={extra.onAnswer ?? (async () => {})} />;
    await act(async () => root.render(extra.hold ? <PermissionHoldContext.Provider value={{ hold: extra.hold }}>{card}</PermissionHoldContext.Provider> : card));
    const buttons = () => [...host.querySelectorAll("button")].map((button) => button.textContent);
    return { host, buttons, unmount: () => act(async () => root.unmount()) };
  }
  const text = (host: HTMLElement) => host.textContent ?? "";

  it("shows the review running while the Captain's buttons stay active, with Hold for me", async () => {
    const hold = vi.fn(async () => {});
    const { host, buttons, unmount } = await show({ review: { state: "reviewing" } }, { hold });
    expect(text(host)).toContain("Metacognition is reviewing…");
    expect(buttons()).toEqual(["Once", "Reject", "Hold for me"]);
    expect([...host.querySelectorAll("button")].every((button) => !button.disabled)).toBe(true);
    await act(async () => [...host.querySelectorAll("button")].find((button) => button.textContent === "Hold for me")!.click());
    expect(hold).toHaveBeenCalledWith("r");
    await unmount();
  });

  it("offers no Hold when no one can hold, such as a read-only view", async () => {
    const { buttons, unmount } = await show({ review: { state: "reviewing" } });
    expect(buttons()).toEqual(["Once", "Reject"]);
    await unmount();
  });

  it("shows a call allowed by Metacognition with its conclusion, basis and consequence, and never says always", async () => {
    const { host, buttons, unmount } = await show({
      resolution: "allow_once",
      approvedBy: "metacognition",
      review: { state: "allowed", conclusion: "Releasing implies the push", basis: "release 0.8.4", consequence: "Pushes the tag to origin" },
    });
    expect(host.querySelector(".outcome")!.textContent).toContain("✓ Allowed once by Metacognition.");
    expect(text(host)).toContain("Releasing implies the push");
    expect(text(host)).toContain("Relied on: release 0.8.4");
    expect(text(host)).toContain("Consequence: Pushes the tag to origin");
    expect(buttons()).toEqual([]);
    expect(text(host).toLowerCase()).not.toContain("always");
    await unmount();
  });

  it("never reads a Metacognition decision as an always answer, whatever the resolution says", async () => {
    const { host, unmount } = await show({ resolution: "allow_always", approvedBy: "metacognition", review: { state: "allowed", conclusion: "c" } });
    expect(host.querySelector(".outcome")!.textContent).toContain("Allowed once by Metacognition.");
    expect(text(host).toLowerCase()).not.toContain("always");
    await unmount();
  });

  it("shows the conclusion above the buttons when the review says wait", async () => {
    const { host, buttons, unmount } = await show({ review: { state: "wait", conclusion: "Deletes files the request did not mention", basis: "fix the warnings", consequence: "Removes 3 files" } });
    const note = host.querySelector(".review")!;
    expect(note.textContent).toContain("Waiting for you");
    expect(note.textContent).toContain("Deletes files the request did not mention");
    expect(note.textContent).toContain("Relied on: fix the warnings");
    expect(note.textContent).toContain("Consequence: Removes 3 files");
    expect(note.compareDocumentPosition(host.querySelector(".actions")!) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(buttons()).toEqual(["Once", "Reject"]);
    await unmount();
  });

  it("says the review was cancelled when the Captain answered first", async () => {
    const { host, unmount } = await show({ resolution: "allow_once", approvedBy: "captain", reviewCancelled: true, review: { state: "reviewing" } });
    expect(host.querySelector(".outcome")!.textContent).toContain("Allowed once.");
    expect(text(host)).toContain("Review cancelled: you answered first.");
    await unmount();
  });

  it("says the card stays the Captain's after Hold for me, without a second Hold", async () => {
    const { host, buttons, unmount } = await show({ review: { state: "held" } }, { hold: async () => {} });
    expect(text(host)).toContain("Held for you: the review stopped.");
    expect(buttons()).toEqual(["Once", "Reject"]);
    await unmount();
  });

  it("is calm when the review gave no answer", async () => {
    const { host, buttons, unmount } = await show({ review: { state: "unavailable" } });
    expect(text(host)).toContain("No review; your call.");
    expect(host.querySelector(".composer-error, .bad")).toBeNull();
    expect(buttons()).toEqual(["Once", "Reject"]);
    await unmount();
  });

  it("shows no review line at all without a review", async () => {
    const { host, unmount } = await show({});
    expect(host.querySelector(".review")).toBeNull();
    expect(text(host)).not.toContain("Metacognition");
    await unmount();
  });

  it("says a call already started, and never that it was undone", async () => {
    const refused = async () => {
      throw new Error("Already started by Metacognition — it ran once.");
    };
    const { host, unmount } = await show({ review: { state: "reviewing" } }, { onAnswer: refused });
    await act(async () => [...host.querySelectorAll("button")].find((button) => button.textContent === "Reject")!.click());
    expect(host.querySelector(".composer-error")!.textContent).toBe("Already started by Metacognition — it ran once.");
    expect(text(host).toLowerCase()).not.toMatch(/undone|reverted|cancelled/);
    await unmount();
  });

  it("shows the refusal of a Hold that came too late", async () => {
    const { host, unmount } = await show({ review: { state: "reviewing" } }, { hold: async () => { throw new Error("Already started by Metacognition — it ran once."); } });
    await act(async () => [...host.querySelectorAll("button")].find((button) => button.textContent === "Hold for me")!.click());
    expect(host.querySelector(".composer-error")!.textContent).toBe("Already started by Metacognition — it ran once.");
    await unmount();
  });
});

describe("answer route", () => {
  afterEach(() => vi.unstubAllGlobals());
  const reply = (status: number, body: object) => vi.fn(async () => new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } }));
  const sent = (fetcher: ReturnType<typeof reply>) => JSON.parse((fetcher.mock.calls[0] as unknown as [string, RequestInit])[1].body as string);

  it("binds an answer to the call digest and turn of its request", async () => {
    const fetcher = reply(200, { accepted: true });
    vi.stubGlobal("fetch", fetcher);
    await api.answerPermission("S", "R", "allow_once", undefined, { callDigest: "d1", turnId: "T1" });
    expect(sent(fetcher)).toEqual({ decision: "allow_once", call_digest: "d1", turn_id: "T1" });
  });

  it("sends Hold for me as a hold decision, without a message", async () => {
    const fetcher = reply(200, { accepted: true, held: true });
    vi.stubGlobal("fetch", fetcher);
    await api.answerPermission("S", "R", "hold", undefined, { callDigest: "d1", turnId: "T1" });
    expect(sent(fetcher)).toEqual({ decision: "hold", call_digest: "d1", turn_id: "T1" });
  });

  it("keeps the previous body when there is no binding", async () => {
    const fetcher = reply(200, { accepted: true });
    vi.stubGlobal("fetch", fetcher);
    await api.answerPermission("S", "R", "reject", "no");
    expect(sent(fetcher)).toEqual({ decision: "reject", message: "no" });
  });

  it("reads a refused answer: stale, or a call the review already started", async () => {
    vi.stubGlobal("fetch", reply(409, { error: "gone", reason: "permission_stale" }));
    const stale = await api.answerPermission("S", "R", "allow_once").catch(permissionAnswerError);
    expect((stale as Error).message).toBe("This request is no longer waiting: it was answered, cancelled or expired.");
    vi.stubGlobal("fetch", reply(409, { error: "the call already started: the review allowed it once", reason: "permission_already_started", approved_by: "metacognition" }));
    const started = await api.answerPermission("S", "R", "reject").catch(permissionAnswerError);
    expect((started as Error).message).toBe("Already started by Metacognition — it ran once.");
  });
});
