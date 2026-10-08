// An in-memory implementation of the flows routes (contracts §5), with the same shapes and error
// envelopes, so the Studio can be built and tried before the server has them. Nothing is written
// to disk; a page reload starts over from the seed. Sample runs invent the instinct's answers.

import { HttpError } from "../../api/http";
import type { StudioApi } from "../api";
import { isDecision, portsOf, TRIGGER_ID, topoOrder, wiresOf } from "../graph";
import { renderState } from "../inputs";
import type {
  Answer, Content, Definition, Diagnostic, FlowDocument, FlowSource, FlowSummary, Location, ProposedAction,
  ReplayResult, RunNode, RunRecord, Trust, ValidateResult,
} from "../types";
import { evaluate } from "./expr";
import { BUILTIN_TOML, CATALOGUE, GUARD_MUTATIONS_TOML, HOST_DIAGNOSTICS, RELOAD_PROPAGATION, REMARK_ON_SAVE_TOML, mixedApplicationRunRecord, sampleRunRecord, suppressedRunRecord } from "./fixtures";
import { flowToToml, parseFlowToml } from "./toml";
import { validateDefinition } from "./validate";

interface Stored {
  name: string;
  location: Location;
  toml: string;
  templates: Record<string, string>;
}

const key = (name: string, location: Location) => `${location}:${name}`;
const pause = (ms = 90) => new Promise((r) => setTimeout(r, ms));
const clone = <T>(v: T): T => structuredClone(v);

/** FNV-1a, hex: stands in for the server's sha256 revisions and digests. */
function hash(text: string): string {
  let h = 0x811c9dc5;
  for (let i = 0; i < text.length; i++) h = Math.imul(h ^ text.charCodeAt(i), 0x01000193) >>> 0;
  return h.toString(16).padStart(8, "0");
}
const revisionOf = (s: Stored) => `sha256:${hash(s.toml + JSON.stringify(Object.entries(s.templates).sort()))}`;

function fail(status: number, reason: string, error: string, errors: Diagnostic[] = []): never {
  throw new HttpError(status, error, reason, { error, reason, errors });
}

function ulid(): string {
  const A = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
  let s = Date.now().toString(32).toUpperCase().padStart(10, "0");
  for (let i = 0; i < 16; i++) s += A[Math.floor(Math.random() * 32)];
  return s;
}

