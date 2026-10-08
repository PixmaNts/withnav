// The flow definition is the single source of truth. The canvas (React Flow nodes and edges) is
// derived from `start`/`next`/`layout`, and every canvas change is translated back into a new
// definition. Pure functions only: everything here is unit-tested.

import type { ActionNode, Catalogue, Definition, FlowNode, NodeType, Position } from "./types";

/** The synthetic trigger card's id (reserved in the definition, contracts §1). */
export const TRIGGER_ID = "trigger";
/** The trigger card's only port: the event wires listed in `flow.start`. */
export const EVENT_PORT = "event";

// Card anatomy (docs/design/INSTINCTFLOW.md): fixed heights so ports line up with their handles.
export const CARD = { header: 30, body: 60, strip: 24, port: 22, foot: 6, width: 220, actionWidth: 190, triggerWidth: 200 } as const;

export function cardHeight(ports: number): number {
  return CARD.header + CARD.body + CARD.strip + CARD.port * ports + CARD.foot;
}

/** Vertical centre of output port `index`, from the top of the card. */
export function portY(index: number): number {
  return CARD.header + CARD.body + CARD.strip + CARD.port * index + CARD.port / 2;
}

export const ID_PATTERN = /^[A-Za-z_][A-Za-z0-9_]{0,63}$/;
export const FLOW_NAME_PATTERN = /^[a-z][a-z0-9-]{0,63}$/;

export function isDecision(node: FlowNode): node is Extract<FlowNode, { type: "noul" | "choice" | "score" }> {
  return node.type === "noul" || node.type === "choice" || node.type === "score";
}

/** The ports of a node, in display order. */
export function portsOf(node: FlowNode): string[] {
  switch (node.type) {
    case "noul":
      return ["yes", "no"];
    case "choice":
      return Object.keys(node.criteria);
    case "score":
      return ["at_least", "below"];
    case "expression":
      return ["true", "false"];
    case "action":
      return ["then"];
  }
}

export function nodeById(def: Definition, id: string): FlowNode | undefined {
  return def.node.find((n) => n.id === id);
}

// ---------- wires ----------

export interface Wire {
  source: string;
  port: string;
  target: string;
}

export function wireId(w: Wire): string {
  return `${w.source}:${w.port}->${w.target}`;
}

/** Every wire of the definition: the trigger's event wires, then each node's ports. */
export function wiresOf(def: Definition): Wire[] {
  const wires: Wire[] = def.flow.start.map((target) => ({ source: TRIGGER_ID, port: EVENT_PORT, target }));
  for (const node of def.node) {
    for (const port of portsOf(node)) {
      for (const target of node.next[port] ?? []) wires.push({ source: node.id, port, target });
    }
  }
  return wires;
}

/** Direct successors of a card (trigger included), whatever the port. */
export function successors(def: Definition, id: string): string[] {
  if (id === TRIGGER_ID) return [...def.flow.start];
  const node = nodeById(def, id);
  if (!node) return [];
  return Object.values(node.next).flat();
}

export function predecessors(def: Definition, id: string): string[] {
  const out = new Set<string>();
  for (const w of wiresOf(def)) if (w.target === id) out.add(w.source);
  return [...out];
}

/** Is `to` reachable from `from` following wires (from itself excluded unless a cycle exists)? */
export function reaches(def: Definition, from: string, to: string, skip?: string): boolean {
  const seen = new Set<string>();
  const stack = successors(def, from);
  while (stack.length) {
    const id = stack.pop()!;
    if (id === skip) continue;
    if (id === to) return true;
    if (seen.has(id)) continue;
    seen.add(id);
    stack.push(...successors(def, id));
  }
  return false;
}

/**
 * Why a new wire `source:port → target` is refused, or null if it may be drawn. Mirrors the React
 * Flow "prevent cycles" example (walk the outgoers of the target looking for the source), plus the
 * rules of the format: nothing wires into the trigger, `then` only leads to actions, no duplicate.
 */
export function connectionError(def: Definition, wire: Wire): string | null {
  const { source, port, target } = wire;
  if (target === TRIGGER_ID) return "Nothing can wire into the trigger.";
  if (source === target) return "A node cannot wire to itself.";
  const to = nodeById(def, target);
  if (!to) return `Unknown node "${target}".`;
  if (source !== TRIGGER_ID) {
    const from = nodeById(def, source);
    if (!from) return `Unknown node "${source}".`;
    if (!portsOf(from).includes(port)) return `"${source}" has no port "${port}".`;
    if (from.type === "action" && to.type !== "action") return "A then port leads only to another action.";
  } else if (port !== EVENT_PORT) return `The trigger has no port "${port}".`;
  const existing = source === TRIGGER_ID ? def.flow.start : (nodeById(def, source)!.next[port] ?? []);
  if (existing.includes(target)) return "This wire already exists.";
  if (reaches(def, target, source)) return "This wire would make a cycle: a flow runs each node at most once.";
  return null;
}

