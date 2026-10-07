// Semantic validation for the mock API: a faithful-enough subset of the engine's checks
// (spec §2.3, contracts §1–§2) so the Studio shows real diagnostics before the server exists.
// Expressions are not parsed here; the engine does that.

import { FLOW_NAME_PATTERN, ID_PATTERN, isDecision, portsOf, TRIGGER_ID, topoOrder, wiresOf, ancestors, reaches } from "../graph";
import type { Catalogue, Definition, Diagnostic, FallbackAction, FlowNode, ParamValue } from "../types";
import { FALLBACK_KEYS } from "../types";

const d = (code: string, message: string, path: string, node_id: string | null = null): Diagnostic => ({ code, message, path, node_id, line: null, column: null });

export function validateDefinition(def: Definition, catalogue: Catalogue): Diagnostic[] {
  const out: Diagnostic[] = [];
  const f = def.flow;
  const trigger = catalogue.triggers.find((t) => t.name === f.on);
  const limits = catalogue.limits;
  if (!FLOW_NAME_PATTERN.test(f.name)) out.push(d("schema", "A flow name is lowercase letters, digits and dashes, starting with a letter.", "flow.name"));
  if (!trigger) out.push(d("unknown_trigger", `Unknown trigger "${f.on}".`, "flow.on"));
  if (f.blocking && trigger && !trigger.can_block) out.push(d("schema", `${f.on} cannot block: make the flow asynchronous.`, "flow.blocking"));
  if (f.blocking && (f.budget_ms === null || f.budget_ms < 1 || f.budget_ms > (limits.blocking_budget_ms ?? 2000)))
    out.push(d("limit", `A blocking flow needs budget_ms between 1 and ${limits.blocking_budget_ms ?? 2000}.`, "flow.budget_ms"));
  if (!f.blocking && f.budget_ms !== null) out.push(d("schema", "budget_ms is only for blocking flows.", "flow.budget_ms"));
  if (def.node.length > (limits.nodes ?? 64)) out.push(d("limit", `A flow has at most ${limits.nodes ?? 64} nodes.`, "node"));

  const ids = new Set<string>();
  for (const n of def.node) {
    if (!ID_PATTERN.test(n.id) || n.id === TRIGGER_ID) out.push(d("schema", `"${n.id}" is not a valid node id.`, `node[${n.id}].id`, n.id));
    if (ids.has(n.id)) out.push(d("schema", `Two nodes are called "${n.id}".`, `node[${n.id}].id`, n.id));
    ids.add(n.id);
  }

  // Wires.
  for (const s of f.start) if (!ids.has(s)) out.push(d("unknown_node", `The trigger wires to unknown node "${s}".`, "flow.start"));
  for (const n of def.node) {
    const ports = portsOf(n);
    for (const [port, targets] of Object.entries(n.next)) {
      if (!ports.includes(port)) out.push(d("unknown_port", `"${n.id}" has no port "${port}".`, `node[${n.id}].next.${port}`, n.id));
      for (const t of targets) {
        if (!ids.has(t)) out.push(d("unknown_node", `"${n.id}" wires to unknown node "${t}".`, `node[${n.id}].next.${port}`, n.id));
        else if (n.type === "action" && def.node.find((x) => x.id === t)?.type !== "action") out.push(d("schema", `The then port of "${n.id}" leads only to actions.`, `node[${n.id}].next.then`, n.id));
      }
    }
  }
  const cyclic = def.node.filter((n) => reaches(def, n.id, n.id));
  for (const n of cyclic) out.push(d("cycle", `"${n.id}" is on a cycle: a flow runs each node at most once.`, `node[${n.id}].next`, n.id));
  if (!cyclic.length)
    for (const n of def.node) if (!reaches(def, TRIGGER_ID, n.id)) out.push(d("unreachable", `Nothing wires to "${n.id}": it can never run.`, `node[${n.id}]`, n.id));

  // Inputs.
  const hostInputs = new Set((catalogue.inputs ?? []).filter((i) => i.triggers.includes(f.on)).map((i) => i.name));
  const eventFields = new Set((trigger?.payload_fields ?? []).map((p) => p.name));
  const checkRef = (ref: string, path: string, nodeId: string | null, graph: boolean) => {
    const [ns, ...rest] = ref.split(".");
    const name = rest.join(".");
    if (ns === "event") {
      if (!eventFields.has(name)) out.push(d("unknown_input", `${f.on} has no payload field "${name}".`, path, nodeId));
    } else if (ns === "constant") {
      if (!(name in def.constants)) out.push(d("unknown_input", `No constant "${name}".`, path, nodeId));
    } else if (ns === "answer" || ns === "result" || ns === "variable") {
      if (!graph || nodeId === null) out.push(d("unknown_input", `${ref} cannot be read here.`, path, nodeId));
      else if (!ids.has(rest[0] ?? "")) out.push(d("unknown_input", `No node "${rest[0]}".`, path, nodeId));
      else if (!ancestors(def, nodeId).has(rest[0]!)) out.push(d("not_upstream", `"${rest[0]}" is not upstream of "${nodeId}": its output cannot be read here.`, path, nodeId));
    } else if (!hostInputs.has(ref)) out.push(d("unknown_input", `${ref} is not offered on ${f.on}.`, path, nodeId));
  };
  for (const [alias, ref] of Object.entries(f.inputs)) checkRef(ref, `flow.inputs.${alias}`, null, false);
  for (const n of def.node) {
    if (Object.keys(n.inputs).length > 16) out.push(d("limit", `"${n.id}" reads more than 16 inputs.`, `node[${n.id}].inputs`, n.id));
    for (const [alias, ref] of Object.entries(n.inputs)) {
      if (!ID_PATTERN.test(alias)) out.push(d("schema", `"${alias}" is not a valid input name.`, `node[${n.id}].inputs.${alias}`, n.id));
      checkRef(ref, `node[${n.id}].inputs.${alias}`, n.id, true);
    }
    checkNode(n, def, out);
  }

  // Actions.
  const checkAction = (name: string, params: Record<string, ParamValue>, path: string, nodeId: string | null, aliases: Set<string>) => {
    const spec = catalogue.actions.find((a) => a.name === name);
    if (!spec) return out.push(d("unknown_action", `Unknown action "${name}".`, `${path}.action`, nodeId));
    if (!spec.allowed_triggers.includes(f.on)) out.push(d("action_not_allowed", `${name} is not allowed on ${f.on}.`, `${path}.action`, nodeId));
    if (spec.kind === "verdict" && !f.blocking) out.push(d("action_not_allowed", `${name} is a verdict: only a blocking flow can use it.`, `${path}.action`, nodeId));
    for (const p of spec.params) {
      const v = params[p.name];
      if (v === undefined || (p.required && v === "")) out.push(d("invalid_parameter", `${name} needs "${p.name}".`, `${path}.params.${p.name}`, nodeId));
      else if (typeof v === "object" && !aliases.has(v.ref)) out.push(d("invalid_parameter", `"${v.ref}" is not an input of this node.`, `${path}.params.${p.name}`, nodeId));
    }
    for (const k of Object.keys(params)) if (!spec.params.some((p) => p.name === k)) out.push(d("invalid_parameter", `${name} has no parameter "${k}".`, `${path}.params.${k}`, nodeId));
  };
  for (const n of def.node) if (n.type === "action") checkAction(n.action, n.params, `node[${n.id}]`, n.id, new Set(Object.keys(n.inputs)));
  if (def.fallback) {
    const aliases = new Set([...Object.keys(def.fallback.inputs), "failure_kind", "failure_message"]);
    for (const [alias, ref] of Object.entries(def.fallback.inputs)) checkRef(ref, `fallback.inputs.${alias}`, null, false);
    for (const k of FALLBACK_KEYS) {
      let a: FallbackAction | null = def.fallback[k];
      let path = `fallback.${k}`;
      let verdicts = 0;
      while (a) {
        checkAction(a.action, a.params, path, null, aliases);
        if (catalogue.actions.find((x) => x.name === a!.action)?.kind === "verdict") verdicts++;
        a = a.then;
        path += ".then";
      }
      if (verdicts > 1) out.push(d("conflicting_verdicts", `fallback.${k} chains two verdicts.`, `fallback.${k}`));
    }
  }

  if (!cyclic.length) out.push(...staticChecks(def, catalogue));
  return out;
}