export function createMockApi(workspace: string): StudioApi {
  const files = new Map<string, Stored>();
  const put = (s: Stored) => files.set(key(s.name, s.location), s);
  put({ name: "guard-mutations", location: "workspace", toml: GUARD_MUTATIONS_TOML, templates: {} });
  put({ name: "public-api-drift", location: "global", toml: REMARK_ON_SAVE_TOML, templates: {} });
  put({ name: "turn-digest", location: "builtin", toml: BUILTIN_TOML, templates: {} });
  /** What the last successful reload loaded: file key → revision. */
  let loaded = new Map([...files.values()].map((s) => [key(s.name, s.location), revisionOf(s)]));
  let trusted = false;
  const runs: RunRecord[] = [sampleRunRecord(`sha256:${hash(GUARD_MUTATIONS_TOML)}`), suppressedRunRecord(), mixedApplicationRunRecord()];
  let reloadErrors: Diagnostic[] = [];

  const compile = (toml: string) => {
    const parsed = parseFlowToml(toml);
    if (!parsed.definition) return parsed;
    return { definition: parsed.definition, errors: [...parsed.errors, ...validateDefinition(parsed.definition, CATALOGUE)] };
  };

  const document = (s: Stored): FlowDocument => {
    const { definition, errors } = compile(s.toml);
    const base = s.location === "workspace" ? `${workspace}/.withnav/flows` : s.location === "global" ? "~/.config/withnav/flows" : "builtin:";
    return {
      name: s.name,
      location: s.location,
      path: s.location === "builtin" ? `builtin:${s.name}` : `${base}/${s.name}.toml`,
      revision: revisionOf(s),
      toml: s.toml,
      definition,
      templates: { ...s.templates },
      errors,
      saved_since_reload: loaded.get(key(s.name, s.location)) !== revisionOf(s),
      read_only: s.location === "builtin",
    };
  };

  const summary = (s: Stored): FlowSummary => {
    const doc = document(s);
    const f = doc.definition?.flow;
    return { name: s.name, trigger: f?.on ?? null, location: s.location, enabled: f?.enabled ?? null, blocking: f?.blocking ?? null, saved_since_reload: doc.saved_since_reload, errors: doc.errors };
  };

  const sorted = () =>
    [...files.values()].map(summary).sort((a, b) => (a.trigger ?? "").localeCompare(b.trigger ?? "") || a.name.localeCompare(b.name) || a.location.localeCompare(b.location));

  /** Content → { toml, definition, errors } exactly as validate answers. */
  const fromContent = (content: Content): ValidateResult => {
    if (content.format === "toml") {
      const { definition, errors } = compile(content.toml);
      return { valid: !!definition && !errors.length, toml: content.toml, definition, templates: content.templates, errors };
    }
    const toml = flowToToml(content.definition);
    const { definition, errors } = compile(toml);
    return { valid: !errors.length, toml: definition ? toml : null, definition: definition ?? content.definition, templates: content.templates, errors };
  };

  const resolveSource = (source: FlowSource): { definition: Definition; workspaceScoped: boolean } => {
    if ("saved" in source) {
      const s = files.get(key(source.saved.name, source.saved.location));
      if (!s) fail(404, "not_found", `No flow ${source.saved.name} in ${source.saved.location}.`);
      if (revisionOf(s) !== source.saved.revision) fail(409, "stale", "The file changed since it was opened. Reopen it.");
      const doc = document(s);
      if (!doc.definition || doc.errors.length) fail(400, "invalid", "The saved flow is not valid.", doc.errors);
      return { definition: doc.definition, workspaceScoped: s.location === "workspace" };
    }
    const v = fromContent(source.content);
    if (!v.valid || !v.definition) fail(400, "invalid", "The flow is not valid.", v.errors);
    return { definition: v.definition, workspaceScoped: true };
  };

  const requireTrust = (workspaceScoped: boolean) => {
    if (workspaceScoped && !trusted) fail(403, "untrusted", "This workspace's flows are not trusted: trust them to run a sample or a replay.");
  };

  return {
    mocked: true,
    async list() {
      await pause(40);
      return { flows: sorted(), errors: clone([...HOST_DIAGNOSTICS, ...reloadErrors]) };
    },
    async get(name, location) {
      await pause(40);
      const s = files.get(key(name, location));
      if (!s) fail(404, "not_found", `No flow ${name} in ${location}.`);
      return document(s);
    },
    async create({ name, location, content }) {
      await pause();
      if (!/^[a-z][a-z0-9-]{0,63}$/.test(name)) fail(400, "invalid", "A flow name is lowercase letters, digits and dashes, starting with a letter.");
      if (files.has(key(name, location))) fail(409, "already_exists", `A ${location} flow called ${name} already exists.`);
      const v = fromContent(content);
      if (content.format === "graph" && v.toml === null) fail(400, "invalid", "This graph cannot be written as TOML.", v.errors);
      const s: Stored = { name, location, toml: content.format === "toml" ? content.toml : v.toml!, templates: content.templates };
      put(s);
      return document(s);
    },
    async save(name, { location, revision, content }) {
      await pause();
      const s = files.get(key(name, location));
      if (!s) fail(404, "not_found", `No flow ${name} in ${location}.`);
      if (s.location === "builtin") fail(409, "read_only", "Built-in flows cannot be edited. Save a copy to the workspace or your configuration.");
      if (revisionOf(s) !== revision) fail(409, "stale", "The file changed on disk since it was opened. Reload it before saving.");
      const v = fromContent(content);
      if (content.format === "graph" && (v.toml === null || !v.valid)) fail(400, "invalid", "An invalid graph cannot be saved. Fix the errors, or save the TOML text.", v.errors);
      if (v.definition && v.definition.flow.name !== name) fail(400, "invalid", "A flow cannot be renamed by saving: its name is its file name.");
      s.toml = content.format === "toml" ? content.toml : v.toml!;
      s.templates = content.templates;
      return document(s);
    },
    async remove(name, { location, revision }) {
      await pause();
      const s = files.get(key(name, location));
      if (!s) fail(404, "not_found", `No flow ${name} in ${location}.`);
      if (s.location === "builtin") fail(409, "read_only", "Built-in flows cannot be deleted.");
      if (revisionOf(s) !== revision) fail(409, "stale", "The file changed since it was opened.");
      files.delete(key(name, location));
      return { deleted: true };
    },
    async validate(content) {
      await pause(60);
      return fromContent(content);
    },
    async catalogue() {
      await pause(30);
      return clone(CATALOGUE);
    },
    async sample({ source, payload }) {
      await pause(350);
      const { definition, workspaceScoped } = resolveSource(source);
      requireTrust(workspaceScoped);
      const record = simulate(definition, payload, "sample");
      runs.unshift(record);
      return clone(record);
    },
    async runs(name, opts = {}) {
      await pause(40);
      const all = runs.filter((r) => r.flow_name === name);
      const start = opts.before ? all.findIndex((r) => r.run_id === opts.before) + 1 : 0;
      const page = all.slice(start, start + (opts.limit ?? 20));
      const more = start + page.length < all.length;
      return {
        runs: page.map((r) => ({ run_id: r.run_id, flow_name: r.flow_name, started_at: r.started_at, status: r.status, duration_ms: r.duration_ms, cost_usd: r.cost_usd, mode: r.mode })),
        next_cursor: more ? page[page.length - 1]!.run_id : null,
      };
    },
    async run(name, runId) {
      await pause(40);
      const r = runs.find((x) => x.flow_name === name && x.run_id === runId);
      if (!r) fail(404, "not_found", `No run ${runId} of ${name}.`);
      return clone(r);
    },
    async replay({ source, run_id }) {
      await pause(120);
      const { definition, workspaceScoped } = resolveSource(source);
      requireTrust(workspaceScoped);
      const record = runs.find((r) => r.run_id === run_id);
      if (!record) fail(404, "not_found", `No run ${run_id}.`);
      return replay(definition, record);
    },
    async reload() {
      await pause(200);
      const errors: Diagnostic[] = [];
      for (const s of files.values()) {
        if (s.location === "workspace" && !trusted) continue;
        for (const e of document(s).errors) errors.push({ ...e, path: `${s.location}:${s.name} ${e.path}`.trim() });
      }
      reloadErrors = errors;
      if (errors.length) fail(422, "reload_invalid", `Nothing reloaded: ${errors.length} error${errors.length === 1 ? "" : "s"}. The previous flows keep running.`, errors);
      loaded = new Map([...files.values()].map((s) => [key(s.name, s.location), revisionOf(s)]));
      return { reloaded: true, errors: [], flows: sorted(), propagation: RELOAD_PROPAGATION };
    },
    async health() {
      await pause(40);
      const end = new Date();
      const rows = [...files.values()].map((s) => {
        const seed = parseInt(hash(s.name).slice(0, 4), 16);
        const runsCount = s.location === "builtin" ? 0 : 12 + (seed % 140);
        const failures = Math.floor(runsCount * ((seed % 7) / 100));
        return { name: s.name, runs: runsCount, failures, fallbacks_applied: failures, cost_usd: null, cost_unknown_runs: runsCount, unavailable: s.location !== "builtin", errors: s.location !== "builtin" ? clone(HOST_DIAGNOSTICS) : [] };
      });
      return { window_start: new Date(end.getTime() - 86_400_000).toISOString(), window_end: end.toISOString(), flows: rows };
    },
    async trust(): Promise<Trust> {
      await pause(30);
      return { workspace, has_workspace_flows: [...files.values()].some((s) => s.location === "workspace"), trusted };
    },
    async setTrust(value) {
      await pause(60);
      trusted = value;
      return { workspace, has_workspace_flows: [...files.values()].some((s) => s.location === "workspace"), trusted };
    },
  };
}

