// TOML ↔ definition JSON for the mock API only. In the real system the server alone parses and
// writes TOML (contracts §5); the Studio never imports this outside `mock/`.

import { parse, TomlError } from "smol-toml";
import type { Definition, Diagnostic, FallbackAction, FlowNode, ParamValue, Scalar } from "../types";
import { FALLBACK_KEYS } from "../types";

const diag = (code: string, message: string, path = "", extra: Partial<Diagnostic> = {}): Diagnostic => ({ code, message, path, node_id: null, line: null, column: null, ...extra });

type Obj = Record<string, unknown>;
const isObj = (v: unknown): v is Obj => typeof v === "object" && v !== null && !Array.isArray(v) && !(v instanceof Date);

const FLOW_KEYS = ["version", "name", "on", "enabled", "blocking", "budget_ms", "inputs", "when", "start"];
const NODE_KEYS: Record<string, string[]> = {
  noul: ["instructions", "threshold", "state_template"],
  choice: ["instructions", "criteria", "state_template"],
  score: ["instructions", "levels", "at_least", "state_template"],
  expression: ["condition", "assign"],
  action: ["action", "params"],
};

function strMap(v: unknown, path: string, errors: Diagnostic[]): Record<string, string> {
  if (v === undefined) return {};
  if (!isObj(v)) {
    errors.push(diag("schema", `${path} must be a table.`, path));
    return {};
  }
  const out: Record<string, string> = {};
  for (const [k, x] of Object.entries(v)) {
    if (typeof x === "string") out[k] = x;
    else errors.push(diag("schema", `${path}.${k} must be a string.`, `${path}.${k}`));
  }
  return out;
}

function params(v: unknown, path: string, errors: Diagnostic[]): Record<string, ParamValue> {
  if (v === undefined) return {};
  if (!isObj(v)) {
    errors.push(diag("schema", `${path} must be a table.`, path));
    return {};
  }
  const out: Record<string, ParamValue> = {};
  for (const [k, x] of Object.entries(v)) {
    if (typeof x === "string" || typeof x === "number" || typeof x === "boolean") out[k] = x;
    else if (isObj(x) && typeof x.ref === "string" && Object.keys(x).length === 1) out[k] = { ref: x.ref };
    else errors.push(diag("schema", `${path}.${k} must be a scalar or {ref = "<alias>"}.`, `${path}.${k}`));
  }
  return out;
}

function fallbackAction(v: unknown, path: string, errors: Diagnostic[], depth = 0): FallbackAction | null {
  if (v === undefined || v === null) return null;
  if (!isObj(v) || typeof v.action !== "string") {
    errors.push(diag("schema", `${path} must be {action, params, then?}.`, path));
    return null;
  }
  if (depth >= 4) errors.push(diag("limit", `${path}: a fallback chain is at most 4 actions deep.`, path));
  for (const k of Object.keys(v)) if (!["action", "params", "then"].includes(k)) errors.push(diag("schema", `Unknown field ${path}.${k}.`, `${path}.${k}`));
  return { action: v.action, params: params(v.params, `${path}.params`, errors), then: fallbackAction(v.then, `${path}.then`, errors, depth + 1) };
}

/** Line of the `id = "<id>"` of a node, for diagnostics that point into the text. */
function lineOf(text: string, needle: RegExp): number | null {
  const lines = text.split("\n");
  const i = lines.findIndex((l) => needle.test(l));
  return i < 0 ? null : i + 1;
}

export function parseFlowToml(text: string): { definition: Definition | null; errors: Diagnostic[] } {
  let raw: Obj;
  try {
    raw = parse(text) as Obj;
  } catch (e) {
    if (e instanceof TomlError) return { definition: null, errors: [diag("parse", e.message.split("\n")[0] ?? "Invalid TOML", "", { line: e.line, column: e.column })] };
    return { definition: null, errors: [diag("parse", String(e))] };
  }
  raw = JSON.parse(JSON.stringify(raw), (key, value) => key === "action" && value === "ask_driver" ? "ask_captain" : value);
  return fromRaw(raw, text);
}

