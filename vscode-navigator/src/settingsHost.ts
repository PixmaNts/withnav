import type {
  AgentRow,
  CatalogModel,
  GatewayTest,
  HostToSettings,
  LayerRow,
  SettingsBody,
  SettingsSnapshot,
  SettingsToHost,
} from "./settingsProtocol";
import { validateSettingValue } from "./settingsProtocol";

/** The authenticated HTTP access to the session server; the token never leaves the host. */
export interface SettingsTransport {
  request(path: string, init?: RequestInit): Promise<Response>;
}

export interface SettingsHostOptions {
  /** Absolute path of the workspace whose catalogue and agents apply. */
  workspace?: string;
  home?: string;
  /** Finds the `.md` file that defines an agent, if any (a built-in agent has none). */
  findDefinition?: (dirs: string[], id: string) => string | undefined;
  /** Called after each successful write, e.g. to flash the status bar. */
  onSaved?: (key: string) => void;
}

/**
 * Talks to `/v1/settings` for one settings page. Saves go out one at a time,
 * in order, each carrying the revision of the body last read, like the
 * VS Code settings editor writes on every change. A 409 means config.json
 * changed elsewhere: the page reloads it and drops the changes still queued
 * rather than replaying them over a file the user has not seen. A 400 stays
 * attached to the key it refused.
 */
export class SettingsHost {
  private snapshot?: SettingsSnapshot;
  private queue: Promise<void> = Promise.resolve();
  /** Bumped on a 409: saves queued before it are dropped. */
  private epoch = 0;
  private disposed = false;

  constructor(
    private readonly transport: SettingsTransport,
    private readonly post: (message: HostToSettings) => void,
    private readonly options: SettingsHostOptions = {},
  ) {}

  dispose(): void {
    this.disposed = true;
  }

  current(): SettingsSnapshot | undefined {
    return this.snapshot;
  }

  /** Handles the messages that need the server; returns once the work is done. */
  handle(message: SettingsToHost): Promise<void> {
    if (this.disposed) return Promise.resolve();
    if (!this.options.workspace) {
      this.send({ type: "noWorkspace" });
      return Promise.resolve();
    }
    switch (message.type) {
      case "ready":
      case "reload":
        return this.enqueue(() => this.load());
      case "save": {
        const error = validateSettingValue(message.key, message.value);
        if (error) {
          this.send({ type: "invalid", key: message.key, message: error });
          return Promise.resolve();
        }
        const epoch = this.epoch;
        this.send({ type: "saving", key: message.key });
        return this.enqueue(() => (epoch === this.epoch ? this.save(message.key, message.value) : Promise.resolve()));
      }
      case "testGateway":
        return this.test(message.name);
      default:
        return Promise.resolve();
    }
  }

  private enqueue(work: () => Promise<void>): Promise<void> {
    const next = this.queue.then(work, work);
    this.queue = next.catch(() => undefined);
    return next;
  }

  private send(message: HostToSettings): void {
    if (!this.disposed) this.post(message);
  }

  private query(): string {
    return this.options.workspace ? `?workspace=${encodeURIComponent(this.options.workspace)}` : "";
  }

  private async getJson(path: string): Promise<any> {
    const response = await this.transport.request(path + this.query(), { method: "GET" });
    if (!response.ok) throw new Error(`${path} answered ${response.status}${await errorDetail(response)}`);
    return response.json();
  }

  private async load(): Promise<void> {
    try {
      const settings = sanitizeSettings(await this.getJson("/v1/settings"));
      this.snapshot = {
        settings,
        models: [],
        agents: [],
        layers: [],
        notes: {},
        home: this.options.home,
      };
      this.send({ type: "snapshot", data: this.snapshot });
      await Promise.all([this.loadProjection("models"), this.loadProjection("agents"), this.loadProjection("layers")]);
    } catch (error) {
      this.send({ type: "loadError", message: `Cannot read the Navai settings: ${errorText(error)}` });
    }
  }

  /** Agent assignments and layer status are projections of config.json: re-read them after a write. */
  private async refreshProjections(): Promise<void> {
    await Promise.all([this.loadProjection("agents"), this.loadProjection("layers")]);
  }

  private async loadProjection(kind: "models" | "agents" | "layers"): Promise<void> {
    const snapshot = this.snapshot;
    if (!snapshot || this.disposed) return;
    const notes = snapshot.notes ??= {};
    try {
      const body = await this.getJson(`/v1/${kind}`);
      if (this.disposed || snapshot !== this.snapshot) return;
      if (kind === "models") snapshot.models = parseModels(body);
      else if (kind === "agents") snapshot.agents = this.parseAgents(body, snapshot.settings);
      else snapshot.layers = parseLayers(body);
      delete notes[kind];
    } catch (error) {
      notes[kind] = `Cannot read ${kind}: ${errorText(error)}`;
    }
    this.send({ type: "snapshot", data: snapshot });
  }