// ---------- simulated engine ----------

/** A deterministic pseudo-random number in [0, 1) from a string. */
const unit = (text: string) => parseInt(hash(text), 16) / 0x1_0000_0000;

function inventAnswer(node: Definition["node"][number], state: string): Answer | null {
  const r = unit(`${node.id}\n${state}`);
  if (node.type === "noul") return { type: "noul", noul: Math.round(r * 100) / 100 };
  if (node.type === "choice") {
    const keys = Object.keys(node.criteria);
    const top = Math.floor(r * keys.length);
    const rest = keys.length > 1 ? (1 - 0.7) / (keys.length - 1) : 0;
    const probabilities = Object.fromEntries(keys.map((k, i) => [k, Math.round((i === top ? 0.7 : rest) * 100) / 100]));
    return { type: "choice", choice: keys[top]!, probabilities, confidence: 0.7 };
  }
  if (node.type === "score") {
    const score = Math.round(r * (node.levels.length - 1) * 10) / 10;
    const legend = Object.fromEntries(node.levels.map((l, i) => [String(i), l]));
    const probabilities = Object.fromEntries(node.levels.map((_, i) => [String(i), Math.round(Math.max(0, 1 - Math.abs(i - score)) / Math.max(1, node.levels.length / 2) * 100) / 100]));
    return { type: "score", score, legend, probabilities, confidence: 0.6 };
  }
  return null;
}

