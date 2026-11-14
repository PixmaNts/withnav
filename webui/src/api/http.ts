// Bearer-token HTTP access to `/v1/*`.
//
// `nav web` opens `http://127.0.0.1:<port>/#pair=<code>`. The single-use pairing
// code is read from the hash, stripped from the address bar immediately with
// `history.replaceState`, and exchanged via `POST /v1/pair/exchange` for an
// expiring browser credential. The credential is kept in sessionStorage (so a
// reload in the same tab keeps working, a new tab does not inherit it). In dev
// there is no token.

import { migratedValue } from "../lib/storage";
const TOKEN_KEY = "withnav.token";

export async function pairFromHash(): Promise<void> {
  const match = /(?:^#|&)pair=([^&]+)/.exec(window.location.hash);
  if (!match?.[1]) return;
  history.replaceState(null, "", window.location.pathname + window.location.search);
  const code = decodeURIComponent(match[1]);
  try {
    const response = await fetch("/v1/pair/exchange", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ code }),
    });
    if (response.ok) {
      const data = await response.json();
      if (typeof data?.credential === "string") {
        sessionStorage.setItem(TOKEN_KEY, data.credential);
      }
    } else {
      reportUnauthorized();
    }
  } catch {
    // Unreachable server will be reported on first API query
  }
}

export const adoptTokenFromHash = pairFromHash;

export function token(): string | null {
  return migratedValue(sessionStorage, TOKEN_KEY, "navai.token");
}

/** Every listener is told once when the server answers 401. */
const unauthorizedListeners = new Set<() => void>();
export function onUnauthorized(listener: () => void): () => void {
  unauthorizedListeners.add(listener);
  return () => unauthorizedListeners.delete(listener);
}
export function reportUnauthorized(): void {
  for (const listener of unauthorizedListeners) listener();
}

export function authHeaders(extra?: HeadersInit): Headers {
  const headers = new Headers(extra);
  const t = token();
  if (t) headers.set("authorization", `Bearer ${t}`);
  return headers;
}

export class HttpError extends Error {
  constructor(
    readonly status: number,
    message: string,
    readonly reason?: string,
    /** The parsed JSON error body, for routes that answer with more than `error` and `reason`. */
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    readonly body?: any,
  ) {
    super(message);
  }
}

/** JSON request; a non-2xx answer becomes an HttpError carrying the server's `error`. */
export async function request<T>(path: string, init: RequestInit = {}): Promise<T> {
  const headers = authHeaders(init.headers);
  if (init.body !== undefined && !headers.has("content-type")) headers.set("content-type", "application/json");
  let response: Response;
  try {
    response = await fetch(path, { ...init, headers });
  } catch (error) {
    throw new HttpError(0, `The WithNav server is unreachable (${error instanceof Error ? error.message : String(error)}).`);
  }
  if (response.status === 401) {
    reportUnauthorized();
    throw new HttpError(401, "Session expired");
  }
  if (response.status === 204) return undefined as T;
  const text = await response.text();
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let body: any;
  try {
    body = text ? JSON.parse(text) : undefined;
  } catch {
    body = undefined;
  }
  if (!response.ok) {
    const detail = typeof body?.error === "string" ? body.error : typeof body?.reason === "string" ? body.reason : text.trim();
    throw new HttpError(response.status, detail || `HTTP ${response.status}`, typeof body?.reason === "string" ? body.reason : undefined, body);
  }
  return body as T;
}

/** Raw bytes of an authenticated route; `<img src>` cannot carry the bearer token. */
export async function blob(path: string): Promise<Blob> {
  let response: Response;
  try {
    response = await fetch(path, { headers: authHeaders() });
  } catch (error) {
    throw new HttpError(0, `The WithNav server is unreachable (${error instanceof Error ? error.message : String(error)}).`);
  }
  if (response.status === 401) {
    reportUnauthorized();
    throw new HttpError(401, "Session expired");
  }
  if (!response.ok) throw new HttpError(response.status, `HTTP ${response.status}`);
  return response.blob();
}

export const get = <T>(path: string) => request<T>(path);
export const post = <T>(path: string, body?: unknown) =>
  request<T>(path, { method: "POST", body: body === undefined ? undefined : JSON.stringify(body) });

export const put = <T>(path: string, body: unknown) => request<T>(path, { method: "PUT", body: JSON.stringify(body) });
export const del = <T>(path: string) => request<T>(path, { method: "DELETE" });
export const q = (value: string) => encodeURIComponent(value);
