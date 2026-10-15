import type { AgentEntry, ModelEntry, SettingApplies, SettingField, SettingsBody } from "../api/types";
import { THEMES, type ThemeId } from "../theme/themes";
import { shortPath } from "./format";

// Reading `GET /v1/settings` and tracking the immediate saves of the Settings
// page. Pure functions only, so the page and the tests share them.

export const SECTIONS = ["gateways", "layers", "permissions", "subagents", "appearance"] as const;
export type SectionId = (typeof SECTIONS)[number];

export function isSection(value: string | null | undefined): value is SectionId {
  return SECTIONS.includes(value as SectionId);
}

export type LayerId = "reflex" | "instinct" | "metacognition";
export const LAYER_API: Record<LayerId, string> = { reflex: "fim", instinct: "systemone", metacognition: "chat" };

/** A model assignment as config.json stores it: `{model, variant}`, or the bare model string. */
export interface ModelChoice {
  model: string;
  /** null: the model's default variant (key absent); "off": no variant. */
  variant: string | null;
}

export function fieldOf(body: SettingsBody | undefined, key: string): SettingField | undefined {
  return body?.fields.find((f) => f.key === key);
}

/** What applies: the stored value when the key is in config.json, else the default. */
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export function effective(field: SettingField | undefined): any {
  if (!field) return undefined;
  return field.set ? field.value : field.default;
}

export function asChoice(value: unknown): ModelChoice | null {
  if (typeof value === "string" && value) return { model: value, variant: null };
  if (value && typeof value === "object" && typeof (value as { model?: unknown }).model === "string") {
    const v = (value as { variant?: unknown }).variant;
    return { model: (value as { model: string }).model, variant: typeof v === "string" ? v : null };
  }
  return null;
}

export function choiceOf(body: SettingsBody | undefined, key: string): ModelChoice | null {
  return asChoice(effective(fieldOf(body, key)));
}

export function asBool(value: unknown, fallback: boolean): boolean {
  return typeof value === "boolean" ? value : fallback;
}

export function asInt(value: unknown, fallback: number): number {
  return typeof value === "number" && Number.isInteger(value) ? value : fallback;
}

export function asPaths(value: unknown): string[] {
  return Array.isArray(value) ? value.filter((p): p is string => typeof p === "string") : [];
}

export function asTheme(value: unknown): ThemeId | null {
  return THEMES.some((t) => t.id === value) ? (value as ThemeId) : null;
}

/** The theme config.json sets, only when the key is present: the local choice stays the fallback. */
export function serverTheme(body: SettingsBody | undefined): ThemeId | null {
  const field = fieldOf(body, "tui.theme");
  return field?.set ? asTheme(field.value) : null;
}

/** The agent names config.json knows a field for (`agents.<name>`), in server order. */
export function agentKeys(body: SettingsBody | undefined): string[] {
  return (body?.fields ?? []).filter((f) => f.key.startsWith("agents.")).map((f) => f.key.slice("agents.".length));
}

/**
 * The variant to keep when a target moves to another model: the current one when the new model
 * offers it (or it is "off"), otherwise none, so the server never rejects a model change for a
 * variant the new model lacks.
 */
export function carryVariant(variant: string | null, next: ModelEntry | undefined): string | null {
  if (!variant) return null;
  if (variant === "off") return next?.variants?.length ? "off" : null;
  return next?.variants?.includes(variant) ? variant : null;
}

// ---------- the one-line summaries of the sidebar ----------

export const WATCHDOG_PRESETS: { secs: number; label: string }[] = [
  { secs: 0, label: "Off" },
  { secs: 60, label: "1 min" },
  { secs: 300, label: "5 min" },
  { secs: 900, label: "15 min" },
  { secs: 3600, label: "1 h" },
];