function portOf(node: Definition["node"][number], answer: Answer, def: Definition): string {
  if (node.type === "noul" && answer.type === "noul") {
    const t = typeof node.threshold === "number" ? node.threshold : Number(def.constants[node.threshold.replace(/^constant\./, "")] ?? 0.5);
    return answer.noul >= t ? "yes" : "no";
  }
  if (answer.type === "choice") return answer.choice;
  if (node.type === "score" && answer.type === "score") return answer.score >= node.levels.indexOf(node.at_least) ? "at_least" : "below";
  return portsOf(node)[0]!;
}

const HOST_SAMPLE: Record<string, unknown> = {
  "session.agent": "build",
  "session.id": "01JA7Q2…",
  "session.model": "omniroute/cc/claude-opus-5-5",
  "conversation.last_captain": "Swap the toml parser, keep Config::load(p).",
  "intent.record": "Incremental TOML parser, public API unchanged.",
  "workspace.branch": "feat/incr-parser",
  "workspace.dirty_files": ["src/config.rs", "src/parser/incr.rs"],
};

interface Walk {
  nodes: RunNode[];
  batches: RunRecord["batches"];
  actions: ProposedAction[];
  failure: { kind: string; message: string } | null;
  missing: string[];
  changed: string[];
}

/**
 * Walk the definition the way the engine schedules it (topological order, OR activation, each node
 * at most once). `answerFor` supplies decisions: invented for a sample, recorded for a replay.
 */
