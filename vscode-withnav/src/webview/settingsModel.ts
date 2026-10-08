import {
  appliesText,
  asChoice,
  carryVariant,
  effectiveValue,
  isModified,
  type AgentRow,
  type CatalogModel,
  type GatewayInfo,
  type SettingField,
  type SettingsSnapshot,
} from "../settingsProtocol";

/** The control a row shows, as in the VS Code settings editor. */
export type Control =
  | { kind: "text"; value: string; disabled?: boolean; mono?: boolean; wide?: boolean }
  | { kind: "number"; value: number | null; min?: number; max?: number; disabled?: boolean; placeholder?: string }
  | { kind: "select"; value: string; options: SelectOption[]; groups?: SelectGroup[]; disabled?: boolean; wide?: boolean }
  | { kind: "checkbox"; checked: boolean; label: string }
  | { kind: "list"; items: string[]; editable: boolean; placeholder?: string; links?: { label: string; section: string }[] }
  | { kind: "link"; label: string; action: LinkAction }
  | { kind: "test"; gateway: string };

export type LinkAction = { open: "config" | "models" } | { openAgent: string } | { section: string };

export interface SelectOption {
  value: string;
  label: string;
  disabled?: boolean;
}
export interface SelectGroup {
  label: string;
  options: SelectOption[];
}

export interface Row {
  /** Unique per page; the config key when the row writes one. */
  id: string;
  /** The `config.json` key the row writes; absent for read-only rows. */
  key?: string;
  category: string;
  title: string;
  description: string;
  /** When the change takes effect; shown after the description. */
  applies?: string;
  control: Control;
  modified: boolean;
  /** The stored value is invalid (read from config.json). */
  error?: string | null;
  /** A non-blocking note under the control (layer not usable, agent fallback). */
  warning?: string | null;
  /** Extra words the search matches (key, values). */
  terms: string;
  /** What the control writes for a given input; null removes the key. */
  write?: (input: string | boolean | string[]) => unknown;
  /** Shows "Reset Setting" when config.json holds the key. */
  resettable: boolean;
}

export interface Section {
  id: string;
  label: string;
  /** 0 = top level, 1 = child, 2 = agent under cognition. */
  level: number;
  /** The section a top-level entry opens (Gateways opens its first gateway). */
  opens?: string;
  title: string;
  description: string;
  /** A link at the end of the description. */
  descriptionLink?: { label: string; action: LinkAction };
  notes?: string[];
  rows: Row[];
}

const LAYERS = [
  { id: "reflex", label: "Reflex", api: "fim", role: "Ghost-text suggestions in the editor.", note: "Only fill-in-the-middle models are offered. Runs only through the language server, with an IDE attached." },
  { id: "instinct", label: "Instinct", api: "systemone", role: "Typed decisions: InstinctFlow, permission review, navigator pipeline. Fixed 2 s deadline.", note: "Only models behind the System One API are offered." },
  { id: "metacognition", label: "Metacognition", api: "chat", role: "Remarks, wakes, and approval of risky calls.", note: "A fast (flash) chat model works best here." },
] as const;

const THEMES: SelectOption[] = [
  { value: "ayu-dark", label: "ayu-dark" },
  { value: "ayu-light", label: "ayu-light" },
  { value: "nord-dark", label: "nord-dark" },
  { value: "nord-light", label: "nord-light" },
];

const KEY_SOURCE: Record<GatewayInfo["key_source"], string> = {
  auth_store: "Key found in the auth store.",
  environment: "Key found in the environment.",
  missing: "No key found: set the variable or log in to the auth store.",
};

/** `/home/me/x` -> `~/x`. */
export function shortPath(file: string, home?: string): string {
  if (home && (file === home || file.startsWith(home.endsWith("/") ? home : `${home}/`))) return `~${file.slice(home.replace(/\/$/, "").length)}`;
  return file;
}

const field = (s: SettingsSnapshot, key: string): SettingField | undefined => s.settings.fields.find((f) => f.key === key);

