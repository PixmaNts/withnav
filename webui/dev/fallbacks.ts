// Dev-only stand-ins for `GET /v1/workspaces`, `GET /v1/intent` and the settings
// routes (`settingsFallback.ts`), used while
// the installed server does not have them yet. Each request goes to the real
// server first; only a 404 falls back to the stand-in, so a server that has the
// route is always the one that answers. Data comes from disk (git, intent.md),
// never invented.

import { execFile } from "node:child_process";
import { readFile } from "node:fs/promises";
import { basename, join } from "node:path";
import { promisify } from "node:util";
import type { Connect } from "vite";
import { settingsFallback } from "./settingsFallback.ts";

const run = promisify(execFile);

async function git(cwd: string, args: string[]): Promise<string | null> {
  try {
    const { stdout } = await run("git", args, { cwd, timeout: 5000 });
    return stdout;
  } catch {
    return null;
  }
}

async function workspace(target: string, path: string) {
  const sessions = await fetch(`${target}/v1/sessions?workspace=${encodeURIComponent(path)}`)
    .then((r) => (r.ok ? r.json() : { sessions: [] }))
    .catch(() => ({ sessions: [] }));
  const rows: { updated_at?: string; parent_session_id?: string }[] = sessions.sessions ?? [];
  const roots = rows.filter((s) => !s.parent_session_id);
  const branch = (await git(path, ["rev-parse", "--abbrev-ref", "HEAD"]))?.trim() ?? null;
  const status = await git(path, ["status", "--porcelain"]);
  return {
    path,
    name: basename(path),
    launched: true,
    session_count: roots.length,
    updated_at: roots[0]?.updated_at ?? null,
    branch: branch && branch !== "HEAD" ? branch : null,
    dirty: status === null ? null : status.split("\n").filter(Boolean).length,
  };
}

function parseIntent(raw: string) {
  const sections: Record<string, string[] | undefined> = {};
  let current: string | null = null;
  for (const line of raw.split("\n")) {
    const heading = /^## (.+)$/.exec(line)?.[1]?.trim();
    if (heading !== undefined) {
      current = ["Intent", "Plan", "Decisions", "Active spec"].includes(heading) ? heading : null;
      continue;
    }
    if (current) (sections[current] ??= []).push(line);
  }
  const text = (key: string) => sections[key]?.join("\n").trim() || null;
  return { exists: true, intent: text("Intent"), plan: text("Plan"), decisions: text("Decisions"), active_spec: text("Active spec"), raw: raw.trim() };
}

export function fallbacks(target: string, workspaces: string[]): Connect.NextHandleFunction {
  return async (req, res, next) => {
    const url = new URL(req.url ?? "/", "http://dev");
    if (url.pathname === "/v1/settings" || /^\/v1\/gateways\/[^/]+\/test$/.test(url.pathname)) {
      // Same rule as below: the real server answers unless it does not know the route.
      const chunks: Buffer[] = [];
      for await (const c of req) chunks.push(c as Buffer);
      const payload = Buffer.concat(chunks);
      const real = await fetch(`${target}${req.url}`, {
        method: req.method,
        headers: { authorization: req.headers.authorization ?? "", "content-type": "application/json" },
        body: req.method === "GET" || req.method === "HEAD" ? undefined : payload,
      }).catch(() => null);
      if (real && real.status !== 404) {
        res.statusCode = real.status;
        res.setHeader("content-type", real.headers.get("content-type") ?? "application/json");
        res.end(Buffer.from(await real.arrayBuffer()));
        return;
      }
      // The stand-in reads the body again: hand it a replayable request.
      const { Readable } = await import("node:stream");
      const replay = Object.assign(Readable.from([payload]), { headers: req.headers, method: req.method });
      if (await settingsFallback(target, replay as never, res, url)) return;
      return next();
    }
    const route = url.pathname === "/v1/workspaces" ? "workspaces" : url.pathname === "/v1/intent" ? "intent" : null;
    if (!route || req.method !== "GET") return next();
    const real = await fetch(`${target}${req.url}`, { headers: { authorization: req.headers.authorization ?? "" } }).catch(() => null);
    if (real && real.status !== 404) {
      res.statusCode = real.status;
      res.setHeader("content-type", real.headers.get("content-type") ?? "application/json");
      res.end(Buffer.from(await real.arrayBuffer()));
      return;
    }
    res.setHeader("content-type", "application/json");
    res.setHeader("x-navai-dev-fallback", route);
    if (route === "workspaces") {
      res.end(JSON.stringify({ workspaces: await Promise.all(workspaces.map((path) => workspace(target, path))) }));
      return;
    }
    const ws = url.searchParams.get("workspace");
    if (!ws) {
      res.statusCode = 400;
      res.end(JSON.stringify({ error: "missing query parameter: workspace" }));
      return;
    }
    const raw = await readFile(join(ws, ".navigator", "intent.md"), "utf8").catch(() => null);
    res.end(JSON.stringify(raw === null ? { exists: false, intent: null, plan: null, decisions: null, active_spec: null, raw: null } : parseIntent(raw)));
  };
}
