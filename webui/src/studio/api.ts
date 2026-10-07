// Typed client of the flows routes (contracts §5). Every call carries `?workspace=<path>`.
// `studioApi()` picks the real server, or the in-memory mock (`mock/server.ts`) when the page
// asks for it (`?mock=1` in the route) or when the server does not have the routes yet.

import { HttpError, request } from "../api/http";
import { createMockApi } from "./mock/server";
import type {
  Catalogue,
  Content,
  ErrorEnvelope,
  FlowDocument,
  FlowSource,
  FlowSummary,
  Health,
  Location,
  ReloadResult,
  ReplayResult,
  RunRecord,
  RunSummary,
  Trust,
  ValidateResult,
} from "./types";

export interface StudioApi {
  readonly mocked: boolean;
  list(): Promise<{ flows: FlowSummary[] }>;
  get(name: string, location: Location): Promise<FlowDocument>;
  create(body: { name: string; location: "workspace" | "global"; content: Content }): Promise<FlowDocument>;
  save(name: string, body: { location: Location; revision: string; content: Content }): Promise<FlowDocument>;
  remove(name: string, body: { location: Location; revision: string }): Promise<{ deleted: boolean }>;
  validate(content: Content): Promise<ValidateResult>;
  catalogue(): Promise<Catalogue>;
  sample(body: { source: FlowSource; payload: Record<string, unknown>; session_id: string | null }): Promise<RunRecord>;
  runs(name: string, opts?: { limit?: number; before?: string }): Promise<{ runs: RunSummary[]; next_cursor: string | null }>;
  run(name: string, runId: string): Promise<RunRecord>;
  replay(body: { source: FlowSource; run_id: string }): Promise<ReplayResult>;
  reload(): Promise<ReloadResult>;
  health(): Promise<Health>;
  trust(): Promise<Trust>;
  setTrust(trusted: boolean): Promise<Trust>;
}

/** The diagnostics of an error envelope, when the server sent one. */
export function envelopeOf(error: unknown): ErrorEnvelope | null {
  if (error instanceof HttpError && error.body && typeof error.body.error === "string") {
    return { error: error.body.error, reason: String(error.body.reason ?? ""), errors: Array.isArray(error.body.errors) ? error.body.errors : [] };
  }
  return null;
}

export function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function httpStudioApi(workspace: string): StudioApi {
  const ws = `workspace=${encodeURIComponent(workspace)}`;
  const url = (path: string, extra = "") => `/v1/flows${path}?${ws}${extra}`;
  const n = encodeURIComponent;
  const json = (method: string, body: unknown) => ({ method, body: JSON.stringify(body) });
  return {
    mocked: false,
    list: () => request(url("")),
    get: (name, location) => request(url(`/${n(name)}`, `&location=${location}`)),
    create: (body) => request(url(""), json("POST", body)),
    save: (name, body) => request(url(`/${n(name)}`), json("PUT", body)),
    remove: (name, body) => request(url(`/${n(name)}`), json("DELETE", body)),
    validate: (content) => request(url("/validate"), json("POST", { content })),
    catalogue: () => request(url("/catalogue")),
    sample: (body) => request(url("/sample"), json("POST", body)),
    runs: (name, opts = {}) =>
      request(url(`/${n(name)}/runs`, `${opts.limit ? `&limit=${opts.limit}` : ""}${opts.before ? `&before=${n(opts.before)}` : ""}`)),
    run: (name, runId) => request(url(`/${n(name)}/runs/${n(runId)}`)),
    replay: (body) => request(url("/replay"), json("POST", body)),
    reload: () => request(url("/reload"), json("POST", {})),
    health: () => request(url("/health")),
    trust: () => request(url("/trust")),
    setTrust: (trusted) => request(url("/trust"), json("POST", { trusted })),
  };
}

const mocks = new Map<string, StudioApi>();
function mockFor(workspace: string): StudioApi {
  let api = mocks.get(workspace);
  if (!api) mocks.set(workspace, (api = createMockApi(workspace)));
  return api;
}

/**
 * The API the Studio uses for a workspace. The catalogue route is probed once: a 404 or 405 means
 * the server predates the flows routes, and the mock takes over (the page says so).
 */
export async function studioApi(workspace: string, forceMock: boolean): Promise<StudioApi> {
  if (forceMock) return mockFor(workspace);
  const real = httpStudioApi(workspace);
  try {
    await real.catalogue();
    return real;
  } catch (error) {
    if (error instanceof HttpError && (error.status === 404 || error.status === 405)) return mockFor(workspace);
    throw error;
  }
}