/** Every section of the page, in table-of-contents order. */
export function buildSections(s: SettingsSnapshot): Section[] {
  const sections: Section[] = [];
  const gateways = s.settings.gateways;
  sections.push({ id: "gateways", label: "Gateways", level: 0, opens: gateways[0] ? `gateway:${gateways[0].name}` : undefined, title: "Gateways", description: "models.json → providers", rows: [] });
  for (const g of gateways) sections.push(gatewaySection(g));
  sections.push({ id: "layers", label: "Layers", level: 0, opens: "layer:reflex", title: "Layers", description: "", rows: [] });
  for (const l of LAYERS) sections.push(layerSection(s, l));
  sections.push(cognitionSection(s));
  const agents = [...s.agents];
  for (const f of s.settings.fields.filter((f) => f.key.startsWith("agents."))) {
    const id = f.key.slice("agents.".length);
    if (!agents.some((a) => a.id === id)) agents.push({ id, description: "", mode: "primary", effective: null, warning: null });
  }
  for (const a of agents) sections.push(agentSection(s, a));
  sections.push(permissionsSection(s), subagentsSection(s), appearanceSection(s));
  for (const section of sections) {
    const notes: string[] = [];
    if (section.id.startsWith("layer:") || section.id.startsWith("agent:") || section.id === "layers") {
      if (s.notes?.models) notes.push(s.notes.models);
      if (s.notes?.layers && !section.id.startsWith("agent:")) notes.push(s.notes.layers);
    }
    if (section.id === "layer:cognition" || section.id.startsWith("agent:")) {
      if (s.notes?.agents) notes.push(s.notes.agents);
    }
    section.notes = notes;
    if (s.notes?.models) {
      for (const row of section.rows) {
        if (row.control.kind === "select" && row.title === "Model") {
          for (const option of row.control.options) {
            if (option.value && option.value === row.control.value) option.label = option.value;
          }
        }
      }
    }
  }
  return sections;
}

function gatewaySection(g: GatewayInfo): Section {
  const c = `Gateways › ${g.name}:`;
  const ro = (id: string, title: string, description: string, control: Control, terms = ""): Row => ({
    id: `gateway:${g.name}:${id}`, category: c, title, description, control, modified: false, resettable: false, terms: `${g.name} ${terms}`,
  });
  const used = g.used_by.length ? `Used by ${g.used_by.join(", ")}.` : "Not used by any layer or agent.";
  return {
    id: `gateway:${g.name}`, label: g.name, level: 1, title: g.name,
    description: `models.json → providers.${g.name} · catalogue: ${g.model_count} models · read-only here.`,
    descriptionLink: { label: "Edit in models.json", action: { open: "models" } },
    rows: [
      ro("base_url", "Base URL", "Root URL of the provider's API.", { kind: "text", value: g.base_url, disabled: true, mono: true }, g.base_url),
      ro("api_key_env", "API Key Env", `Environment variable that holds the key. Keys saved in the auth store are used too. Never shown. ${KEY_SOURCE[g.key_source]}`, { kind: "text", value: g.api_key_env ?? "", disabled: true, mono: true }, `${g.api_key_env ?? ""} ${g.key_source}`),
      ro("catalog_format", "Catalog Format", "How the model list is read from the provider.", { kind: "select", value: g.catalog_format ?? "none", options: [{ value: g.catalog_format ?? "none", label: g.catalog_format ?? "none" }], disabled: true }),
      ro("connect_timeout_ms", "Connect Timeout Ms", "Time allowed to open a connection.", { kind: "number", value: g.connect_timeout_ms, disabled: true }),
      ro("request_timeout_ms", "Request Timeout Ms", "Ceiling for one request. The instinct always uses its own 2 s deadline.", { kind: "number", value: g.request_timeout_ms, disabled: true }),
      g.headers.length
        ? ro("headers", "Headers", "Extra HTTP headers sent with every request. Names only: values may be secrets.", { kind: "list", items: g.headers, editable: false }, g.headers.join(" "))
        : ro("headers", "Headers", "Extra HTTP headers sent with every request.", { kind: "link", label: "Edit in models.json", action: { open: "models" } }),
      ro("connection", "Connection", `Lists the gateway's models with its key and connect timeout. ${used}`, { kind: "test", gateway: g.name }, g.used_by.join(" ")),
    ],
  };
}

/** Chat/fim/systemone models of the catalogue, grouped gateway › provider. */
export function modelGroups(models: CatalogModel[], api: string, current: string): { groups: SelectGroup[]; missing?: SelectOption } {
  const groups = new Map<string, SelectOption[]>();
  for (const m of models.filter((m) => m.api === api)) {
    const label = m.provider_group && m.provider_group !== m.gateway ? `${m.gateway} › ${m.provider_group}` : m.gateway;
    if (!groups.has(label)) groups.set(label, []);
    groups.get(label)!.push({ value: m.name, label: m.name, disabled: m.enabled === false && m.name !== current });
  }
  const list = [...groups].map(([label, options]) => ({ label, options }));
  const missing = current && !models.some((m) => m.name === current) ? { value: current, label: `${current} (not in the catalogue)` } : undefined;
  return { groups: list, missing };
}

