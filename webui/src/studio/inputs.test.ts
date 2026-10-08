import { describe, expect, it } from "vitest";
import { defaultDecisionInputs, hasEmptyDecisionState, insertInputAlias } from "./inputs";
import { CATALOGUE } from "./mock/fixtures";
import type { NoulNode, TriggerSpec } from "./types";

const trigger = (name: string, fields: string[]): TriggerSpec => ({ name, payload_fields: fields.map((name) => ({ name, required: true, value_type: "string" })), can_block: false, allowed_actions: [], sample_payload: {}, default_fallback: null });

describe("filter aliases", () => {
  it("declares and inserts at the cursor", () => {
    expect(insertInputAlias({}, ' == "test"', "event.text", 0)).toEqual({ inputs: { text: "event.text" }, expression: 'text == "test"', cursor: 4 });
  });
  it("reuses a declared alias without removing it and replaces a selection", () => {
    expect(insertInputAlias({ message: "event.text" }, "foo != 1", "event.text", 0, 3)).toEqual({ inputs: { message: "event.text" }, expression: "message != 1", cursor: 7 });
  });
  it("avoids alias collisions", () => {
    expect(insertInputAlias({ text: "constant.text" }, "", "event.text", 0).inputs).toEqual({ text: "constant.text", text_2: "event.text" });
  });
});

describe("decision defaults", () => {
  it("warns only when a decision has neither inputs nor a template", () => {
    const node: NoulNode = { id: "judge", label: null, type: "noul", instructions: "Judge", threshold: 0.5, state_template: null, inputs: {}, next: {} };
    expect(hasEmptyDecisionState(node)).toBe(true);
    expect(hasEmptyDecisionState({ ...node, inputs: { text: "event.text" } })).toBe(false);
    expect(hasEmptyDecisionState({ ...node, state_template: "A fixed state" })).toBe(false);
  });
  it.each([
    ["message.received", { text: "event.text" }],
    ["save", { diff: "event.diff" }],
    ["idle", { diff: "event.diff" }],
    ["tool.pre_call", { tool: "event.tool", args: "event.args" }],
    ["turn.end", { text: "event.text" }],
    ["conversation", { text: "event.text" }],
    ["diagnostics", { diagnostics: "event.diagnostics", diff: "event.diff" }],
  ])("seeds content from the %s catalogue payload", (name, expected) => {
    expect(defaultDecisionInputs(CATALOGUE.triggers.find((t) => t.name === name))).toEqual(expected);
  });
  it("prefers text, then diff, then tool args, then diagnostics by field name", () => {
    const fields = ["diagnostics", "args", "tool", "diff", "text"];
    expect(defaultDecisionInputs(trigger("custom", fields))).toEqual({ text: "event.text" });
    expect(defaultDecisionInputs(trigger("custom", fields.slice(0, 4)))).toEqual({ diagnostics: "event.diagnostics", diff: "event.diff" });
    expect(defaultDecisionInputs(trigger("custom", fields.slice(0, 3)))).toEqual({ args: "event.args", tool: "event.tool" });
    expect(defaultDecisionInputs(trigger("custom", fields.slice(0, 1)))).toEqual({ diagnostics: "event.diagnostics" });
  });
  it("falls back to all non-identifier string and object fields", () => {
    const fallback = trigger("custom", ["uri", "command_id", "language_id", "summary"]);
    fallback.payload_fields.push({ name: "details", required: false, value_type: "object" }, { name: "count", required: true, value_type: "integer" });
    expect(defaultDecisionInputs(fallback)).toEqual({ summary: "event.summary", details: "event.details" });
    expect(defaultDecisionInputs(trigger("custom", ["uri", "turn_id"]))).toEqual({});
  });
  it("handles missing or empty catalogues", () => {
    expect(defaultDecisionInputs(undefined)).toEqual({});
    expect(defaultDecisionInputs(trigger("empty", []))).toEqual({});
  });
});
