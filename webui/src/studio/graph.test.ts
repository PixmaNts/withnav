import { describe, expect, it } from "vitest";
import { canvasEdges, canvasNodes, definitionFromDrag } from "./canvasModel";
import {
  ancestors, changeDecisionType, connect, connectionError, disconnect, mayBeAbsent, removeNode, renameNode,
  renameOption, topoOrder, TRIGGER_ID, wiresOf,
} from "./graph";
import { badUpstreamReads, inputGroups, renderState, toggleInput } from "./inputs";
import { CATALOGUE, GUARD_MUTATIONS_TOML, sampleRunRecord } from "./mock/fixtures";
import { flowToToml, parseFlowToml } from "./mock/toml";
import { validateDefinition } from "./mock/validate";
import type { Definition } from "./types";

function guard(): Definition {
  const { definition, errors } = parseFlowToml(GUARD_MUTATIONS_TOML);
  expect(errors).toEqual([]);
  return definition!;
}

describe("definition → canvas", () => {
  it("derives one card per node plus the trigger, placed by the layout", () => {
    const def = guard();
    const nodes = canvasNodes(def, null);
    expect(nodes.map((n) => n.id)).toEqual([TRIGGER_ID, ...def.node.map((n) => n.id)]);
    expect(nodes.find((n) => n.id === "plan_overreach")!.position).toEqual({ x: 726, y: 120 });
    expect(nodes.find((n) => n.id === TRIGGER_ID)!.type).toBe("trigger");
  });

  it("derives one edge per wire, with the port as source handle", () => {
    const def = guard();
    const edges = canvasEdges(def, null);
    expect(edges).toHaveLength(wiresOf(def).length);
    const start = edges.find((e) => e.source === TRIGGER_ID)!;
    expect(start).toMatchObject({ target: "effect", sourceHandle: "event" });
    const fork = edges.filter((e) => e.source === "effect").map((e) => [e.sourceHandle, e.target]);
    expect(fork).toEqual([
      ["read_only", "continue"],
      ["local_change", "asked_for"],
      ["network", "trusted_host"],
      ["destructive", "blast_radius"],
    ]);
    expect(edges.find((e) => e.source === "deny")).toMatchObject({ sourceHandle: "then", target: "audit" });
  });

  it("marks taken wires and reached cards from a run", () => {
    const def = guard();
    const run = sampleRunRecord("x");
    const edges = canvasEdges(def, run);
    const taken = edges.filter((e) => e.data?.taken).map((e) => e.id);
    expect(taken).toEqual(["trigger:event->effect", "effect:local_change->asked_for", "asked_for:no->plan_overreach", "plan_overreach:true->deny", "deny:then->audit"]);
    const nodes = canvasNodes(def, run);
    expect(nodes.find((n) => n.id === "ask")!.data.reached).toBe(false);
    expect(nodes.find((n) => n.id === "deny")!.data.reached).toBe(true);
  });

  it("writes a drag back into the layout only", () => {
    const def = guard();
    const next = definitionFromDrag(def, [{ type: "position", id: "ask", position: { x: 1000.4, y: 300 }, dragging: true }]);
    expect(next.layout.ask).toEqual({ x: 1000, y: 300 });
    expect(next.node).toBe(def.node);
    expect(definitionFromDrag(def, [{ type: "select", id: "ask", selected: true }])).toBe(def);
  });
});