export function summaries(body: SettingsBody | undefined): Record<SectionId, string> {
  if (!body) return { gateways: "", layers: "", permissions: "", subagents: "", appearance: "" };
  const layers = (["reflex", "instinct", "metacognition"] as const).filter((l) => choiceOf(body, `layers.${l}`)).map((l) => (l === "metacognition" ? "meta" : l));
  const scratch = asPaths(effective(fieldOf(body, "permissions.scratch_dirs"))).length;
  const max = asInt(effective(fieldOf(body, "subagents.max_concurrent")), 4);
  const theme = asTheme(effective(fieldOf(body, "tui.theme")));
  return {
    gateways: body.gateways.map((g) => g.name).join(" · ") || "none in models.json",
    layers: [...layers, "cognition"].join(" · "),
    permissions: scratch === 0 ? "no scratch dir" : `${scratch} scratch dir${scratch === 1 ? "" : "s"}`,
    subagents: `max ${max} per session`,
    appearance: theme ? THEMES.find((t) => t.id === theme)!.label : "not set",
  };
}

/** The two file lines at the bottom of the sidebar: one folder when both files share it. */
export function fileLines(paths: SettingsBody["paths"] | undefined): string[] {
  if (!paths) return [];
  const dir = (p: string) => p.slice(0, p.lastIndexOf("/") + 1);
  const base = (p: string) => p.slice(p.lastIndexOf("/") + 1);
  if (paths.models && dir(paths.models) === dir(paths.config)) return [shortPath(dir(paths.config)), `${base(paths.config)} · ${base(paths.models)}`];
  return [shortPath(paths.config), ...(paths.models ? [shortPath(paths.models)] : [])];
}

/** `layers.metacognition` → `metacognition`, `agents.build` → `cognition · build`. */
export function usedByLabel(key: string): string {
  if (key.startsWith("layers.")) return key.slice("layers.".length);
  if (key.startsWith("agents.")) return `cognition · ${crewLabel(key.slice("agents.".length))}`;
  return key;
}

/** Agents that may be the default agent: primary or all. */
export const canLead = (a: Pick<AgentEntry, "mode">) => a.mode === "primary" || a.mode === "all";

// ---------- search ----------

export interface SearchEntry {
  section: SectionId;
  /** Stable id a card or row checks to know whether it stays visible. */
  id: string;
  text: string;
}

export function searchIndex(body: SettingsBody | undefined, agents: AgentEntry[]): SearchEntry[] {
  const e = (section: SectionId, id: string, ...text: (string | null | undefined)[]): SearchEntry => ({ section, id, text: text.filter(Boolean).join(" ").toLowerCase() });
  const out: SearchEntry[] = [];
  for (const g of body?.gateways ?? []) out.push(e("gateways", `gateway:${g.name}`, "gateway provider", g.name, g.base_url, g.api_key_env, g.catalog_format, "timeout headers catalogue"));
  out.push(e("layers", "layer:reflex", "reflex fim editor lsp layers.reflex", choiceOf(body, "layers.reflex")?.model));
  out.push(e("layers", "layer:instinct", "instinct systemone layers.instinct", choiceOf(body, "layers.instinct")?.model));
  out.push(e("layers", "layer:metacognition", "metacognition chat variant layers.metacognition", choiceOf(body, "layers.metacognition")?.model));
  out.push(e("layers", "layer:cognition", "cognition agents default agent default_agent"));
  for (const a of agents) out.push(e("layers", `agent:${a.id}`, "agent", a.id, `agents.${a.id}`, a.mode, a.description, choiceOf(body, `agents.${a.id}`)?.model ?? a.effective?.model));
  out.push(e("permissions", "perm:rules", "permission rules always once reject deny bash write edit fetch instinct flow captain nav permissions"));
  out.push(e("permissions", "perm:scratch", "scratch directories permissions.scratch_dirs", ...asPaths(effective(fieldOf(body, "permissions.scratch_dirs")))));
  out.push(e("subagents", "sub:max", "max concurrent per session subagents.max_concurrent"));
  out.push(e("subagents", "sub:watchdog", "subagent watchdog subagents.watchdog_interval_secs"));
  out.push(e("subagents", "sub:root", "root agent watchdog agent.watchdog_interval_secs"));
  for (const t of THEMES) out.push(e("appearance", `theme:${t.id}`, "appearance theme tui.theme", t.label, t.id));
  return out;
}

