// The JSON shapes of the InstinctFlow contracts (.scratch/instinct-flow/contracts.md §1, §4–§6).
// The server alone parses and writes TOML; the Studio edits this definition JSON and derives the
// canvas from it.

export type Scalar = string | number | boolean;

// ---------- definition (§1) ----------

export type DecisionType = "noul" | "choice" | "score";
export type NodeType = DecisionType | "expression" | "action";

/** `{ref = "<alias>"}` names one of the node's declared aliases; anything else is a literal. */
export type ParamValue = Scalar | { ref: string };

interface NodeBase {
  id: string;
  label: string | null;
  /** alias → reference (`event.args`, `session.agent`, `constant.MIN_YES`, `answer.asked_for`…). */
  inputs: Record<string, string>;
  /** port → node ids. */
  next: Record<string, string[]>;
}

export interface NoulNode extends NodeBase {
  type: "noul";
  instructions: string;
  /** A number in 0–1, or `constant.NAME`. */
  threshold: number | string;
  state_template: string | null;
}
export interface ChoiceNode extends NodeBase {
  type: "choice";
  instructions: string;
  criteria: Record<string, string>;
  state_template: string | null;
}
export interface ScoreNode extends NodeBase {
  type: "score";
  instructions: string;
  levels: string[];
  at_least: string;
  state_template: string | null;
}
export interface ExpressionNode extends NodeBase {
  type: "expression";
  condition: string;
  assign: Record<string, string>;
}
export interface ActionNode extends NodeBase {
  type: "action";
  action: string;
  params: Record<string, ParamValue>;
}
export type DecisionNode = NoulNode | ChoiceNode | ScoreNode;
export type FlowNode = DecisionNode | ExpressionNode | ActionNode;

export interface FlowHeader {
  version: 1;
  name: string;
  on: string;
  enabled: boolean;
  blocking: boolean;
  budget_ms: number | null;
  inputs: Record<string, string>;
  when: string | null;
  start: string[];
}

export interface FallbackAction {
  action: string;
  params: Record<string, ParamValue>;
  then: FallbackAction | null;
}
export const FALLBACK_KEYS = ["on_failure", "on_timeout", "on_provider_error", "on_invalid_answer"] as const;
export type FallbackKey = (typeof FALLBACK_KEYS)[number];
export type Fallback = { inputs: Record<string, string> } & Record<FallbackKey, FallbackAction | null>;

export interface Position {
  x: number;
  y: number;
}

export interface Definition {
  flow: FlowHeader;
  constants: Record<string, Scalar>;
  node: FlowNode[];
  fallback: Fallback | null;
  /** Card positions by node id, `trigger` included. Ignored by the engine. */
  layout: Record<string, Position>;
}

// ---------- catalogue (§4) ----------

export type ValueType = "string" | "number" | "integer" | "boolean" | "object" | "array";

export interface FieldSpec {
  name: string;
  value_type: ValueType;
  required: boolean;
}
export interface TriggerSpec {
  name: string;
  can_block: boolean;
  payload_fields: FieldSpec[];
  allowed_actions: string[];
  sample_payload: Record<string, unknown>;
  default_fallback: FallbackAction | null;
}
export interface InputSpec {
  name: string;
  input_type: "text" | "image";
  value_type: ValueType;
  source: string;
  triggers: string[];
}
export interface ActionSpec {
  name: string;
  kind: "verdict" | "effect";
  priority: number | null;
  params: FieldSpec[];
  allowed_triggers: string[];
}
export interface Catalogue {
  version: number;
  triggers: TriggerSpec[];
  inputs: InputSpec[];
  actions: ActionSpec[];
  limits: Record<string, number>;
}

// ---------- HTTP (§5) ----------

export type Location = "workspace" | "global" | "builtin";

export type DiagnosticCode =
  | "parse" | "schema" | "unknown_trigger" | "unknown_input" | "not_upstream" | "image_unsupported"
  | "unknown_action" | "action_not_allowed" | "invalid_parameter" | "cycle" | "unknown_node"
  | "unreachable" | "unknown_port" | "conflicting_verdicts" | "expression" | "template" | "limit"
  | "io" | "trust";

