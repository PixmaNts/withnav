// Dev-only stand-in for `GET/PUT /v1/settings` and `POST /v1/gateways/{name}/test`
// (`.scratch/settings/contract.md`), used only while the server answers 404.
//
// It starts from the real `config.json` and `models.json` of `NAVAI_CONFIG_DIR`
// (default `~/.config/navai`), keeps every change in memory and never writes a
// file. Catalogue checks use the real server's `GET /v1/models` and
// `GET /v1/agents`, so a model or agent that does not exist is refused as the
// server would. `auth.json` is only checked for a gateway name, never read for
// a key. The gateway test lists the gateway's models with its real key.

import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import type { IncomingMessage, ServerResponse } from "node:http";
import { homedir } from "node:os";
import { join } from "node:path";

/* eslint-disable @typescript-eslint/no-explicit-any */
type Json = any;

const configDir = process.env.NAVAI_CONFIG_DIR ?? join(homedir(), ".config", "navai");
const THEMES = ["ayu-dark", "ayu-light", "nord-dark", "nord-light"];
const LAYER_API: Record<string, string> = { reflex: "fim", instinct: "systemone", metacognition: "chat" };
const MAX_SECS = 31_536_000;

let config: Json | null = null;
let revisionCounter = 0;

async function readJson(path: string): Promise<Json | null> {
  try {
    return JSON.parse(await readFile(path, "utf8"));
  } catch {
    return null;
  }
}

async function loadConfig(): Promise<Json> {
  if (config === null) config = (await readJson(join(configDir, "config.json"))) ?? {};
  return config;
}

function revision(cfg: Json): string {
  return `sha256:${createHash("sha256").update(JSON.stringify(cfg)).update(String(revisionCounter)).digest("hex")}`;
}

function getPath(obj: Json, key: string): Json {
  return key.split(".").reduce((o, k) => (o && typeof o === "object" ? o[k] : undefined), obj);
}

function setPath(obj: Json, key: string, value: Json) {
  const parts = key.split(".");
  let o = obj;
  for (const p of parts.slice(0, -1)) o = o[p] && typeof o[p] === "object" ? o[p] : (o[p] = {});
  const last = parts[parts.length - 1]!;
  if (value === null) delete o[last];
  else o[last] = value;
}

/** `{model, variant}` as the contract returns it, from the string or object config.json holds. */
function asChoice(raw: Json): Json {
  if (typeof raw === "string") return { model: raw, variant: null };
  if (raw && typeof raw === "object" && typeof raw.model === "string") return { model: raw.model, variant: raw.variant ?? null };
  return raw ?? null;
}

async function upstream(target: string, path: string, auth: string): Promise<Json | null> {
  try {
    const r = await fetch(`${target}${path}`, { headers: { authorization: auth } });
    return r.ok ? r.json() : null;
  } catch {
    return null;
  }
}

async function body(target: string, workspace: string, auth: string) {
  const cfg = await loadConfig();
  const models = (await readJson(join(configDir, "models.json"))) ?? {};
  const auths = (await readJson(join(configDir, "auth.json"))) ?? {};
  const agents: Json = (await upstream(target, `/v1/agents?workspace=${encodeURIComponent(workspace)}`, auth))?.agents ?? [];
  const field = (key: string, def: Json, applies: string, map = (v: Json) => v) => {
    const raw = getPath(cfg, key);
    return { key, value: raw === undefined ? null : map(raw), default: def, set: raw !== undefined, error: null, applies };
  };
  const fields = [
    // As the real server: the layers are built once, at start.
    field("layers.reflex", null, "restart", asChoice),
    field("layers.instinct", null, "restart", asChoice),
    field("layers.metacognition", null, "restart", asChoice),
    field("default_agent", "build", "new_sessions"),
    ...agents.map((a: Json) => field(`agents.${a.id}`, null, "new_sessions", asChoice)),
    field("permissions.automatic_review", true, "new_sessions"),
    field("permissions.scratch_dirs", [`${process.env.XDG_RUNTIME_DIR ?? "/run/user/1000"}/navai`, "/tmp/navai"], "new_sessions"),
    field("subagents.max_concurrent", 4, "new_sessions"),
    field("subagents.watchdog_interval_secs", 300, "new_sessions"),
    field("agent.watchdog_interval_secs", 300, "new_sessions"),
    field("tui.theme", null, "live"),
  ];
  const used = (gateway: string) =>
    fields.filter((f) => (f.key.startsWith("layers.") || f.key.startsWith("agents.")) && f.set && typeof f.value?.model === "string" && f.value.model.startsWith(`${gateway}/`)).map((f) => f.key);
  const gateways = Object.entries(models.providers ?? {}).map(([name, p]: [string, Json]) => ({
    name,
    base_url: p.base_url ?? "",
    api_key_env: p.api_key_env ?? null,
    key_source: auths[name] ? "auth_store" : p.api_key_env && process.env[p.api_key_env] ? "environment" : "missing",
    catalog_format: p.catalog_format ?? null,
    connect_timeout_ms: p.connect_timeout_ms ?? 5000,
    request_timeout_ms: p.request_timeout_ms ?? 600000,
    headers: Object.keys(p.headers ?? {}),
    model_count: Object.values(models.models ?? {}).filter((m: Json) => m.provider === name).length,
    used_by: used(name),
  }));
  return {
    revision: revision(cfg),
    paths: { config: join(configDir, "config.json"), models: join(configDir, "models.json"), agents: [join(configDir, "agents"), join(workspace, ".navai", "agents")] },
    fields,
    gateways,
  };
}

