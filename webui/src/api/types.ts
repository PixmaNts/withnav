// Wire shapes of the session server (`src/server/routes.rs`, `src/bus/event.rs`).
// Only the fields this client reads are typed; everything is validated loosely
// at the point of use because the server may add fields at any time.

export interface Envelope {
  schema_version: number;
  event_id: string;
  seq: number;
  session_id: string;
  actor_id: string;
  kind: string;
  occurred_at: string;
  causation_id: string | null;
  correlation_id: string | null;
  // The payload differs per kind; the reducer reads it defensively.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  payload: any;
}

export interface WorkspaceEntry {
  path: string;
  name: string;
  launched: boolean;
  session_count: number;
  updated_at: string | null;
  branch: string | null;
  dirty: number | null;
}

export interface SessionListEntry {
  bypass?: boolean;
  bypass_seq?: number;
  incarnation?: string | null;
  id: string;
  title: string | null;
  created_at: string;
  updated_at: string;
  message_count: number;
  agent: string;
  model: string;
  variant: string | null;
  live: boolean;
  read_only: boolean;
  resume_disabled_reason: string | null;
  parent_session_id?: string;
  child_status?: string;
  promptable?: boolean;
}

export interface SessionSummary {
  bypass?: boolean;
  bypass_seq?: number;
  incarnation?: string | null;
  session_id: string;
  actor_state: string;
  workspace: string;
  model: string;
  agent: string;
  variant: string | null;
  source: string | null;
  variant_source: string | null;
  gateway: string;
  upstream_id: string;
  current_seq: number;
  read_only: boolean;
  resume_disabled_reason: string | null;
}

export interface ModelEntry {
  name: string;
  gateway: string;
  provider_group: string;
  upstream_id: string;
  api: string;
  supports_tools: boolean;
  supports_parallel_tool_calls: boolean | null;
  context_window: number | null;
  enabled?: boolean;
  disabled_reason?: string;
  variants?: string[];
  default_variant?: string;
  reported_output?: number;
  max_input_tokens?: number;
}

export interface Selection {
  model: string;
  variant: string | null;
  source: string;
  variant_source: string;
}

export interface AgentEntry {
  id: string;
  description: string;
  mode: "primary" | "subagent" | "all" | string;
  assignment: Selection | null;
  effective: Selection | null;
  override?: { model?: string; variant?: string | null };
  warning: string | null;
  inherits_parent: boolean;
}

export interface AgentsBody {
  default_agent: string;
  agents: AgentEntry[];
}

export interface CommandInfo {
  name: string;
  description: string;
  args_hint: string;
}

export interface SkillsBody {
  skills: { name: string; description: string }[];
  disabled_reason?: string | null;
}

export interface IntentBody {
  exists: boolean;
  intent: string | null;
  plan: string | null;
  decisions: string | null;
  active_spec: string | null;
  raw: string | null;
}

export interface ContextSnapshot {
  model: string;
  variant?: string | null;
  window: number;
  window_source: string;
  estimate_total: number;
  input_budget: number;
  percent: number;
  incarnation?: string;
  revision: number;
  breakdown: Record<string, number>;
  last_usage?: { prompt_tokens: number; completion_tokens: number } | null;
}

export interface CommandOutcome {
  operation_id: string;
  ok: boolean;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  data?: any;
  error?: string;
}

export type ContentBlock = { type: "text"; text: string } | { type: "skill"; name: string };

// Settings (`.scratch/settings/contract.md`): config.json fields and the gateways of models.json.
export type SettingApplies = "new_sessions" | "restart" | "live";

export interface SettingField {
  /** Dotted key as in config.json: `layers.reflex`, `agents.build`, `tui.theme`, … */
  key: string;
  // The value shape depends on the key; read through `lib/settings.ts`.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  value: any;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  default: any;
  set: boolean;
  error: string | null;
  applies: SettingApplies;
}

export interface GatewayInfo {
  name: string;
  base_url: string;
  api_key_env: string | null;
  key_source: "auth_store" | "environment" | "missing";
  catalog_format: string | null;
  connect_timeout_ms: number | null;
  request_timeout_ms: number | null;
  headers: string[];
  model_count: number | null;
  used_by: string[];
}

export interface SettingsBody {
  revision: string;
  paths: { config: string; models: string | null; agents: string[] };
  fields: SettingField[];
  gateways: GatewayInfo[];
}

export interface LayerEntry {
  layer: "reflex" | "instinct" | "metacognition" | string;
  status: "usable" | "disabled" | string;
  model: string | null;
  variant: string | null;
  gateway: string | null;
  upstream_id: string | null;
  api: string | null;
  reason: string | null;
  variant_diagnostic?: string;
}

export interface GatewayTest {
  ok: boolean;
  status: number | null;
  latency_ms: number;
  model_count: number | null;
  error: string | null;
}
