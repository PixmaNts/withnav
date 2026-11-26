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
  yolo?: boolean;
  permissions_unavailable?: string | null;
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
  yolo?: boolean;
  permissions_unavailable?: string | null;
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

/** One MCP server as `GET /v1/mcp` and the `mcp.status` event report it: redacted and bounded. */
export interface McpServerView {
  server_id: string;
  transport: string;
  state: "starting" | "ready" | "failed" | "disabled" | string;
  /** 0 unless ready. */
  tool_count: number;
  skipped_tools: string[];
  skipped_more: number;
  error: string | null;
  stderr_tail: string;
  stderr_truncated: boolean;
}

/** The service's MCP servers. Keep the higher `revision` of one `incarnation`. */
export interface McpStatus {
  incarnation: string;
  revision: number;
  servers: McpServerView[];
}

export interface McpMediaItem {
  mime_type: string;
  data?: string;
  svg?: string;
}

export interface McpUiMeta {
  server: string;
  tool: string;
  resource_uri?: string;
  html?: string;
  url?: string;
}

export interface McpAppDescriptor {
  callId: string;
  server: string;
  tool: string;
  resourceUri?: string;
  html?: string;
  url?: string;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  args?: any;
  output?: string;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  metadata?: any;
}

export interface McpResourceBody {
  uri: string;
  mime_type: string;
  text?: string | null;
  blob?: string | null;
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

/** A stored image, by reference: the bytes stay on the server, behind the session's attachment route. */
export interface ImageRef {
  attachment_id: string;
  media_type?: string;
  name?: string;
  size?: number;
  width?: number;
  height?: number;
}

export type ContentBlock =
  | { type: "text"; text: string }
  | { type: "skill"; name: string }
  | { type: "image"; attachment_id: string };

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

/** One stored secret as `GET /v1/sessions/{id}/secrets` lists it: metadata and the last four characters at most. */
export interface SecretView {
  name: string;
  scope: "session" | "workspace";
  ttl: "1h" | "24h" | "session";
  expires_at?: string;
  created_at: string;
  use_count: number;
  last4: string;
}

// ---------- memory register (`src/server/memory.rs`, `withnav-client-api/src/memory.rs`) ----------

export type MemoryScope = "global" | "group" | "project" | "session";
export type MemoryKind = "decision" | "convention" | "preference" | "environment" | "pitfall";
export type MemoryStatus = "active" | "proposed" | "invalidated" | "rejected";

export interface MemoryVersion {
  version: number;
  change: "created" | "updated" | "invalidated" | "restored" | "rescoped" | string;
  title: string;
  body: string;
  why: string | null;
  quote: string | null;
  scope: MemoryScope;
  /** Empty for a global item; a group id, a project id or a session id otherwise. */
  scope_ref: string;
  source_actor: string;
  drafted_by: string;
  confirmed_by: string | null;
  source_session_id: string | null;
  source_event_seq: number | null;
  reason: string | null;
  at: string;
}

export interface MemoryItem {
  id: string;
  kind: MemoryKind | string;
  scope: MemoryScope;
  scope_ref: string;
  status: MemoryStatus | string;
  current_version: number;
  pinned: boolean;
  created_at: string;
  updated_at: string;
  current: MemoryVersion;
}

export interface MemoryItemDetail extends MemoryItem {
  /** Oldest first. */
  versions: MemoryVersion[];
}

export interface MemoryFilter {
  /** The items eligible for this session (active ones unless `status` is set). */
  session?: string;
  scope?: MemoryScope;
  kind?: MemoryKind;
  status?: MemoryStatus;
  scope_ref?: string;
  limit?: number;
}

export interface MemorySessionLink {
  session_id: string;
  project_id: string;
  /** False once detached: the link stays as history. */
  active: boolean;
}

export interface MemoryProject {
  id: string;
  name: string;
  /** False while the new-project group question is open. */
  group_answered: boolean;
  /** Group ids. */
  groups: string[];
  /** Other names the project is known by (repository paths). */
  aliases: string[];
  sessions: MemorySessionLink[];
}

export interface MemoryGroup {
  id: string;
  name: string;
  /** Project ids. */
  members: string[];
  /** Active items scoped to the group. */
  items: number;
}

export interface MembershipChange {
  id: number;
  relation: "project_group" | "session_project";
  project_id: string;
  group_id?: string;
  linked_session_id?: string;
  change: "add" | "remove" | string;
  status: "applied" | "proposed" | "rejected" | string;
  actor: string;
  session_id: string | null;
  at: string;
  decided_at: string | null;
}

export interface ProjectsBody {
  projects: MemoryProject[];
  groups: MemoryGroup[];
  history: MembershipChange[];
}

/** 409 body of a write that would let more projects read an item or group. */
export interface WideningBody {
  reason: "widening_requires_confirmation";
  affected_projects: MemoryProject[];
}

// ---------- session ledger (`GET /v1/sessions/{id}/ledger`, `withnav-client-api/src/ledger.rs`) ----------

export type CriterionStatus = "supported" | "contradicted" | "not_checked";
export type ClaimStatus = "supported" | "contradicted" | "unverified";

export interface CriterionCheck {
  /** Numbered from 1, like the goal's criteria. */
  index: number;
  status: CriterionStatus;
  /** A quote of the ledger line the status rests on; may be empty. */
  evidence: string;
}

export interface ClaimCheck {
  claim: string;
  status: ClaimStatus;
  evidence: string;
}

/** The last finished goal check of the goal's current revision. */
export interface GoalCheck {
  revision: number;
  /** The turn whose end started the check; a ULID, so it carries the time. */
  turn_id: string;
  statuses: CriterionCheck[];
  claims: ClaimCheck[];
  note_sent: boolean;
  note?: string;
}

export interface SessionGoal {
  revision: number;
  objective: string;
  criteria: string[];
  status: "active" | "paused";
}

export interface SessionLedger {
  through_seq: number | null;
  turns: { count: number; last?: { turn_id: string; outcome: "done" | "yielded" | "interrupted" | "error" } | null };
  /** Most recently changed first. */
  files_touched: { path: string; changes: number; last_turn: number }[];
  commands: { command: string; outcome: { status: string; code?: number; error?: string }; turn: number }[];
  /** Most failures first. */
  repeated_failures: { command: string; count: number; last_error: string }[];
  last_turn_text?: string | null;
  open_questions: { kind: "ask" | "secret" | "permission"; id: string; title: string; child_session_id?: string }[];
  compactions: number;
  goal?: SessionGoal | null;
  goal_check?: GoalCheck | null;
}