function walk(def: Definition, payload: Record<string, unknown>, workspaceRoot: string, answerFor: (nodeId: string, state: string) => { answer: Answer; ms: number } | "missing" | "changed"): Walk {
  const wires = wiresOf(def);
  const ran = new Map<string, RunNode>();
  const out: Walk = { nodes: [], batches: [], actions: [], failure: null, missing: [], changed: [] };
  const host: Record<string, unknown> = { ...HOST_SAMPLE, "workspace.root": workspaceRoot, "session.workspace": workspaceRoot };
  const pseudoRun = (): RunRecord => ({
    nodes: [...ran.values()],
    event: { trigger: def.flow.on, payload },
    inputs: Object.fromEntries(Object.entries(host).map(([k, v]) => [k, { value: v, cut: false }])),
  }) as unknown as RunRecord;
  const scopeOf = (inputs: Record<string, string>) => {
    const scope: Record<string, number | string | boolean | undefined> = {};
    for (const [alias, ref] of Object.entries(inputs)) {
      const [ns, ...rest] = ref.split(".");
      let v: unknown;
      if (ns === "event") v = payload[rest.join(".")];
      else if (ns === "constant") v = def.constants[rest.join(".")];
      else if (ns === "answer" || ns === "result" || ns === "variable") {
        const n = ran.get(rest[0]!);
        if (n?.status === "completed") {
          if (ns === "result") v = n.result ?? undefined;
          else if (ns === "variable") v = n.variables[rest[1] ?? ""];
          else if (n.answer) v = rest[1] === "confidence" && "confidence" in n.answer ? n.answer.confidence : n.answer.type === "noul" ? n.answer.noul : n.answer.type === "choice" ? n.answer.choice : n.answer.score;
        }
      } else v = host[ref];
      scope[alias] = typeof v === "object" && v !== null ? JSON.stringify(v) : (v as number | string | boolean | undefined);
    }
    return scope;
  };

  let batch = 0;
  for (const id of topoOrder(def)) {
    const node = def.node.find((n) => n.id === id)!;
    const active = wires.some((w) => w.target === id && (w.source === TRIGGER_ID || (ran.get(w.source)?.status === "completed" && ran.get(w.source)?.port === w.port)));
    const base: RunNode = { id, status: "not_reached", port: null, duration_ms: null, answer: null, result: null, variables: {}, batch_id: null, error: null };
    if (!active || out.failure) {
      ran.set(id, base);
      continue;
    }
    if (isDecision(node)) {
      const state = renderState(node, def, payload, pseudoRun());
      const got = answerFor(id, state);
      if (got === "missing" || got === "changed") {
        (got === "missing" ? out.missing : out.changed).push(id);
        ran.set(id, base);
        out.failure = { kind: "replay", message: `${id}: no comparable capture` };
        continue;
      }
      const bid = `b${++batch}`;
      out.batches.push({ id: bid, node_ids: [id], state, questions: { [id]: questionOf(node) }, duration_ms: got.ms, usage: { input_tokens: Math.round(state.length / 4) + 120, output_tokens: 14 }, cost_usd: null });
      ran.set(id, { ...base, status: "completed", port: portOf(node, got.answer, def), duration_ms: got.ms, answer: got.answer, batch_id: bid });
    } else if (node.type === "expression") {
      try {
        const scope = scopeOf(node.inputs);
        const variables: Record<string, unknown> = {};
        for (const name of Object.keys(node.assign).sort()) {
          const v = evaluate(node.assign[name]!, scope);
          scope[name] = v;
          variables[name] = v;
        }
        const result = !!evaluate(node.condition, scope);
        ran.set(id, { ...base, status: "completed", port: result ? "true" : "false", duration_ms: 0, result, variables });
      } catch (e) {
        ran.set(id, { ...base, status: "failed", error: e instanceof Error ? e.message : String(e) });
        out.failure = { kind: "expression", message: `${id}: ${e instanceof Error ? e.message : String(e)}` };
      }
    } else {
      const scope = scopeOf(node.inputs);
      const params = Object.fromEntries(
        Object.entries(node.params).map(([k, v]) => [
          k,
          typeof v === "object" ? scope[v.ref] ?? null : typeof v === "string" ? v.replace(/\{(\w+)\}/g, (_, a: string) => (scope[a] === undefined ? "—" : String(scope[a]))) : v,
        ]),
      );
      out.actions.push({ node_id: id, action: node.action, params });
      ran.set(id, { ...base, status: "completed", port: (node.next.then ?? []).length ? "then" : null, duration_ms: 0 });
    }
  }
  out.nodes = def.node.map((n) => ran.get(n.id)!);
  return out;
}

function questionOf(node: Definition["node"][number]): unknown {
  if (node.type === "noul") return { type: "noul", instructions: node.instructions };
  if (node.type === "choice") return { type: "choice", instructions: node.instructions, criteria: node.criteria };
  if (node.type === "score") return { type: "score", instructions: node.instructions, criteria: node.levels };
  return null;
}

