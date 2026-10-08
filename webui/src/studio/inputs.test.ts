import { describe, expect, it } from "vitest";
import { defaultDecisionInputs, hasEmptyDecisionState, insertInputAlias } from "./inputs";
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
  it("uses the first event field", () => expect(defaultDecisionInputs(trigger("message.received", ["text", "role"]))).toEqual({ text: "event.text" }));
  it("includes tool and args for tool calls", () => expect(defaultDecisionInputs(trigger("tool.pre_call", ["tool", "args", "id"]))).toEqual({ tool: "event.tool", args: "event.args" }));
  it("handles missing or empty catalogues", () => {
    expect(defaultDecisionInputs(undefined)).toEqual({});
    expect(defaultDecisionInputs(trigger("empty", []))).toEqual({});
  });
});