function fromRaw(raw: Obj, text: string): { definition: Definition | null; errors: Diagnostic[] } {
  const errors: Diagnostic[] = [];
  for (const k of Object.keys(raw)) if (!["flow", "constants", "node", "fallback", "layout"].includes(k)) errors.push(diag("schema", `Unknown table [${k}].`, k));
  const flow = raw.flow;
  if (!isObj(flow)) return { definition: null, errors: [...errors, diag("schema", "The [flow] table is required.", "flow")] };
  for (const k of Object.keys(flow)) if (!FLOW_KEYS.includes(k)) errors.push(diag("schema", `Unknown field flow.${k}.`, `flow.${k}`, { line: lineOf(text, new RegExp(`^${k}\\s*=`)) }));
  if (flow.version !== 1) errors.push(diag("schema", "flow.version must be 1.", "flow.version"));
  if (typeof flow.name !== "string") errors.push(diag("schema", "flow.name is required.", "flow.name"));
  if (typeof flow.on !== "string") errors.push(diag("schema", "flow.on is required.", "flow.on"));
  const start = Array.isArray(flow.start) ? flow.start.filter((s): s is string => typeof s === "string") : [];
  if (!Array.isArray(flow.start) || !start.length) errors.push(diag("schema", "flow.start must list at least one node.", "flow.start"));

  const constants: Record<string, Scalar> = {};
  if (raw.constants !== undefined) {
    if (!isObj(raw.constants)) errors.push(diag("schema", "[constants] must be a table.", "constants"));
    else
      for (const [k, v] of Object.entries(raw.constants)) {
        if (typeof v === "string" || typeof v === "number" || typeof v === "boolean") constants[k] = v;
        else errors.push(diag("schema", `constants.${k} must be a string, number or boolean.`, `constants.${k}`));
      }
  }

  const nodes: FlowNode[] = [];
  const rawNodes = Array.isArray(raw.node) ? raw.node : [];
  if (!rawNodes.length) errors.push(diag("schema", "A flow needs at least one [[node]].", "node"));
  for (const [i, n] of rawNodes.entries()) {
    if (!isObj(n) || typeof n.id !== "string" || typeof n.type !== "string") {
      errors.push(diag("schema", `node ${i + 1} needs an id and a type.`, `node[${i}]`));
      continue;
    }
    const path = `node[${n.id}]`;
    const at = { node_id: n.id, line: lineOf(text, new RegExp(`^id\\s*=\\s*["']${n.id}["']`)) };
    const allowed = NODE_KEYS[n.type];
    if (!allowed) {
      errors.push(diag("schema", `Unknown node type "${n.type}".`, `${path}.type`, at));
      continue;
    }
    for (const k of Object.keys(n)) if (![...allowed, "id", "type", "label", "inputs", "next"].includes(k)) errors.push(diag("schema", `${n.type} nodes have no field "${k}".`, `${path}.${k}`, at));
    const next: Record<string, string[]> = {};
    if (isObj(n.next))
      for (const [p, v] of Object.entries(n.next)) {
        if (Array.isArray(v) && v.every((x) => typeof x === "string")) next[p] = v as string[];
        else errors.push(diag("schema", `${path}.next.${p} must be a list of node ids.`, `${path}.next.${p}`, at));
      }
    const base = { id: n.id, label: typeof n.label === "string" ? n.label : null, inputs: strMap(n.inputs, `${path}.inputs`, errors), next };
    const instructions = typeof n.instructions === "string" ? n.instructions : "";
    const state_template = typeof n.state_template === "string" ? n.state_template : null;
    const need = (k: string, ok: boolean) => !ok && errors.push(diag("schema", `${n.type} node "${n.id}" needs ${k}.`, `${path}.${k}`, at));
    switch (n.type) {
      case "noul":
        need("instructions", typeof n.instructions === "string");
        need("threshold", typeof n.threshold === "number" || typeof n.threshold === "string");
        nodes.push({ ...base, type: "noul", instructions, threshold: typeof n.threshold === "number" || typeof n.threshold === "string" ? n.threshold : 0.5, state_template });
        break;
      case "choice":
        need("instructions", typeof n.instructions === "string");
        need("criteria", isObj(n.criteria));
        nodes.push({ ...base, type: "choice", instructions, criteria: strMap(n.criteria, `${path}.criteria`, errors), state_template });
        break;
      case "score": {
        const levels = Array.isArray(n.levels) ? n.levels.filter((x): x is string => typeof x === "string") : [];
        need("instructions", typeof n.instructions === "string");
        need("levels", levels.length > 0);
        need("at_least", typeof n.at_least === "string");
        nodes.push({ ...base, type: "score", instructions, levels, at_least: typeof n.at_least === "string" ? n.at_least : "", state_template });
        break;
      }
      case "expression":
        need("condition", typeof n.condition === "string");
        nodes.push({ ...base, type: "expression", condition: typeof n.condition === "string" ? n.condition : "", assign: strMap(n.assign, `${path}.assign`, errors) });
        break;
      case "action":
        need("action", typeof n.action === "string");
        nodes.push({ ...base, type: "action", action: typeof n.action === "string" ? n.action : "", params: params(n.params, `${path}.params`, errors) });
        break;
    }
  }

  let fallback: Definition["fallback"] = null;
  if (raw.fallback !== undefined) {
    if (!isObj(raw.fallback)) errors.push(diag("schema", "[fallback] must be a table.", "fallback"));
    else {
      for (const k of Object.keys(raw.fallback)) if (![...FALLBACK_KEYS, "inputs"].includes(k)) errors.push(diag("schema", `Unknown field fallback.${k}.`, `fallback.${k}`));
      const fb = raw.fallback;
      fallback = {
        inputs: strMap(fb.inputs, "fallback.inputs", errors),
        on_failure: fallbackAction(fb.on_failure, "fallback.on_failure", errors),
        on_timeout: fallbackAction(fb.on_timeout, "fallback.on_timeout", errors),
        on_provider_error: fallbackAction(fb.on_provider_error, "fallback.on_provider_error", errors),
        on_invalid_answer: fallbackAction(fb.on_invalid_answer, "fallback.on_invalid_answer", errors),
      };
    }
  }

  const layout: Definition["layout"] = {};
  if (isObj(raw.layout))
    for (const [k, v] of Object.entries(raw.layout)) {
      if (isObj(v) && typeof v.x === "number" && typeof v.y === "number" && Number.isFinite(v.x) && Number.isFinite(v.y)) layout[k] = { x: v.x, y: v.y };
      else errors.push(diag("schema", `layout.${k} must be {x, y}.`, `layout.${k}`));
    }

  const schemaBroken = errors.some((e) => e.code === "schema" && (e.path === "flow" || e.path.startsWith("flow.name") || e.path.startsWith("flow.on")));
  if (schemaBroken) return { definition: null, errors };
  const definition: Definition = {
    flow: {
      version: 1,
      name: String(flow.name),
      on: String(flow.on),
      enabled: typeof flow.enabled === "boolean" ? flow.enabled : true,
      blocking: typeof flow.blocking === "boolean" ? flow.blocking : false,
      budget_ms: typeof flow.budget_ms === "number" ? flow.budget_ms : null,
      inputs: strMap(flow.inputs, "flow.inputs", []),
      when: typeof flow.when === "string" ? flow.when : null,
      start,
    },
    constants,
    node: nodes,
    fallback,
    layout,
  };
  return { definition, errors };
}