function variantOptions(model: CatalogModel | undefined): SelectOption[] {
  const variants = model?.variants ?? [];
  const def = model?.default_variant;
  return [
    { value: "", label: def ? `${def} (model default)` : "model default" },
    ...variants.map((v) => ({ value: v, label: v })),
    ...(variants.length ? [{ value: "off", label: "off" }] : []),
  ];
}

function layerSection(s: SettingsSnapshot, l: (typeof LAYERS)[number]): Section {
  const key = `layers.${l.id}`;
  const f = field(s, key);
  const choice = asChoice(effectiveValue(f));
  const model = s.models.find((m) => m.name === choice?.model);
  const status = s.layers.find((x) => x.layer === l.id);
  const c = `Layers › ${l.id}:`;
  const applies = f ? appliesText(f.applies) : undefined;
  const { groups, missing } = modelGroups(s.models, l.api, choice?.model ?? "");
  const rows: Row[] = [{
    id: key, key, category: c, title: "Model", description: `<gateway>/<id>. ${l.note}`, applies,
    control: { kind: "select", value: choice?.model ?? "", options: [...(choice ? [] : [{ value: "", label: "not assigned" }]), ...(missing ? [missing] : [])], groups, wide: true },
    modified: isModified(f), resettable: !!f?.set, error: f?.error,
    warning: status && status.status !== "usable" ? `Not usable: ${status.reason ?? status.status}` : null,
    terms: `${key} ${choice?.model ?? ""} ${l.api}`,
    write: (input) => (input ? { model: String(input), variant: carryVariant(choice?.variant ?? null, s.models.find((m) => m.name === input)) } : null),
  }];
  if (l.id === "metacognition") {
    const offered = model?.variants?.length ? `Variants offered by the model: ${model.variants.join(", ")}.` : "The model offers no variant.";
    rows.push({
      id: `${key}.variant`, key, category: c, title: "Variant", description: `${offered} low by default when offered.`, applies,
      control: { kind: "select", value: choice?.variant ?? "", options: variantOptions(model), disabled: !choice || !model?.variants?.length },
      // The metacognition runs `low` when the model offers it, so only another variant is a change.
      modified: !!choice?.variant && choice.variant !== "low", resettable: false, terms: `variant ${(model?.variants ?? []).join(" ")}`,
      write: (input) => (choice ? { model: choice.model, variant: input ? String(input) : null } : undefined),
    });
  }
  rows.push(
    { id: `${key}.context`, category: c, title: "Context Window", description: "From the catalogue.", control: { kind: "number", value: model?.context_window ?? null, disabled: true, placeholder: "unknown" }, modified: false, resettable: false, terms: "context window tokens" },
    { id: `${key}.output`, category: c, title: "Max Output Tokens", description: "Reported by the gateway, when it does.", control: { kind: "number", value: model?.reported_output ?? null, disabled: true, placeholder: "not reported" }, modified: false, resettable: false, terms: "max output tokens" },
  );
  return { id: `layer:${l.id}`, label: l.id, level: 1, title: l.label, description: `config.json → ${key} · one model at a time. ${l.role}`, rows };
}

const leads = (a: AgentRow) => a.mode === "primary" || a.mode === "all";