describe("wires", () => {
  it("connects and disconnects through start and next", () => {
    let def = guard();
    def = connect(def, { source: TRIGGER_ID, port: "event", target: "trusted_host" });
    expect(def.flow.start).toEqual(["effect", "trusted_host"]);
    def = connect(def, { source: "trusted_host", port: "no", target: "audit" });
    expect(def.node.find((n) => n.id === "trusted_host")!.next.no).toEqual(["ask", "audit"]);
    def = disconnect(def, { source: "trusted_host", port: "no", target: "ask" });
    expect(def.node.find((n) => n.id === "trusted_host")!.next.no).toEqual(["audit"]);
  });

  it("refuses wires that would make a cycle", () => {
    const def = guard();
    expect(connectionError(def, { source: "plan_overreach", port: "false", target: "effect" })).toMatch(/cycle/);
    expect(connectionError(def, { source: "asked_for", port: "yes", target: "asked_for" })).toMatch(/itself/);
    expect(connectionError(def, { source: "plan_overreach", port: "false", target: TRIGGER_ID })).toMatch(/trigger/);
    expect(connect(def, { source: "plan_overreach", port: "false", target: "effect" })).toBe(def);
    // Forward wires between unrelated branches are fine.
    expect(connectionError(def, { source: "trusted_host", port: "no", target: "deny" })).toBeNull();
  });

  it("refuses then → non-action, unknown ports and duplicates", () => {
    const def = guard();
    expect(connectionError(def, { source: "deny", port: "then", target: "plan_overreach" })).toMatch(/action/);
    expect(connectionError(def, { source: "asked_for", port: "maybe", target: "ask" })).toMatch(/no port/);
    expect(connectionError(def, { source: "asked_for", port: "no", target: "plan_overreach" })).toMatch(/already/);
  });

  it("orders nodes topologically", () => {
    const order = topoOrder(guard());
    expect(order.indexOf("effect")).toBeLessThan(order.indexOf("asked_for"));
    expect(order.indexOf("asked_for")).toBeLessThan(order.indexOf("plan_overreach"));
    expect(order.indexOf("deny")).toBeLessThan(order.indexOf("audit"));
  });
});

describe("upstream and the inputs picker", () => {
  it("computes strict ancestors", () => {
    const def = guard();
    expect([...ancestors(def, "plan_overreach")].sort()).toEqual(["asked_for", "effect"]);
    expect([...ancestors(def, "ask")].sort()).toEqual(["asked_for", "blast_radius", "effect", "plan_overreach", "trusted_host"]);
    expect(ancestors(def, "effect").size).toBe(0);
  });

  it("knows when an upstream answer may be absent", () => {
    const def = guard();
    // ask is reached through trusted_host or blast_radius without passing asked_for.
    expect(mayBeAbsent(def, "asked_for", "ask")).toBe(true);
    // Every path to plan_overreach passes asked_for.
    expect(mayBeAbsent(def, "asked_for", "plan_overreach")).toBe(false);
  });

  it("disables answers of nodes that are not upstream", () => {
    const def = guard();
    const groups = inputGroups(def, CATALOGUE, "plan_overreach");
    const upstream = groups.find((g) => g.label === "Upstream answers")!.options;
    const opt = (ref: string) => upstream.find((o) => o.reference === ref)!;
    expect(opt("answer.asked_for").disabled).toBeNull();
    expect(opt("answer.effect").disabled).toBeNull();
    expect(opt("answer.trusted_host").disabled).toMatch(/upstream/);
    expect(upstream.some((o) => o.reference.includes("plan_overreach"))).toBe(false);
    // Event fields and host inputs come from the catalogue, filtered by trigger.
    expect(groups[0]!.options.map((o) => o.reference)).toEqual(["event.tool", "event.args", "event.call_id"]);
    expect(groups.some((g) => g.options.some((o) => o.reference.startsWith("editor.")))).toBe(false);
  });

  it("toggles inputs with unique aliases", () => {
    const def = guard();
    const node = def.node.find((n) => n.id === "ask")!;
    const added = toggleInput(node, "event.args");
    expect(added.inputs.args).toBe("event.args");
    expect(toggleInput(added, "event.args").inputs).toEqual(node.inputs);
    const twice = toggleInput(toggleInput(node, "event.tool"), "answer.tool" as never);
    expect(Object.keys(twice.inputs)).toContain("tool_2");
  });

  it("flags reads of nodes that are not upstream", () => {
    const def = guard();
    expect(badUpstreamReads(def, "ask")).toEqual([]);
    const broken = { ...def, node: def.node.map((n) => (n.id === "asked_for" ? { ...n, inputs: { ...n.inputs, x: "answer.blast_radius" } } : n)) };
    expect(badUpstreamReads(broken, "asked_for")).toEqual(["answer.blast_radius"]);
  });

  it("renders the state as the default renderer does", () => {
    const def = guard();
    const node = def.node.find((n) => n.id === "asked_for")!;
    const state = renderState(node, def, { tool: "write", args: { path: "a" } }, null);
    expect(state).toBe('args = {"path":"a"}\nintent = <absent>\nlast_captain = <absent>');
  });
});