export function connect(def: Definition, wire: Wire): Definition {
  if (connectionError(def, wire)) return def;
  if (wire.source === TRIGGER_ID) return { ...def, flow: { ...def.flow, start: [...def.flow.start, wire.target] } };
  return mapNode(def, wire.source, (n) => ({ ...n, next: { ...n.next, [wire.port]: [...(n.next[wire.port] ?? []), wire.target] } }));
}

export function disconnect(def: Definition, wire: Wire): Definition {
  if (wire.source === TRIGGER_ID) return { ...def, flow: { ...def.flow, start: def.flow.start.filter((t) => t !== wire.target) } };
  return mapNode(def, wire.source, (n) => {
    const left = (n.next[wire.port] ?? []).filter((t) => t !== wire.target);
    return { ...n, next: { ...n.next, [wire.port]: left } };
  });
}

export function parseWireId(id: string): Wire | null {
  const m = /^(.+?):([^:]+)->(.+)$/.exec(id);
  return m ? { source: m[1]!, port: m[2]!, target: m[3]! } : null;
}

// ---------- ancestry (inputs picker) ----------

/** Strict ancestors of a node: every node with a path of wires to it. The trigger is not a node. */
export function ancestors(def: Definition, id: string): Set<string> {
  const out = new Set<string>();
  const stack = predecessors(def, id);
  while (stack.length) {
    const p = stack.pop()!;
    if (p === TRIGGER_ID || out.has(p)) continue;
    out.add(p);
    stack.push(...predecessors(def, p));
  }
  return out;
}

/**
 * Can `upstream`'s output be absent when `id` runs? True when some path from the trigger reaches
 * `id` without passing through `upstream` (a run through that path never visits it).
 */
export function mayBeAbsent(def: Definition, upstream: string, id: string): boolean {
  return reaches(def, TRIGGER_ID, id, upstream);
}

/** Nodes in topological order (Kahn), ties kept in definition order; cyclic leftovers appended. */
export function topoOrder(def: Definition): string[] {
  const ids = def.node.map((n) => n.id);
  const indeg = new Map(ids.map((id) => [id, 0]));
  for (const w of wiresOf(def)) if (w.source !== TRIGGER_ID && indeg.has(w.target)) indeg.set(w.target, (indeg.get(w.target) ?? 0) + 1);
  const ready = ids.filter((id) => indeg.get(id) === 0);
  const out: string[] = [];
  while (ready.length) {
    const id = ready.shift()!;
    out.push(id);
    for (const s of new Set(successors(def, id))) {
      if (!indeg.has(s)) continue;
      const d = indeg.get(s)! - 1;
      indeg.set(s, d);
      if (d === 0) ready.push(s);
    }
  }
  for (const id of ids) if (!out.includes(id)) out.push(id);
  return out;
}

// ---------- node edits ----------

export function mapNode(def: Definition, id: string, f: (n: FlowNode) => FlowNode): Definition {
  return { ...def, node: def.node.map((n) => (n.id === id ? f(n) : n)) };
}

/** References a node's outputs are read by: `answer.<id>…`, `result.<id>`, `variable.<id>.<name>`. */
export function readsOutputOf(reference: string, id: string): boolean {
  return reference === `answer.${id}` || reference.startsWith(`answer.${id}.`) || reference === `result.${id}` || reference.startsWith(`variable.${id}.`);
}

function renameReference(reference: string, from: string, to: string): string {
  if (!readsOutputOf(reference, from)) return reference;
  const [ns, , ...rest] = reference.split(".");
  return [ns, to, ...rest].join(".");
}

function mapRecord<V>(record: Record<string, V>, f: (v: V) => V): Record<string, V> {
  return Object.fromEntries(Object.entries(record).map(([k, v]) => [k, f(v)]));
}

/** Remove a node, every wire to it, its card position and the inputs that read its outputs. */
export function removeNode(def: Definition, id: string): Definition {
  const layout = { ...def.layout };
  delete layout[id];
  return {
    ...def,
    flow: { ...def.flow, start: def.flow.start.filter((t) => t !== id) },
    node: def.node
      .filter((n) => n.id !== id)
      .map((n) => ({
        ...n,
        next: mapRecord(n.next, (ts) => ts.filter((t) => t !== id)),
        inputs: Object.fromEntries(Object.entries(n.inputs).filter(([, ref]) => !readsOutputOf(ref, id))),
      })),
    layout,
  };
}