function cognitionSection(s: SettingsSnapshot): Section {
  const f = field(s, "default_agent");
  const current = String(effectiveValue(f) ?? "");
  const c = "Layers › cognition:";
  const options = s.agents.filter(leads).map((a) => ({ value: a.id, label: a.id }));
  if (current && !options.some((o) => o.value === current)) options.unshift({ value: current, label: `${current} (not a primary agent)` });
  const agentModel = (a: AgentRow) => {
    const choice = asChoice(effectiveValue(field(s, `agents.${a.id}`))) ?? a.effective;
    return choice?.model ?? "inherits the parent session";
  };
  return {
    id: "layer:cognition", label: "cognition", level: 1, title: "Cognition",
    description: "The agentic chat. It runs agents, each with its own model, and several flows at once through subagents.",
    rows: [
      {
        id: "default_agent", key: "default_agent", category: c, title: "Default Agent", description: "config.json → default_agent. A new conversation starts with this primary agent.",
        applies: f ? appliesText(f.applies) : undefined, control: { kind: "select", value: current, options }, modified: isModified(f), resettable: !!f?.set, error: f?.error,
        terms: `default_agent ${current}`, write: (input) => (input ? String(input) : null),
      },
      {
        id: "agents", category: c, title: "Agents", description: `Defined in ${s.settings.paths.agents.map((d) => shortPath(d, s.home)).join(" and ")}. Pick one to edit its model.`,
        control: { kind: "list", items: s.agents.map((a) => `${a.id}  ·  ${agentModel(a)}`), editable: false, links: s.agents.map((a) => ({ label: `${a.id}  ·  ${agentModel(a)}`, section: `agent:${a.id}` })) },
        modified: false, resettable: false, terms: s.agents.map((a) => a.id).join(" "),
      },
    ],
  };
}

const SOURCE: Record<string, string> = {
  config: "config.json",
  header: "its .md file",
  default_agent: "the default agent",
  parent: "the parent session",
  layer_preference: "the layer preference",
};

function agentSection(s: SettingsSnapshot, a: AgentRow): Section {
  const key = `agents.${a.id}`;
  const f = field(s, key);
  const stored = asChoice(effectiveValue(f));
  const shown = stored ?? (a.effective ? { model: a.effective.model, variant: a.effective.variant } : null);
  const model = s.models.find((m) => m.name === shown?.model);
  const c = `Cognition › ${a.id}:`;
  const applies = f ? appliesText(f.applies) : undefined;
  const { groups, missing } = modelGroups(s.models, "chat", stored?.model ?? "");
  const inherit = a.effective
    ? `not set: ${a.effective.model} from ${SOURCE[a.effective.source] ?? a.effective.source}`
    : "not set: inherits the parent session";
  const file = a.definition ? shortPath(a.definition, s.home) : null;
  const offered = model?.variants?.length ? `Offered by this model: ${model.variants.join(", ")}.` : "This model offers no variant.";
  return {
    id: `agent:${a.id}`, label: a.id, level: 2, title: a.id, description: a.description || `Agent ${a.id}.`,
    rows: [
      {
        id: key, key, category: c, title: "Model", description: `<gateway>/<id>. Saved in config.json → ${key}.model; wins over a model: line in ${a.id}.md.`, applies,
        control: { kind: "select", value: stored?.model ?? "", options: [{ value: "", label: inherit }, ...(missing ? [missing] : [])], groups, wide: true },
        modified: isModified(f), resettable: !!f?.set, error: f?.error, warning: a.warning, terms: `${key} ${shown?.model ?? ""}`,
        write: (input) => (input ? { model: String(input), variant: carryVariant(stored?.variant ?? null, s.models.find((m) => m.name === input)) } : null),
      },
      {
        id: `${key}.variant`, key, category: c, title: "Variant", description: offered, applies,
        control: { kind: "select", value: stored?.variant ?? "", options: variantOptions(model), disabled: !stored || !model?.variants?.length },
        modified: !!stored?.variant, resettable: false, terms: `variant ${(model?.variants ?? []).join(" ")}`,
        write: (input) => (stored ? { model: stored.model, variant: input ? String(input) : null } : undefined),
      },
      {
        id: `${key}.mode`, category: c, title: "Mode", description: `primary drives a conversation, subagent runs as a background flow, all does both. Set in ${file ? `${a.id}.md` : "the built-in definition"}.`,
        control: { kind: "select", value: a.mode, options: [{ value: a.mode, label: a.mode }], disabled: true }, modified: false, resettable: false, terms: `mode ${a.mode}`,
      },
      {
        id: `${key}.definition`, category: c, title: "Definition", description: "Prompt, tools and access live in the agent file. Its model: line is overridden by config.json.",
        control: file ? { kind: "link", label: file, action: { openAgent: a.id } } : { kind: "text", value: "built-in agent", disabled: true, mono: true },
        modified: false, resettable: false, terms: file ?? "built-in",
      },
    ],
  };
}

