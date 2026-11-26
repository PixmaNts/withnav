import { expect, test } from "vitest";
import { outcomeText } from "./outcome";

test("memory outcomes show scope and format history as lines", () => {
  const item = { id: "abc123", current_version: 1, current: { title: "Checklist" } };
  expect(outcomeText("remember", true, { item, scope_label: "project WithNav" })).toBe('Remembered "Checklist" · project WithNav · abc123 v1');
  expect(outcomeText("forget", true, { item, scope_label: "session" })).toBe('Forgot "Checklist" · session · abc123 v1');
  expect(outcomeText("memory", true, { versions: [{ version: 1, change: "created", title: "Checklist" }, { version: 2, change: "invalidated", title: "Checklist" }] })).toBe("v1 · created · Checklist\nv2 · invalidated · Checklist");
});