async function validate(target: string, workspace: string, auth: string, key: string, value: Json): Promise<string | null> {
  if (value === null) return null;
  const isInt = (v: Json, lo: number, hi: number) => Number.isInteger(v) && v >= lo && v <= hi;
  if (key === "permissions.automatic_review") return typeof value === "boolean" ? null : `${key} must be a boolean`;
  if (key === "permissions.scratch_dirs")
    return Array.isArray(value) && value.every((p) => typeof p === "string" && p.startsWith("/")) ? null : `${key} must contain absolute paths`;
  if (key === "subagents.max_concurrent") return isInt(value, 1, 64) ? null : `${key} must be an integer from 1 to 64`;
  if (key === "subagents.watchdog_interval_secs" || key === "agent.watchdog_interval_secs") return isInt(value, 0, MAX_SECS) ? null : `${key} must be an integer from 0 to ${MAX_SECS}`;
  if (key === "tui.theme") return THEMES.includes(value) ? null : `unknown theme "${value}"`;
  const agents: Json[] = (await upstream(target, `/v1/agents?workspace=${encodeURIComponent(workspace)}`, auth))?.agents ?? [];
  if (key === "default_agent") {
    const a = agents.find((x) => x.id === value);
    return a && (a.mode === "primary" || a.mode === "all") ? null : `default_agent must be a primary agent; "${value}" is not`;
  }
  const layer = key.startsWith("layers.") ? key.slice(7) : null;
  const agent = key.startsWith("agents.") ? key.slice(7) : null;
  if ((layer && LAYER_API[layer]) || (agent && agents.some((a) => a.id === agent))) {
    if (!value || typeof value.model !== "string") return `${key} must be {"model", "variant"}`;
    const catalog: Json[] = (await upstream(target, `/v1/models?workspace=${encodeURIComponent(workspace)}`, auth))?.models ?? [];
    const m = catalog.find((x) => x.name === value.model);
    if (!m) return `model ${value.model} is not in the catalogue`;
    const api = layer ? LAYER_API[layer] : "chat";
    if (m.api !== api) return `${value.model} serves the ${m.api} API; ${key} needs ${api}`;
    if (value.variant != null && value.variant !== "off" && !(m.variants ?? []).includes(value.variant)) return `${value.model} does not offer the variant "${value.variant}"`;
    return null;
  }
  return `unknown key ${key}`;
}

function send(res: ServerResponse, status: number, value: Json) {
  res.statusCode = status;
  res.setHeader("content-type", "application/json");
  res.setHeader("x-navai-dev-fallback", "settings");
  res.end(JSON.stringify(value));
}

async function readBody(req: IncomingMessage): Promise<Json> {
  const chunks: Buffer[] = [];
  for await (const c of req) chunks.push(c as Buffer);
  try {
    return JSON.parse(Buffer.concat(chunks).toString("utf8"));
  } catch {
    return null;
  }
}

/** True when this request is one of the three settings routes (and was answered). */
export async function settingsFallback(target: string, req: IncomingMessage, res: ServerResponse, url: URL): Promise<boolean> {
  const auth = req.headers.authorization ?? "";
  const workspace = url.searchParams.get("workspace");
  const test = /^\/v1\/gateways\/([^/]+)\/test$/.exec(url.pathname);
  const isSettings = url.pathname === "/v1/settings";
  if (!isSettings && !test) return false;
  if (!workspace) {
    send(res, 400, { error: "missing query parameter: workspace" });
    return true;
  }
  if (isSettings && req.method === "GET") {
    send(res, 200, await body(target, workspace, auth));
    return true;
  }
  if (isSettings && req.method === "PUT") {
    const put = await readBody(req);
    const cfg = await loadConfig();
    if (!put || typeof put.key !== "string" || !("value" in put)) {
      send(res, 400, { error: "expected {revision, key, value}", reason: "invalid", key: put?.key ?? null });
      return true;
    }
    if (put.revision !== revision(cfg)) {
      send(res, 409, { error: "config.json changed since it was read", reason: "stale", revision: revision(cfg) });
      return true;
    }
    const problem = await validate(target, workspace, auth, put.key, put.value);
    if (problem) {
      send(res, 400, { error: problem, reason: "invalid", key: put.key });
      return true;
    }
    // config.json keeps a bare string when no variant is chosen, like the files the TUI writes.
    const stored = put.value && typeof put.value === "object" && "model" in put.value && put.value.variant == null ? put.value.model : put.value;
    setPath(cfg, put.key, stored);
    revisionCounter += 1;
    send(res, 200, await body(target, workspace, auth));
    return true;
  }
  if (test && req.method === "POST") {
    const name = decodeURIComponent(test[1]!);
    const models = (await readJson(join(configDir, "models.json"))) ?? {};
    const p = models.providers?.[name];
    if (!p) {
      send(res, 404, { error: `unknown gateway ${name}` });
      return true;
    }
    const auths = (await readJson(join(configDir, "auth.json"))) ?? {};
    const key: string | undefined = auths[name]?.api_key ?? (p.api_key_env ? process.env[p.api_key_env] : undefined);
    const started = Date.now();
    try {
      const r = await fetch(`${String(p.base_url).replace(/\/$/, "")}/models`, {
        headers: key ? { authorization: `Bearer ${key}` } : {},
        signal: AbortSignal.timeout(p.connect_timeout_ms ?? 5000),
      });
      const latency_ms = Date.now() - started;
      const listed: Json = r.ok ? await r.json().catch(() => null) : null;
      const count = Array.isArray(listed?.data) ? listed.data.length : null;
      send(res, 200, { ok: r.ok, status: r.status, latency_ms, model_count: count, error: r.ok ? null : `HTTP ${r.status}` });
    } catch (error) {
      send(res, 200, { ok: false, status: null, latency_ms: Date.now() - started, model_count: null, error: error instanceof Error ? error.message : String(error) });
    }
    return true;
  }
  return false;
}
