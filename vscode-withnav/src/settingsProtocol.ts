// Contract between the extension host (which talks to the session server's
// `/v1/settings` routes) and the "Navai Settings" webview (which only renders
// and never sees the server token). Both sides import these types and the
// small pure helpers below.

/** When a change takes effect, as the server reports it per key. */
export type Applies = "restart" | "new_sessions" | "live";

/** One key of `config.json`, as `GET /v1/settings` lists it. */
export interface SettingField {
  key: string;
  value: unknown;
  /** What applies when the key is absent (null = nothing). */
  default: unknown;
  /** True when config.json holds the key. */
  set: boolean;
  /** Why the stored value is invalid, if it is. */
  error: string | null;
  applies: Applies;
}

/** A gateway of `models.json` (read-only here). Never carries the key nor header values. */
export interface GatewayInfo {
  name: string;
  base_url: string;
  api_key_env: string | null;
  key_source: "auth_store" | "environment" | "missing";
  catalog_format: string | null;
  connect_timeout_ms: number;
  request_timeout_ms: number;
  headers: string[];
  model_count: number;
  used_by: string[];
}

export interface SettingsBody {
  revision: string;
  paths: { config: string; models: string; agents: string[] };
  fields: SettingField[];
  gateways: GatewayInfo[];
}

/** One catalogue model (`GET /v1/models`). */
export interface CatalogModel {
  name: string;
  gateway: string;
  provider_group: string;
  upstream_id: string;
  api: string;
  enabled?: boolean;
  context_window?: number;
  reported_output?: number;
  variants?: string[];
  default_variant?: string;
}

/** One agent (`GET /v1/agents`), plus the definition file the host found for it. */
export interface AgentRow {
  id: string;
  description: string;
  mode: string;
  effective: { model: string; variant: string | null; source: string } | null;
  warning: string | null;
  /** The `.md` file that defines the agent; absent for a built-in agent. */
  definition?: string;
}

/** One lower layer (`GET /v1/layers`). */
export interface LayerRow {
  layer: string;
  status: string;
  model: string | null;
  variant: string | null;
  reason: string | null;
}

/** Everything the page renders from. */
export interface SettingsSnapshot {
  settings: SettingsBody;
  models: CatalogModel[];
  agents: AgentRow[];
  layers: LayerRow[];
  /** The user's home directory, to shorten paths to `~/…`. */
  home?: string;
  notes?: { models?: string; agents?: string; layers?: string };
}

export interface GatewayTest {
  ok: boolean;
  status: number | null;
  latency_ms: number;
  model_count: number | null;
  error: string | null;
}

export type HostToSettings =
  | { type: "snapshot"; data: SettingsSnapshot; projectionOnly?: boolean }
  | { type: "noWorkspace" }
  /** The settings could not be read (no server yet, server error). */
  | { type: "loadError"; message: string }
  | { type: "saving"; key: string }
  | { type: "saved"; key: string }
  /** The server refused the value (400): shown under that key's row. */
  | { type: "invalid"; key: string; message: string }
  /** config.json changed elsewhere (409): the page was reloaded, the change dropped. */
  | { type: "stale"; key: string; message: string }
  | { type: "saveFailed"; key: string; message: string }
  | { type: "testing"; name: string }
  | { type: "testResult"; name: string; result: GatewayTest };

export type SettingsToHost =
  | { type: "ready" }
  | { type: "reload" }
  | { type: "openFolder" }
  /** Writes one key now; `value: null` removes it from config.json. */
  | { type: "save"; key: string; value: unknown }
  | { type: "open"; target: "config" | "models" }
  | { type: "openAgent"; id: string }
  | { type: "testGateway"; name: string }
  | { type: "openExtensionSettings" };

/**
 * Checks a message from the webview. The webview is treated as untrusted
 * input: anything malformed is dropped rather than forwarded to the server.
 */
export function parseSettingsMessage(raw: unknown): SettingsToHost | null {
  if (!raw || typeof raw !== "object") return null;
  const m = raw as Record<string, unknown>;
  switch (m.type) {
    case "ready":
    case "reload":
    case "openFolder":
    case "openExtensionSettings":
      return { type: m.type };
    case "save":
      return typeof m.key === "string" && m.key && "value" in m ? { type: "save", key: m.key, value: m.value } : null;
    case "open":
      return m.target === "config" || m.target === "models" ? { type: "open", target: m.target } : null;
    case "openAgent":
      return typeof m.id === "string" && m.id ? { type: "openAgent", id: m.id } : null;
    case "testGateway":
      return typeof m.name === "string" && m.name ? { type: "testGateway", name: m.name } : null;
    default:
      return null;
  }
}

/** Wire types are checked before HTTP; semantic catalogue and range checks belong to the server. */
export function validateSettingValue(key: string, value: unknown): string | null {
  if (value === null) return null;
  if (key === "permissions.automatic_review") return typeof value === "boolean" ? null : "Expected a boolean.";
  if (["subagents.max_concurrent", "subagents.watchdog_interval_secs", "agent.watchdog_interval_secs"].includes(key)) {
    return typeof value === "number" && Number.isFinite(value) && Number.isInteger(value) ? null : "Expected a finite integer.";
  }
  if (key === "permissions.scratch_dirs") return Array.isArray(value) && value.every((v) => typeof v === "string") ? null : "Expected an array of strings.";
  if (key.startsWith("layers.") || key.startsWith("agents.")) {
    if (value && typeof value === "object" && !Array.isArray(value)) {
      const choice = value as Record<string, unknown>;
      if (typeof choice.model === "string" && (!Object.hasOwn(choice, "variant") || choice.variant === null || typeof choice.variant === "string")) return null;
    }
    return "Expected {model: string, variant?: string | null}.";
  }
  if (key === "default_agent" || key === "tui.theme") return typeof value === "string" ? null : "Expected a string.";
  return "Unknown setting.";
}

/** The value in force: what config.json holds, else the default. */
export function effectiveValue(field: Pick<SettingField, "set" | "value" | "default"> | undefined): unknown {
  if (!field) return undefined;
  return field.set ? field.value : field.default;
}

/** "Modified" as in the VS Code settings editor: config.json holds a value other than the default. */
export function isModified(field: Pick<SettingField, "set" | "value" | "default"> | undefined): boolean {
  return !!field && field.set && JSON.stringify(field.value) !== JSON.stringify(field.default);
}

export interface ModelChoice {
  model: string;
  /** null = the model's default variant; "off" = no variant. */
  variant: string | null;
}

/** Reads a `{model, variant}` value (or a bare model name); anything else is null. */
export function asChoice(value: unknown): ModelChoice | null {
  if (typeof value === "string" && value) return { model: value, variant: null };
  if (value && typeof value === "object" && typeof (value as { model?: unknown }).model === "string") {
    const v = (value as { variant?: unknown }).variant;
    return { model: (value as { model: string }).model, variant: typeof v === "string" ? v : null };
  }
  return null;
}

/** Keeps a variant across a model change only when the new model offers it. */
export function carryVariant(variant: string | null, next: CatalogModel | undefined): string | null {
  if (!variant) return null;
  if (variant === "off") return next?.variants?.length ? "off" : null;
  return next?.variants?.includes(variant) ? variant : null;
}

/** One honest sentence about when a change takes effect. */
export function appliesText(applies: Applies): string {
  switch (applies) {
    case "restart":
      return "Applies after restarting Navai.";
    case "new_sessions":
      return "Applies to new sessions and new subagents.";
    case "live":
      return "Applies at once here; other open clients on their next start.";
  }
}