describe("node edits", () => {
  it("removes a node with its wires, layout and the inputs reading it", () => {
    const def = removeNode(guard(), "asked_for");
    expect(def.node.find((n) => n.id === "effect")!.next.local_change).toEqual([]);
    expect(def.layout.asked_for).toBeUndefined();
    expect(Object.values(def.node.find((n) => n.id === "ask")!.inputs)).toEqual(["answer.effect"]);
  });

  it("renames a node everywhere", () => {
    const def = renameNode(guard(), "asked_for", "requested")!;
    expect(def.node.find((n) => n.id === "effect")!.next.local_change).toEqual(["requested"]);
    expect(def.node.find((n) => n.id === "plan_overreach")!.inputs.asked_for).toBe("answer.requested");
    expect(def.layout.requested).toEqual({ x: 476, y: 30 });
    expect(renameNode(guard(), "asked_for", "effect")).toBeNull();
    expect(renameNode(guard(), "asked_for", "trigger")).toBeNull();
  });

  it("keeps wires that still fit when a decision changes type", () => {
    const node = guard().node.find((n) => n.id === "asked_for")!;
    const choice = changeDecisionType(node, "choice");
    expect(choice.type).toBe("choice");
    expect(choice.next).toEqual({});
    expect(changeDecisionType(choice, "noul").type).toBe("noul");
  });

  it("moves a port's wires when its option is renamed", () => {
    const node = guard().node.find((n) => n.id === "effect")!;
    const next = renameOption(node, "network", "remote")!;
    expect(next.type === "choice" && Object.keys(next.criteria)).toEqual(["read_only", "local_change", "remote", "destructive"]);
    expect(next.next.remote).toEqual(["trusted_host"]);
    expect(renameOption(node, "network", "read_only")).toBeNull();
  });
});

describe("mock round trip and validation", () => {
  it("parse → to_toml → parse gives the same definition", () => {
    const first = guard();
    const again = parseFlowToml(flowToToml(first));
    expect(again.errors).toEqual([]);
    expect(again.definition).toEqual(first);
  });

  it("accepts guard-mutations", () => {
    expect(validateDefinition(guard(), CATALOGUE)).toEqual([]);
  });

  it("refuses two verdicts reachable in one run", () => {
    const def = connect(guard(), { source: "trusted_host", port: "yes", target: "deny" });
    const errors = validateDefinition(def, CATALOGUE);
    expect(errors.map((e) => e.code)).toContain("conflicting_verdicts");
  });

  it("refuses a non-upstream read and a verdict in an async flow", () => {
    const def = guard();
    const broken: Definition = {
      ...def,
      flow: { ...def.flow, blocking: false, budget_ms: null },
      node: def.node.map((n) => (n.id === "asked_for" ? { ...n, inputs: { ...n.inputs, x: "answer.blast_radius" } } : n)),
    };
    const codes = validateDefinition(broken, CATALOGUE).map((e) => e.code);
    expect(codes).toContain("not_upstream");
    expect(codes).toContain("action_not_allowed");
  });

  it("reports TOML syntax errors with a line", () => {
    const { definition, errors } = parseFlowToml("[flow]\nname = = 1\n");
    expect(definition).toBeNull();
    expect(errors[0]).toMatchObject({ code: "parse", line: 2 });
  });
});