export interface Diagnostic {
  code: DiagnosticCode | string;
  message: string;
  path: string;
  node_id: string | null;
  line: number | null;
  column: number | null;
}

export type Content =
  | { format: "toml"; toml: string; templates: Record<string, string> }
  | { format: "graph"; definition: Definition; templates: Record<string, string> };

export interface FlowDocument {
  name: string;
  location: Location;
  path: string;
  revision: string;
  toml: string;
  definition: Definition | null;
  templates: Record<string, string>;
  errors: Diagnostic[];
  saved_since_reload: boolean;
  read_only: boolean;
}

export interface FlowSummary {
  name: string;
  trigger: string | null;
  location: Location;
  enabled: boolean | null;
  blocking: boolean | null;
  saved_since_reload: boolean;
  errors: Diagnostic[];
}

export interface ValidateResult {
  valid: boolean;
  toml: string | null;
  definition: Definition | null;
  templates: Record<string, string>;
  errors: Diagnostic[];
}

export type FlowSource =
  | { saved: { name: string; location: Location; revision: string } }
  | { content: Content };

export interface RunSummary {
  run_id: string;
  flow_name: string;
  started_at: string;
  status: RunStatus;
  duration_ms: number;
  cost_usd: number | null;
  mode: RunMode;
}

export interface HealthRow {
  name: string;
  runs: number;
  failures: number;
  fallbacks_applied: number;
  cost_usd: number | null;
  cost_unknown_runs: number;
  errors?: Diagnostic[];
  unavailable?: boolean;
}
export interface Health {
  window_start: string;
  window_end: string;
  flows: HealthRow[];
  errors?: Diagnostic[];
}

export interface Trust {
  workspace: string;
  has_workspace_flows: boolean;
  trusted: boolean;
}

export interface ReloadResult {
  reloaded: boolean;
  errors: Diagnostic[];
  flows: FlowSummary[];
  propagation?: string;
}

/** `{error, reason, errors}` on every non-2xx answer. */
export interface ErrorEnvelope {
  error: string;
  reason: string;
  errors: Diagnostic[];
}

// ---------- run records (§6) ----------

export type RunMode = "live" | "sample" | "replay";
export type RunStatus = "success" | "skipped" | "failure";

export type Answer =
  | { type: "noul"; noul: number }
  | { type: "choice"; choice: string; probabilities: Record<string, number>; confidence: number }
  | { type: "score"; score: number; legend: Record<string, string>; probabilities: Record<string, number>; confidence: number };

export interface RunNode {
  id: string;
  status: "completed" | "not_reached" | "failed";
  port: string | null;
  duration_ms: number | null;
  answer: Answer | null;
  result: boolean | null;
  variables: Record<string, unknown>;
  batch_id: string | null;
  error: string | null;
}

export interface Usage {
  input_tokens: number;
  output_tokens: number;
}

export interface RunBatch {
  id: string;
  node_ids: string[];
  state: string;
  questions: Record<string, unknown>;
  duration_ms: number;
  usage: Usage | null;
  cost_usd: number | null;
}

export interface ProposedAction {
  node_id: string | null;
  action: string;
  params: Record<string, unknown>;
}

export interface RunRecord {
  version: 1;
  run_id: string;
  flow_name: string;
  definition_digest: string;
  mode: RunMode;
  started_at: string;
  duration_ms: number;
  budget_ms: number | null;
  event: { trigger: string; payload: Record<string, unknown> };
  identity: { model: string; variant: string | null };
  inputs: Record<string, { value: unknown; cut: boolean; redacted?: boolean }>;
  nodes: RunNode[];
  batches: RunBatch[];
  status: RunStatus;
  failure: { kind: string; message: string } | null;
  actions: ProposedAction[];
  fallback_actions: ProposedAction[];
  usage: Usage | null;
  cost_usd: number | null;
  application: { status: "not_applied" | "applied" | "failed" | "uncertain" | "suppressed"; actions: { node_id: string | null; action: string; status: string; message: string | null }[] };
}

export type ReplayNodeStatus = "comparable" | "question_changed" | "missing_capture" | "not_reached";

export interface ReplayResult {
  comparable: boolean;
  nodes: { id: string; status: ReplayNodeStatus }[];
  outcome: RunRecord | null;
}
