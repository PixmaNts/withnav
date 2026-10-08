// What a node may read (contracts §1, "Reference namespace"), grouped for the inputs picker, and the
// state a decision sends, rendered the way the engine's default renderer does.

import { ancestors, isDecision, mayBeAbsent, nodeById } from "./graph";
import type { Catalogue, Definition, FlowNode, RunRecord } from "./types";

export interface InputOption {
  reference: string;
  /** Short type note shown at the right of the row. */
  note: string;
  /** Why it cannot be picked (not upstream of the node), or null. */
  disabled: string | null;
  /** An upstream output the run may not have passed through: it can be absent. */
  mayBeAbsent: boolean;
}

export interface InputGroup {
  label: string;
  options: InputOption[];
}

const SOURCE_LABEL: Record<string, string> = {
  session: "Session",
  conversation: "Conversation with the cognition",
  intent: "Intent",
  workspace: "Workspace",
  editor: "Editor",
};

/** Every reference the catalogue and the definition offer to `nodeId` (or to `when` when null). */
export function inputGroups(def: Definition, catalogue: Catalogue | undefined, nodeId: string | null): InputGroup[] {
  const groups: InputGroup[] = [];
  const trigger = catalogue?.triggers.find((t) => t.name === def.flow.on);
  const option = (reference: string, note: string): InputOption => ({ reference, note, disabled: null, mayBeAbsent: false });

  groups.push({
    label: `Event · ${def.flow.on}`,
    options: (trigger?.payload_fields ?? []).map((f) => option(`event.${f.name}`, f.required ? f.value_type : `${f.value_type}, optional`)),
  });

  const bySource = new Map<string, InputOption[]>();
  for (const input of catalogue?.inputs ?? []) {
    if (!input.triggers.includes(def.flow.on)) continue;
    const list = bySource.get(input.source) ?? [];
    list.push(option(input.name, input.input_type === "image" ? "image" : input.value_type));
    bySource.set(input.source, list);
  }
  for (const [source, options] of bySource) groups.push({ label: SOURCE_LABEL[source] ?? source, options });

  const constants = Object.entries(def.constants).map(([name, value]) => option(`constant.${name}`, String(value)));
  if (constants.length) groups.push({ label: "Constants", options: constants });

  if (nodeId !== null) {
    const up = ancestors(def, nodeId);
    const outputs: InputOption[] = [];
    for (const n of def.node) {
      if (n.id === nodeId) continue;
      const upstream = up.has(n.id);
      const disabled = upstream ? null : "Not upstream of this node: its output cannot be read here.";
      const absent = upstream && mayBeAbsent(def, n.id, nodeId);
      const add = (reference: string, note: string) => outputs.push({ reference, note, disabled, mayBeAbsent: absent });
      if (n.type === "noul") add(`answer.${n.id}`, "noul 0–1");
      else if (n.type === "choice") {
        add(`answer.${n.id}`, "choice");
        add(`answer.${n.id}.confidence`, "number");
      } else if (n.type === "score") {
        add(`answer.${n.id}`, "score");
        add(`answer.${n.id}.confidence`, "number");
      } else if (n.type === "expression") {
        add(`result.${n.id}`, "boolean");
        for (const v of Object.keys(n.assign)) add(`variable.${n.id}.${v}`, "variable");
      }
    }
    // Upstream first, so what can be picked sits on top.
    outputs.sort((a, b) => Number(!!a.disabled) - Number(!!b.disabled));
    if (outputs.length) groups.push({ label: "Upstream answers", options: outputs });
  }
  return groups.filter((g) => g.options.length);
}

/** The alias a new input gets: the last segment of its reference, made unique in the node. */
export function aliasFor(reference: string, taken: Record<string, string>): string {
  const last = reference.split(".").pop() ?? "input";
  const base = /^[A-Za-z_]/.test(last) ? last.replace(/[^A-Za-z0-9_]/g, "_") : `in_${last}`;
  if (!(base in taken)) return base;
  for (let i = 2; ; i++) if (!(`${base}_${i}` in taken)) return `${base}_${i}`;
}