  private async save(key: string, value: unknown): Promise<void> {
    if (this.disposed) return;
    const snapshot = this.snapshot;
    if (!snapshot) {
      this.send({ type: "saveFailed", key, message: "The settings are not loaded yet." });
      return;
    }
    let response: Response;
    try {
      response = await this.transport.request(`/v1/settings${this.query()}`, {
        method: "PUT",
        body: JSON.stringify({ revision: snapshot.settings.revision, key, value }),
      });
    } catch (error) {
      this.send({ type: "saveFailed", key, message: errorText(error) });
      return;
    }
    const body = await response.json().catch(() => undefined);
    if (response.status === 409) {
      this.epoch += 1;
      await this.load();
      this.send({ type: "stale", key, message: "config.json changed elsewhere. The page was reloaded; your last change was not saved." });
      return;
    }
    if (response.status === 400) {
      const refused = typeof body?.key === "string" && body.key ? body.key : key;
      this.send({ type: "invalid", key: refused, message: typeof body?.error === "string" ? body.error : "Invalid value." });
      return;
    }
    if (!response.ok || !body) {
      const detail = typeof body?.error === "string" ? body.error : `the server answered ${response.status}`;
      this.send({ type: "saveFailed", key, message: `Not saved: ${detail}` });
      return;
    }
    snapshot.settings = sanitizeSettings(body as SettingsBody);
    await this.refreshProjections();
    this.send({ type: "snapshot", data: snapshot });
    this.send({ type: "saved", key });
    this.options.onSaved?.(key);
  }

  private async test(name: string): Promise<void> {
    if (!this.snapshot?.settings.gateways.some((g) => g.name === name)) return;
    this.send({ type: "testing", name });
    let result: GatewayTest;
    try {
      const response = await this.transport.request(`/v1/gateways/${encodeURIComponent(name)}/test${this.query()}`, { method: "POST" });
      const body = await response.json().catch(() => undefined);
      result = response.ok && body && typeof body.ok === "boolean"
        ? { ok: body.ok, status: body.status ?? null, latency_ms: Number(body.latency_ms) || 0, model_count: body.model_count ?? null, error: body.error ?? null }
        : { ok: false, status: response.status, latency_ms: 0, model_count: null, error: body?.error ?? `the server answered ${response.status}` };
    } catch (error) {
      result = { ok: false, status: null, latency_ms: 0, model_count: null, error: errorText(error) };
    }
    this.send({ type: "testResult", name, result });
  }

  private parseAgents(body: any, settings: SettingsBody): AgentRow[] {
    const agents = Array.isArray(body?.agents) ? body.agents : [];
    const dirs = settings.paths?.agents ?? [];
    return agents
      .filter((a: any) => a && typeof a.id === "string")
      .map((a: any) => ({
        id: a.id,
        description: typeof a.description === "string" ? a.description : "",
        mode: typeof a.mode === "string" ? a.mode : "primary",
        effective: a.effective && typeof a.effective.model === "string"
          ? { model: a.effective.model, variant: typeof a.effective.variant === "string" ? a.effective.variant : null, source: String(a.effective.source ?? "") }
          : null,
        warning: typeof a.warning === "string" ? a.warning : null,
        definition: this.options.findDefinition?.(dirs, a.id),
      }));
  }
}

/** Redact gateway URL secrets even when an older server returns an unfiltered URL. */
export function sanitizeSettings(settings: SettingsBody): SettingsBody {
  return { ...settings, gateways: settings.gateways.map((gateway) => {
    let base_url = "(invalid URL)";
    try {
      const url = new URL(gateway.base_url);
      if (url.protocol !== "http:" && url.protocol !== "https:") throw new Error("Invalid gateway URL");
      url.username = "";
      url.password = "";
      url.search = "";
      url.hash = "";
      base_url = url.toString();
    } catch { /* Invalid URLs must not reveal unparsed credentials. */ }
    return { ...gateway, base_url };
  }) };
}

function parseModels(body: any): CatalogModel[] {
  const models = Array.isArray(body?.models) ? body.models : [];
  return models
    .filter((m: any) => m && typeof m.name === "string" && typeof m.gateway === "string" && typeof m.api === "string")
    .map((m: any) => ({
      name: m.name,
      gateway: m.gateway,
      provider_group: typeof m.provider_group === "string" ? m.provider_group : m.gateway,
      upstream_id: typeof m.upstream_id === "string" ? m.upstream_id : m.name,
      api: m.api,
      enabled: m.enabled,
      context_window: typeof m.context_window === "number" ? m.context_window : undefined,
      reported_output: typeof m.reported_output === "number" ? m.reported_output : undefined,
      variants: Array.isArray(m.variants) ? m.variants.filter((v: unknown) => typeof v === "string") : [],
      default_variant: typeof m.default_variant === "string" ? m.default_variant : undefined,
    }));
}

function parseLayers(body: any): LayerRow[] {
  const layers = Array.isArray(body?.layers) ? body.layers : [];
  return layers
    .filter((l: any) => l && typeof l.layer === "string")
    .map((l: any) => ({ layer: l.layer, status: String(l.status ?? ""), model: l.model ?? null, variant: l.variant ?? null, reason: l.reason ?? null }));
}

async function errorDetail(response: Response): Promise<string> {
  try {
    const body = await response.json();
    return typeof body?.error === "string" ? `: ${body.error}` : "";
  } catch {
    return "";
  }
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