/** Rename a node id and every reference to it (wires, layout, inputs). Null if the id is taken or invalid. */
export function renameNode(def: Definition, from: string, to: string): Definition | null {
  if (from === to) return def;
  if (!ID_PATTERN.test(to) || to === TRIGGER_ID || nodeById(def, to)) return null;
  const swap = (t: string) => (t === from ? to : t);
  const layout: Record<string, Position> = {};
  for (const [k, v] of Object.entries(def.layout)) layout[swap(k)] = v;
  return {
    ...def,
    flow: { ...def.flow, start: def.flow.start.map(swap) },
    node: def.node.map((n) => ({
      ...n,
      id: swap(n.id),
      next: mapRecord(n.next, (ts) => ts.map(swap)),
      inputs: mapRecord(n.inputs, (ref) => renameReference(ref, from, to)),
    })),
    layout,
  };
}

/** Drop the `next` entries of ports the node no longer has (after a type or option change). */
export function prunePorts(node: FlowNode): FlowNode {
  const ports = new Set(portsOf(node));
  return { ...node, next: Object.fromEntries(Object.entries(node.next).filter(([p]) => ports.has(p))) };
}

export function uniqueId(def: Definition, base: string): string {
  const taken = new Set([TRIGGER_ID, ...def.node.map((n) => n.id)]);
  if (!taken.has(base)) return base;
  for (let i = 2; ; i++) if (!taken.has(`${base}_${i}`)) return `${base}_${i}`;
}

/** A fresh node with the defaults of its type. For actions, `action` names a catalogue action. */
export function newNode(def: Definition, type: NodeType, catalogue: Catalogue | undefined, action = "log"): FlowNode {
  const base = { label: null, inputs: {}, next: {} };
  switch (type) {
    case "noul":
      return { ...base, id: uniqueId(def, "question"), type, instructions: "", threshold: 0.7, state_template: null };
    case "choice":
      return { ...base, id: uniqueId(def, "kind"), type, instructions: "", criteria: { first: "", second: "" }, state_template: null };
    case "score":
      return { ...base, id: uniqueId(def, "level"), type, instructions: "", levels: ["low", "medium", "high"], at_least: "medium", state_template: null };
    case "expression":
      return { ...base, id: uniqueId(def, "check"), type, condition: "", assign: {} };
    case "action": {
      const spec = catalogue?.actions.find((a) => a.name === action);
      const params = Object.fromEntries((spec?.params ?? []).map((p) => [p.name, p.value_type === "boolean" ? false : p.value_type === "number" || p.value_type === "integer" ? 0 : ""]));
      return { ...base, id: uniqueId(def, action), type, action, params } satisfies ActionNode;
    }
  }
}

export function addNode(def: Definition, node: FlowNode, at: Position): Definition {
  return { ...def, node: [...def.node, node], layout: { ...def.layout, [node.id]: at } };
}

/** Switch a decision between noul, choice and score, keeping instructions, inputs and wires that still fit. */
export function changeDecisionType(node: FlowNode, type: "noul" | "choice" | "score"): FlowNode {
  if (node.type === type || !isDecision(node)) return node;
  const common = { id: node.id, label: node.label, inputs: node.inputs, next: node.next, instructions: node.instructions, state_template: node.state_template };
  const next: FlowNode =
    type === "noul"
      ? { ...common, type, threshold: 0.7 }
      : type === "choice"
        ? { ...common, type, criteria: { first: "", second: "" } }
        : { ...common, type, levels: ["low", "medium", "high"], at_least: "medium" };
  return prunePorts(next);
}

/** Rename a choice option: its criteria key and its port's wires move together. */
export function renameOption(node: FlowNode, from: string, to: string): FlowNode | null {
  if (node.type !== "choice" || from === to) return node.type === "choice" ? node : null;
  if (!ID_PATTERN.test(to) || to in node.criteria) return null;
  const criteria = Object.fromEntries(Object.entries(node.criteria).map(([k, v]) => [k === from ? to : k, v]));
  const next = Object.fromEntries(Object.entries(node.next).map(([k, v]) => [k === from ? to : k, v]));
  return { ...node, criteria, next };
}

// ---------- layout ----------

/** Positions for cards the layout does not place yet: one column per depth from the trigger. */
export function completeLayout(def: Definition): Record<string, Position> {
  const layout = { ...def.layout };
  const depth = new Map<string, number>([[TRIGGER_ID, 0]]);
  for (const id of [TRIGGER_ID, ...topoOrder(def)]) {
    const d = depth.get(id) ?? 1;
    for (const s of successors(def, id)) depth.set(s, Math.max(depth.get(s) ?? 0, d + 1));
  }
  const perColumn = new Map<number, number>();
  for (const id of [TRIGGER_ID, ...def.node.map((n) => n.id)]) {
    if (layout[id]) continue;
    const d = depth.get(id) ?? 1;
    const row = perColumn.get(d) ?? 0;
    perColumn.set(d, row + 1);
    layout[id] = { x: 16 + d * 250, y: 30 + row * 200 };
  }
  return layout;
}