function record(def: Definition, payload: Record<string, unknown>, w: Walk, mode: RunRecord["mode"], skipped: boolean): RunRecord {
  const usage = w.batches.reduce((u, b) => ({ input_tokens: u.input_tokens + (b.usage?.input_tokens ?? 0), output_tokens: u.output_tokens + (b.usage?.output_tokens ?? 0) }), { input_tokens: 0, output_tokens: 0 });
  const duration = Math.max(0, ...w.batches.map((b) => b.duration_ms)) + w.batches.length * 9;
  const failed = !!w.failure;
  const fb = def.fallback?.on_failure ?? (def.flow.blocking ? CATALOGUE.triggers.find((t) => t.name === def.flow.on)?.default_fallback ?? null : null);
  return {
    version: 1,
    run_id: ulid(),
    flow_name: def.flow.name,
    definition_digest: `sha256:${hash(JSON.stringify(def))}`,
    mode,
    started_at: new Date().toISOString(),
    duration_ms: duration,
    budget_ms: def.flow.blocking ? def.flow.budget_ms : null,
    event: { trigger: def.flow.on, payload },
    identity: { model: "typesafe/jev-1.13", variant: null },
    inputs: {},
    nodes: w.nodes,
    batches: w.batches,
    status: skipped ? "skipped" : failed ? "failure" : "success",
    failure: w.failure,
    actions: failed ? [] : w.actions,
    fallback_actions: failed && fb ? [{ node_id: null, action: fb.action, params: Object.fromEntries(Object.entries(fb.params).map(([k, v]) => [k, typeof v === "string" ? v.replace("{failure_message}", w.failure!.message) : v])) }] : [],
    usage: w.batches.length ? usage : null,
    cost_usd: null,
    application: { status: "not_applied", actions: [] },
  };
}

function whenHolds(def: Definition, payload: Record<string, unknown>): boolean {
  if (!def.flow.when) return true;
  const scope: Record<string, number | string | boolean | undefined> = {};
  for (const [alias, ref] of Object.entries(def.flow.inputs)) {
    const v = ref.startsWith("event.") ? payload[ref.slice(6)] : ref.startsWith("constant.") ? def.constants[ref.slice(9)] : HOST_SAMPLE[ref];
    scope[alias] = typeof v === "object" && v !== null ? JSON.stringify(v) : (v as number | string | boolean | undefined);
  }
  try {
    return !!evaluate(def.flow.when, scope);
  } catch {
    return false;
  }
}

function simulate(def: Definition, payload: Record<string, unknown>, mode: RunRecord["mode"]): RunRecord {
  if (!whenHolds(def, payload)) {
    const empty: Walk = { nodes: def.node.map((n) => ({ id: n.id, status: "not_reached", port: null, duration_ms: null, answer: null, result: null, variables: {}, batch_id: null, error: null })), batches: [], actions: [], failure: null, missing: [], changed: [] };
    return record(def, payload, empty, mode, true);
  }
  const w = walk(def, payload, "/repo", (id, state) => {
    const node = def.node.find((n) => n.id === id)!;
    return { answer: inventAnswer(node, state)!, ms: 180 + Math.round(unit(state + id) * 220) };
  });
  return record(def, payload, w, mode, false);
}

function replay(def: Definition, recorded: RunRecord): ReplayResult {
  const capture = new Map<string, { answer: Answer; question: string; ms: number }>();
  for (const n of recorded.nodes) {
    if (!n.answer) continue;
    const b = recorded.batches.find((x) => x.id === n.batch_id);
    capture.set(n.id, { answer: n.answer, question: JSON.stringify(b?.questions[n.id] ?? null), ms: n.duration_ms ?? 0 });
  }
  const w = walk(def, recorded.event.payload, "/repo", (id) => {
    const c = capture.get(id);
    if (!c) return "missing";
    const node = def.node.find((n) => n.id === id)!;
    if (JSON.stringify(questionOf(node)) !== c.question) return "changed";
    return { answer: c.answer, ms: c.ms };
  });
  const comparable = !w.missing.length && !w.changed.length;
  return {
    comparable,
    nodes: def.node.map((n) => ({
      id: n.id,
      status: w.missing.includes(n.id) ? "missing_capture" : w.changed.includes(n.id) ? "question_changed" : w.nodes.find((x) => x.id === n.id)?.status === "completed" ? "comparable" : "not_reached",
    })),
    outcome: comparable ? { ...record(def, recorded.event.payload, w, "replay", false), batches: w.batches.map((b) => ({ ...b, usage: null })), usage: null } : null,
  };
}
