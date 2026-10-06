// Bearer-token HTTP access to `/v1/*`.
//
// `navai web` opens `http://127.0.0.1:<port>/#token=<token>`. The token is read
// from the hash once, kept in sessionStorage (so a reload in the same tab keeps
// working, a new tab does not inherit it) and stripped from the address bar so
// it never lands in history or a screenshot. In dev there is no token.

const TOKEN_KEY = "navai.token";

export function adoptTokenFromHash(): void {
  // The server's token is two lowercase ULIDs (Crockford base32), not hex:
  // take everything up to the next `&` rather than guessing an alphabet.
  const match = /(?:^#|&)token=([^&]+)/.exec(window.location.hash);
  if (!match?.[1]) return;
  sessionStorage.setItem(TOKEN_KEY, decodeURIComponent(match[1]));
  history.replaceState(null, "", window.location.pathname + window.location.search);
}

export function token(): string | null {
  return sessionStorage.getItem(TOKEN_KEY);
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
  ) {
    super(message);
  }
}

/** JSON request; a non-2xx answer becomes an HttpError carrying the server's `error`. */
export async function request<T>(path: string, init: RequestInit = {}): Promise<T> {
  const headers = authHeaders(init.headers);
  if (init.body !== undefined) headers.set("content-type", "application/json");
  let response: Response;
  try {
    response = await fetch(path, { ...init, headers });
  } catch (error) {
    throw new HttpError(0, `The Navai server is unreachable (${error instanceof Error ? error.message : String(error)}).`);
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
    throw new HttpError(response.status, detail || `HTTP ${response.status}`, typeof body?.reason === "string" ? body.reason : undefined);
  }
  return body as T;
}

export const get = <T>(path: string) => request<T>(path);
export const post = <T>(path: string, body?: unknown) =>
  request<T>(path, { method: "POST", body: body === undefined ? undefined : JSON.stringify(body) });

export const q = (value: string) => encodeURIComponent(value);