// ---------- writing ----------

const key = (k: string) => (/^[A-Za-z0-9_-]+$/.test(k) ? k : JSON.stringify(k));
function value(v: unknown): string {
  if (typeof v === "string") return v.includes('"') && !v.includes("'") && !v.includes("\n") ? `'${v}'` : JSON.stringify(v);
  if (typeof v === "number" || typeof v === "boolean") return String(v);
  if (Array.isArray(v)) return `[${v.map(value).join(", ")}]`;
  if (isObj(v)) {
    const entries = Object.entries(v).filter(([, x]) => x !== null && x !== undefined);
    return entries.length ? `{ ${entries.map(([k, x]) => `${key(k)} = ${value(x)}`).join(", ")} }` : "{}";
  }
  return '""';
}
const table = (name: string, record: Record<string, unknown>, always = false) => {
  const entries = Object.entries(record);
  if (!entries.length && !always) return [];
  return [`[${name}]`, ...entries.map(([k, v]) => `${key(k)} = ${value(v)}`)];
};

/** Canonical TOML of a definition: the shape of contracts §1, comments not kept. */
export function flowToToml(def: Definition): string {
  const f = def.flow;
  const L: string[] = ["[flow]", "version = 1", `name = ${value(f.name)}`, `on = ${value(f.on)}`, `enabled = ${f.enabled}`];
  if (f.blocking) L.push("blocking = true");
  if (f.budget_ms !== null) L.push(`budget_ms = ${f.budget_ms}`);
  L.push(`start = ${value(f.start)}`);
  if (f.when !== null) L.push(`when = ${value(f.when)}`);
  L.push(...table("flow.inputs", f.inputs));
  if (Object.keys(def.constants).length) L.push("", ...table("constants", def.constants));
  if (def.fallback) {
    L.push("", "[fallback]");
    for (const k of FALLBACK_KEYS) if (def.fallback[k]) L.push(`${k} = ${value(def.fallback[k])}`);
    L.push(...table("fallback.inputs", def.fallback.inputs));
  }
  for (const n of def.node) {
    L.push("", "[[node]]", `id = ${value(n.id)}`, `type = ${value(n.type)}`);
    if (n.label !== null) L.push(`label = ${value(n.label)}`);
    if (n.type === "noul" || n.type === "choice" || n.type === "score") L.push(`instructions = ${value(n.instructions)}`);
    if (n.type === "noul") L.push(`threshold = ${value(n.threshold)}`);
    if (n.type === "score") L.push(`levels = ${value(n.levels)}`, `at_least = ${value(n.at_least)}`);
    if (n.type === "expression") L.push(`condition = ${value(n.condition)}`);
    if (n.type === "action") L.push(`action = ${value(n.action)}`);
    if ((n.type === "noul" || n.type === "choice" || n.type === "score") && n.state_template !== null) L.push(`state_template = ${value(n.state_template)}`);
    L.push(...table("node.inputs", n.inputs));
    if (n.type === "choice") L.push(...table("node.criteria", n.criteria));
    if (n.type === "expression") L.push(...table("node.assign", n.assign));
    if (n.type === "action") L.push(...table("node.params", n.params, true));
    L.push(...table("node.next", Object.fromEntries(Object.entries(n.next).filter(([, v]) => v.length))));
  }
  if (Object.keys(def.layout).length) L.push("", ...table("layout", def.layout));
  return `${L.join("\n")}\n`;
}
