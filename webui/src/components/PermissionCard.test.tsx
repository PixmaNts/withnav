// @vitest-environment jsdom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import { PermissionCard } from "./PermissionCard";
import type { Permission } from "../session/reducer";

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
  await act(async () => root.render(<PermissionCard perm={{ ...perm, tool: "fetch", skillName: "sync", allowConfigSession: true }} onAnswer={async () => {}} />));
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