function checkNode(n: FlowNode, def: Definition, out: Diagnostic[]) {
  const at = `node[${n.id}]`;
  if (isDecision(n) && !n.instructions.trim()) out.push(d("schema", `"${n.id}" needs instructions: the question the instinct answers.`, `${at}.instructions`, n.id));
  if (n.type === "noul") {
    const t = typeof n.threshold === "string" ? (n.threshold.startsWith("constant.") ? def.constants[n.threshold.slice(9)] : undefined) : n.threshold;
    if (typeof t !== "number" || t < 0 || t > 1) out.push(d("schema", `The threshold of "${n.id}" must be a number from 0 to 1, or a constant holding one.`, `${at}.threshold`, n.id));
  }
  if (n.type === "choice") {
    const keys = Object.keys(n.criteria);
    if (keys.length < 2 || keys.length > 16) out.push(d("schema", `"${n.id}" needs 2 to 16 options.`, `${at}.criteria`, n.id));
    for (const k of keys) {
      if (!ID_PATTERN.test(k)) out.push(d("schema", `"${k}" is not a valid option name.`, `${at}.criteria.${k}`, n.id));
      if (!n.criteria[k]!.trim()) out.push(d("schema", `Option "${k}" of "${n.id}" needs criteria.`, `${at}.criteria.${k}`, n.id));
    }
  }
  if (n.type === "score") {
    if (n.levels.length < 2 || n.levels.length > 10) out.push(d("schema", `"${n.id}" needs 2 to 10 levels.`, `${at}.levels`, n.id));
    if (new Set(n.levels).size !== n.levels.length || n.levels.some((l) => !l.trim())) out.push(d("schema", `The levels of "${n.id}" must be distinct and not empty.`, `${at}.levels`, n.id));
    if (!n.levels.includes(n.at_least)) out.push(d("schema", `"${n.at_least}" is not one of the levels of "${n.id}".`, `${at}.at_least`, n.id));
  }
  if (n.type === "expression") {
    if (!n.condition.trim()) out.push(d("expression", `"${n.id}" needs a condition.`, `${at}.condition`, n.id));
    const names = new Set([...Object.keys(n.inputs), ...Object.keys(n.assign)]);
    const used = (n.condition.replace(/"[^"]*"|'[^']*'/g, "").match(/[A-Za-z_][A-Za-z0-9_]*/g) ?? []).filter((w) => !["true", "false", "if", "is_set"].includes(w));
    for (const w of new Set(used)) if (!names.has(w)) out.push(d("expression", `"${w}" is not an input or variable of "${n.id}".`, `${at}.condition`, n.id));
    if (n.condition.length > 4096) out.push(d("limit", `The condition of "${n.id}" is over 4096 bytes.`, `${at}.condition`, n.id));
  }
}

/**
 * Port-choice analysis (contracts §2): every combination of one port per decision and expression is
 * possible. Two verdicts reachable together → conflicting_verdicts; more than 16 decisions → limit.
 */
function staticChecks(def: Definition, catalogue: Catalogue): Diagnostic[] {
  const verdict = new Set(catalogue.actions.filter((a) => a.kind === "verdict").map((a) => a.name));
  const order = topoOrder(def);
  const byId = new Map(def.node.map((n) => [n.id, n]));
  const wires = wiresOf(def);
  let steps = 0;
  let conflict: string[] | null = null;
  let tooMany = false;
  const limit = catalogue.limits.decisions_per_run ?? 16;

  const walk = (i: number, reached: Set<string>, chosen: Map<string, string>): boolean => {
    if (++steps > 100_000) return true;
    if (i === order.length) {
      const verdicts = [...reached].filter((id) => {
        const n = byId.get(id);
        return n?.type === "action" && verdict.has(n.action);
      });
      if (verdicts.length > 1 && !conflict) conflict = verdicts;
      if ([...reached].filter((id) => byId.get(id) && isDecision(byId.get(id)!)).length > limit) tooMany = true;
      return !!conflict;
    }
    const id = order[i]!;
    const n = byId.get(id)!;
    const isReached = wires.some((w) => w.target === id && (w.source === TRIGGER_ID || (reached.has(w.source) && (byId.get(w.source)?.type === "action" || chosen.get(w.source) === w.port))));
    if (!isReached) return walk(i + 1, reached, chosen);
    const next = new Set(reached).add(id);
    if (n.type === "action") return walk(i + 1, next, chosen);
    for (const port of portsOf(n)) {
      if (walk(i + 1, next, new Map(chosen).set(id, port))) return true;
    }
    return false;
  };
  walk(0, new Set(), new Map());
  const out: Diagnostic[] = [];
  if (steps > 100_000) out.push(d("limit", "The flow has too many port combinations to check.", "node"));
  if (conflict) {
    const ids = conflict as string[];
    out.push(d("conflicting_verdicts", `Two verdicts can be reached in the same run: ${ids.join(" and ")}. Only one verdict may apply.`, `node[${ids[1]}]`, ids[1] ?? null));
  }
  if (tooMany) out.push(d("limit", `More than ${limit} decisions can run in one run.`, "node"));
  return out;
}