/** The ids that match the query, or null when there is no query (everything shows). */
export function searchMatches(index: SearchEntry[], query: string): Set<string> | null {
  const words = query.trim().toLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) return null;
  return new Set(index.filter((entry) => words.every((w) => entry.text.includes(w))).map((entry) => entry.id));
}

// ---------- save status ----------

/**
 * Settings save on every change (one PUT per change, one at a time). The status line shows the
 * latest outcome; a 400 sticks to the key it is about until that key saves; a 409 means another
 * client wrote config.json in between, so the page reloads and the change is not replayed.
 */
export interface SaveState {
  /** Changes sent and not answered yet. */
  inFlight: number;
  /** "invalid": the server refused a value (its message is at the control); "error": the save failed. */
  status: "idle" | "saved" | "invalid" | "error" | "stale";
  message: string | null;
  /** The key of the last successful save, for the "applies…" hint. */
  lastKey: string | null;
  /** Server validation messages, by key. */
  fieldErrors: Record<string, string>;
  /** Which control of a key sent the refused value (a key may have several: model and variant). */
  errorControl: Record<string, string | undefined>;
  /** The control each in-flight key was changed from. */
  controls: Record<string, string | undefined>;
  /** The value each in-flight key shows until its answer (optimistic). */
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  pending: Record<string, any>;
}

export const initialSave: SaveState = { inFlight: 0, status: "idle", message: null, lastKey: null, fieldErrors: {}, errorControl: {}, controls: {}, pending: {} };

export type SaveEvent =
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  | { type: "start"; key: string; value: any; control?: string }
  | { type: "saved"; key: string }
  | { type: "invalid"; key: string; message: string }
  | { type: "stale" }
  | { type: "failed"; key: string; message: string };

export function saveReducer(state: SaveState, event: SaveEvent): SaveState {
  const settle = (key: string) => {
    const pending = { ...state.pending };
    delete pending[key];
    return { inFlight: Math.max(0, state.inFlight - 1), pending };
  };
  switch (event.type) {
    case "start":
      return { ...state, inFlight: state.inFlight + 1, pending: { ...state.pending, [event.key]: event.value }, controls: { ...state.controls, [event.key]: event.control } };
    case "saved": {
      const fieldErrors = { ...state.fieldErrors };
      delete fieldErrors[event.key];
      return { ...state, ...settle(event.key), status: "saved", message: null, lastKey: event.key, fieldErrors };
    }
    case "invalid":
      return {
        ...state,
        ...settle(event.key),
        status: "invalid",
        message: event.message,
        fieldErrors: { ...state.fieldErrors, [event.key]: event.message },
        errorControl: { ...state.errorControl, [event.key]: state.controls[event.key] },
      };
    case "stale":
      // Every queued change was made against the old file: drop them all.
      return { ...state, inFlight: 0, pending: {}, fieldErrors: {}, status: "stale", message: "config.json changed elsewhere; reloaded" };
    case "failed":
      return { ...state, ...settle(event.key), status: "error", message: event.message };
  }
}

/** When a saved change takes effect, as the server reports it for each key. */
export function appliesText(applies: SettingApplies | undefined): string | null {
  switch (applies) {
    case "restart":
      return "Applies after restarting WithNav";
    case "new_sessions":
      return "Applies to new sessions and subagents";
    case "live":
      return "Applies here now; other open windows on their next start";
    default:
      return null;
  }
}

/** `applies`: of the key last saved, so "Saved" also says when the change takes effect. */
export function statusLine(state: SaveState, applies?: SettingApplies): { tone: "ok" | "busy" | "warn" | "bad"; text: string } {
  if (state.inFlight > 0) return { tone: "busy", text: "Saving…" };
  if (state.status === "stale") return { tone: "warn", text: state.message ?? "Reloaded" };
  if (state.status === "invalid") return { tone: "bad", text: "Not saved" };
  if (state.status === "error") return { tone: "bad", text: `Not saved: ${state.message ?? "error"}` };
  const when = state.status === "saved" ? appliesText(applies) : null;
  return { tone: "ok", text: when ? `Saved · ${when[0]!.toLowerCase()}${when.slice(1)}` : "Saved" };
}
import { crewLabel } from "../components/Crew";