export function toggleInput(node: FlowNode, reference: string): FlowNode {
  const existing = Object.entries(node.inputs).find(([, ref]) => ref === reference);
  if (existing) {
    const inputs = { ...node.inputs };
    delete inputs[existing[0]];
    return { ...node, inputs };
  }
  return { ...node, inputs: { ...node.inputs, [aliasFor(reference, node.inputs)]: reference } };
}

/** Declare once and replace the expression's selection with the actual alias. */
export function insertInputAlias(inputs: Record<string, string>, expression: string, reference: string, start: number, end = start) {
  const alias = Object.entries(inputs).find(([, ref]) => ref === reference)?.[0] ?? aliasFor(reference, inputs);
  return { inputs: { ...inputs, [alias]: reference }, expression: expression.slice(0, start) + alias + expression.slice(end), cursor: start + alias.length };
}

export function defaultDecisionInputs(trigger: Catalogue["triggers"][number] | undefined): Record<string, string> {
  const fields = trigger?.name === "tool.pre_call" ? trigger.payload_fields.filter((f) => f.name === "tool" || f.name === "args") : trigger?.payload_fields.slice(0, 1) ?? [];
  return Object.fromEntries(fields.map((f) => [f.name, `event.${f.name}`]));
}

export const EMPTY_DECISION_WARNING = "No inputs: the instinct has nothing to judge.";
export function hasEmptyDecisionState(node: FlowNode): boolean {
  return isDecision(node) && !Object.keys(node.inputs).length && !node.state_template?.trim();
}

/** The value a reference has in a run (or in the sample payload when there is no run); undefined = absent. */
export function valueOf(reference: string, def: Definition, payload: Record<string, unknown> | undefined, run: RunRecord | null): unknown {
  const [ns, ...rest] = reference.split(".");
  if (ns === "event") return (run?.event.payload ?? payload)?.[rest.join(".")];
  if (ns === "constant") return def.constants[rest.join(".")];
  if (ns === "answer" || ns === "result" || ns === "variable") {
    const node = run?.nodes.find((n) => n.id === rest[0]);
    if (!node || node.status !== "completed") return undefined;
    if (ns === "result") return node.result ?? undefined;
    if (ns === "variable") return node.variables[rest[1] ?? ""];
    const a = node.answer;
    if (!a) return undefined;
    if (rest[1] === "confidence") return "confidence" in a ? a.confidence : undefined;
    return a.type === "noul" ? a.noul : a.type === "choice" ? a.choice : a.score;
  }
  return run?.inputs[reference]?.value ?? undefined;
}

/** Default state rendering (contracts §1): aliases sorted, one `alias = <JSON>` line each. */
export function renderState(node: FlowNode, def: Definition, payload: Record<string, unknown> | undefined, run: RunRecord | null): string {
  const aliases = Object.keys(node.inputs).sort();
  if (!aliases.length) return "(empty state)";
  return aliases
    .map((alias) => {
      const reference = node.inputs[alias]!;
      const value = valueOf(reference, def, payload, run);
      const cut = run?.inputs[reference]?.cut ? " [cut]" : "";
      return `${alias} = ${value === undefined || value === null ? "<absent>" : JSON.stringify(value)}${cut}`;
    })
    .join("\n");
}

/** Inputs a node reads that are upstream outputs it is NOT downstream of (refused at validation). */
export function badUpstreamReads(def: Definition, nodeId: string): string[] {
  const node = nodeById(def, nodeId);
  if (!node) return [];
  const up = ancestors(def, nodeId);
  return Object.values(node.inputs).filter((ref) => {
    const [ns, id] = ref.split(".");
    return (ns === "answer" || ns === "result" || ns === "variable") && !!id && !up.has(id);
  });
}

export function decisionCount(def: Definition): number {
  return def.node.filter(isDecision).length;
}