function permissionsSection(s: SettingsSnapshot): Section {
  const review = field(s, "permissions.automatic_review");
  const dirs = field(s, "permissions.scratch_dirs");
  const list = effectiveValue(dirs);
  const items = Array.isArray(list) ? list.map(String) : [];
  return {
    id: "permissions", label: "Permissions", level: 0, title: "Permissions", description: "config.json → permissions",
    rows: [
      {
        id: "permissions.automatic_review", key: "permissions.automatic_review", category: "Permissions:", title: "Automatic Review",
        description: "Before a bash, write or edit runs, the instinct classifies effects, scope and risk; risky or outside-project changes need the metacognition's approval; any doubt or failure asks you. fetch keeps its origin grants.",
        applies: review ? appliesText(review.applies) : undefined,
        control: { kind: "checkbox", checked: effectiveValue(review) !== false, label: "Review calls automatically. When off, every bash, write and edit call is asked." },
        modified: isModified(review), resettable: !!review?.set, error: review?.error, terms: "permissions.automatic_review approval review",
        write: (input) => input === true,
      },
      {
        id: "permissions.scratch_dirs", key: "permissions.scratch_dirs", category: "Permissions:", title: "Scratch Dirs",
        description: "Absolute paths agents may write to without asking. An empty list turns scratch space off.",
        applies: dirs ? appliesText(dirs.applies) : undefined,
        control: { kind: "list", items, editable: true, placeholder: "/absolute/path" },
        modified: isModified(dirs), resettable: !!dirs?.set, error: dirs?.error, terms: `permissions.scratch_dirs ${items.join(" ")}`,
        write: (input) => (Array.isArray(input) ? input : []),
      },
    ],
  };
}

function numberRow(s: SettingsSnapshot, key: string, category: string, title: string, description: string, min: number, max: number): Row {
  const f = field(s, key);
  const v = effectiveValue(f);
  return {
    id: key, key, category, title, description, applies: f ? appliesText(f.applies) : undefined,
    control: { kind: "number", value: typeof v === "number" ? v : null, min, max },
    modified: isModified(f), resettable: !!f?.set, error: f?.error, terms: key,
    write: (input) => Number(input),
  };
}

function subagentsSection(s: SettingsSnapshot): Section {
  return {
    id: "subagents", label: "Subagents", level: 0, title: "Subagents", description: "config.json → subagents, agent",
    rows: [
      numberRow(s, "subagents.max_concurrent", "Subagents:", "Max Concurrent", "Children one session may have starting, running or waiting for an answer. 1 to 64.", 1, 64),
      numberRow(s, "subagents.watchdog_interval_secs", "Subagents:", "Watchdog Interval Secs", "Progress check for running children. 0 disables supervision.", 0, 31536000),
      numberRow(s, "agent.watchdog_interval_secs", "Agent:", "Watchdog Interval Secs", "The same check for the session's own agent. 0 disables it.", 0, 31536000),
    ],
  };
}

function appearanceSection(s: SettingsSnapshot): Section {
  const f = field(s, "tui.theme");
  const current = String(effectiveValue(f) ?? "ayu-dark");
  const options = THEMES.some((t) => t.value === current) ? THEMES : [{ value: current, label: `${current} (unknown)` }, ...THEMES];
  return {
    id: "appearance", label: "Appearance", level: 0, title: "Appearance", description: "config.json → tui.theme",
    rows: [{
      id: "tui.theme", key: "tui.theme", category: "Tui:", title: "Theme",
      description: "Terminal client and WebUI theme. This page follows your VS Code theme.", applies: f ? appliesText(f.applies) : undefined,
      control: { kind: "select", value: current, options }, modified: f?.set === true && current !== "ayu-dark", resettable: !!f?.set, error: f?.error,
      terms: `tui.theme ${current}`, write: (input) => (input ? String(input) : null),
    }],
  };
}

/** True when every word of the query appears in the row (category, title, description, key, values). */
export function matches(row: Row, query: string): boolean {
  const q = query.trim().toLowerCase();
  if (!q) return true;
  const words = q.split(/\s+/);
  const modifiedOnly = words.includes("@modified");
  const rest = words.filter((w) => w !== "@modified");
  if (modifiedOnly && !row.modified) return false;
  const text = `${row.category} ${row.title} ${row.description} ${row.terms}`.toLowerCase();
  return rest.every((w) => text.includes(w));
}

/** Every row of the page, for the count in the search box. */
export function countRows(sections: Section[]): number {
  return sections.reduce((n, s) => n + s.rows.length, 0);
}
