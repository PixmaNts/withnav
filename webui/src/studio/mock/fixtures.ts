// Seed data of the mock API: the WithNav catalogue of contracts §7, the complete `guard-mutations`
// example of contracts §1, two smaller flows so the list has more than one group, and one recorded
// run of `guard-mutations`.

import type { ActionSpec, Catalogue, Diagnostic, FieldSpec, InputSpec, RunRecord, TriggerSpec } from "../types";

export const HOST_DIAGNOSTICS: Diagnostic[] = [
  { code: "unavailable", message: "Instinct unavailable: the mock host has no live model configured. Sample answers are invented.", path: "", node_id: null, line: null, column: null },
];
export const RELOAD_PROPAGATION = "Other WithNav processes reload on their next event.";

const f = (name: string, value_type: FieldSpec["value_type"], required = true): FieldSpec => ({ name, value_type, required });

const EDITOR = ["save", "idle", "diagnostics", "conversation"];
const AGENT = ["tool.pre_call", "message.received", "turn.end"];
const ALL = [...EDITOR, ...AGENT].sort();
const EFFECTS = ["wake", "inject_note", "log"];

const triggers: TriggerSpec[] = [
  {
    name: "conversation",
    can_block: false,
    payload_fields: [f("turn_id", "string"), f("text", "string"), f("uri", "string", false)],
    allowed_actions: ["remark", ...EFFECTS],
    sample_payload: { turn_id: "t42", text: "Swap the toml parser, keep Config::load(p).", uri: "file:///repo/src/config.rs" },
    default_fallback: null,
  },
  {
    name: "diagnostics",
    can_block: false,
    payload_fields: [f("uri", "string"), f("language_id", "string"), f("version", "integer"), f("diagnostics", "array"), f("diff", "string")],
    allowed_actions: ["remark", ...EFFECTS],
    sample_payload: {
      uri: "file:///repo/src/config.rs",
      language_id: "rust",
      version: 12,
      diagnostics: [{ message: "unused variable: `p`", severity: 2, range: { start: { line: 41, character: 8 }, end: { line: 41, character: 9 } } }],
      diff: "@@ -40,3 +40,4 @@\n+    let p = path;",
    },
    default_fallback: null,
  },
  {
    name: "idle",
    can_block: false,
    payload_fields: [f("uri", "string"), f("language_id", "string"), f("version", "integer"), f("diff", "string"), f("idle_ms", "integer")],
    allowed_actions: ["remark", ...EFFECTS],
    sample_payload: { uri: "file:///repo/src/parser/incr.rs", language_id: "rust", version: 7, diff: "@@ -1 +1,2 @@\n+use toml_edit::DocumentMut;", idle_ms: 4000 },
    default_fallback: null,
  },
  {
    name: "message.received",
    can_block: true,
    payload_fields: [f("command_id", "string"), f("author", "string"), f("text", "string"), f("selected_skill", "object", false)],
    allowed_actions: [...EFFECTS],
    sample_payload: { command_id: "cmd_01", author: "captain", text: "Can you review the diff in incr.rs before I commit?", selected_skill: null },
    default_fallback: null,
  },
  {
    name: "save",
    can_block: false,
    payload_fields: [f("uri", "string"), f("language_id", "string"), f("version", "integer"), f("diff", "string")],
    allowed_actions: ["remark", ...EFFECTS],
    sample_payload: { uri: "file:///repo/src/config.rs", language_id: "rust", version: 13, diff: "@@ -42 +42 @@\n-pub fn load(p: &Path)\n+pub fn load(p: &Path, strict: bool)" },
    default_fallback: null,
  },
  {
    name: "tool.pre_call",
    can_block: true,
    payload_fields: [f("tool", "string"), f("args", "object"), f("call_id", "string")],
    allowed_actions: ["ask_captain", "deny", "inject_note", "log", "no_objection", "wake"],
    sample_payload: { tool: "write", args: { path: "src/lib.rs", content: "" }, call_id: "sample" },
    default_fallback: { action: "ask_captain", params: { note: "Flow failed: {failure_message}" }, then: null },
  },
  {
    name: "turn.end",
    can_block: false,
    payload_fields: [f("turn_id", "string"), f("text", "string"), f("status", "string")],
    allowed_actions: [...EFFECTS],
    sample_payload: { turn_id: "t42", text: "Done: the parser is swapped and the tests pass.", status: "completed" },
    default_fallback: null,
  },
];

const input = (name: string, value_type: InputSpec["value_type"], source: string, on: string[]): InputSpec => ({ name, input_type: "text", value_type, source, triggers: on });

const inputs: InputSpec[] = [
  input("conversation.digest", "string", "conversation", ALL),
  input("conversation.last_captain", "string", "conversation", ALL),
  input("conversation.last_turns", "array", "conversation", ALL),
  input("editor.cursor", "object", "editor", EDITOR),
  input("editor.open_files", "array", "editor", EDITOR),
  input("editor.selection", "string", "editor", EDITOR),
  input("intent.record", "string", "intent", ALL),
  input("session.agent", "string", "session", ALL),
  input("session.id", "string", "session", ALL),
  input("session.model", "string", "session", ALL),
  input("session.workspace", "string", "session", ALL),
  input("workspace.branch", "string", "workspace", ALL),
  input("workspace.dirty_files", "array", "workspace", ALL),
  input("workspace.root", "string", "workspace", ALL),
];

const note = [f("note", "string")];
const actions: ActionSpec[] = [
  { name: "ask_captain", kind: "verdict", priority: 1, params: note, allowed_triggers: ["tool.pre_call"] },
  { name: "deny", kind: "verdict", priority: 2, params: note, allowed_triggers: ["tool.pre_call"] },
  { name: "inject_note", kind: "effect", priority: null, params: [f("text", "string")], allowed_triggers: ALL },
  { name: "log", kind: "effect", priority: null, params: [f("text", "string")], allowed_triggers: ALL },
  { name: "no_objection", kind: "verdict", priority: 0, params: [], allowed_triggers: ["tool.pre_call"] },
  { name: "remark", kind: "effect", priority: null, params: [f("prompt", "string")], allowed_triggers: EDITOR },
  { name: "wake", kind: "effect", priority: null, params: note, allowed_triggers: ALL },
];

export const CATALOGUE: Catalogue = {
  version: 1,
  triggers,
  inputs,
  actions,
  limits: {
    nodes: 64, decisions_per_run: 16, blocking_budget_ms: 2000, async_deadline_ms: 5000, call_deadline_ms: 2000,
    flow_bytes: 262144, input_bytes: 16384, run_input_bytes: 65536, state_bytes: 65536, template_bytes: 65536,
    expression_bytes: 4096, expression_tokens: 128, expression_operators: 32, expression_nesting: 16,
    expression_ast_depth: 32, expression_ast_nodes: 128, response_bytes: 262144, concurrent_calls: 4,
  },
};

/** Contracts §1, verbatim. */
export const GUARD_MUTATIONS_TOML = `[flow]
version = 1
name = "guard-mutations"
on = "tool.pre_call"
enabled = true
blocking = true
budget_ms = 800
start = ["effect"]
when = 'tool == "bash" || tool == "write" || tool == "edit" || tool == "fetch"'
[flow.inputs]
tool = "event.tool"

[constants]
MIN_YES = 0.75

[fallback]
on_failure = { action = "ask_captain", params = { note = "Flow failed: {failure_message}" } }
on_timeout = { action = "no_objection", params = {} }
on_provider_error = { action = "ask_captain", params = { note = "Instinct unavailable" } }
on_invalid_answer = { action = "ask_captain", params = { note = "Invalid instinct answer" } }

[[node]]
id = "effect"
type = "choice"
instructions = "What will this call do? Choose its most consequential effect."
[node.inputs]
tool = "event.tool"
args = "event.args"
agent = "session.agent"
[node.criteria]
read_only = "Only reads files or prints information."
local_change = "Changes files inside the workspace."
network = "Reaches a host outside the machine."
destructive = "Deletes, force-pushes or rewrites history."
[node.next]
read_only = ["continue"]
local_change = ["asked_for"]
network = ["trusted_host"]
destructive = ["blast_radius"]

[[node]]
id = "asked_for"
type = "noul"
instructions = "Did the Captain or the intent record ask for this exact change?"
threshold = "constant.MIN_YES"
[node.inputs]
last_captain = "conversation.last_captain"
intent = "intent.record"
args = "event.args"
[node.next]
yes = ["continue"]
no = ["plan_overreach"]

[[node]]
id = "trusted_host"
type = "noul"
instructions = "Is this a read-only request to an identifiable public documentation host?"
threshold = 0.9
[node.inputs]
args = "event.args"
[node.next]
yes = ["continue"]
no = ["ask"]

[[node]]
id = "blast_radius"
type = "score"
instructions = "How far could this reach if it goes wrong? Levels are weakest first."
levels = ["one file", "one folder", "whole repo", "outside the repo"]
at_least = "whole repo"
[node.inputs]
args = "event.args"
root = "workspace.root"
dirty = "workspace.dirty_files"
[node.next]
at_least = ["deny"]
below = ["ask"]

[[node]]
id = "plan_overreach"
type = "expression"
condition = 'asked_for < THRESHOLD'
[node.inputs]
asked_for = "answer.asked_for"
agent = "session.agent"
[node.assign]
# The plan agent should not change code: demand a clearer request from it.
THRESHOLD = 'if(agent == "plan", 0.9, 0.6)'
[node.next]
true = ["deny"]
false = ["ask"]

[[node]]
id = "continue"
type = "action"
action = "no_objection"
[node.params]

[[node]]
id = "ask"
type = "action"
action = "ask_captain"
[node.inputs]
effect = "answer.effect"
asked_for = "answer.asked_for"
[node.params]
note = "Review {effect}; requested confidence: {asked_for}."

[[node]]
id = "deny"
type = "action"
action = "deny"
[node.params]
note = "Change not requested, or destructive reach too broad. Propose it to the Captain."
[node.next]
then = ["audit"]

[[node]]
id = "audit"
type = "action"
action = "log"
[node.params]
text = "guard-mutations proposed denial"

[layout]
trigger = { x = 16, y = 230 }
effect = { x = 226, y = 200 }
asked_for = { x = 476, y = 30 }
trusted_host = { x = 476, y = 250 }
blast_radius = { x = 476, y = 450 }
plan_overreach = { x = 726, y = 120 }
continue = { x = 966, y = 30 }
ask = { x = 966, y = 260 }
deny = { x = 966, y = 470 }
audit = { x = 1180, y = 470 }
`;

/** A global, asynchronous flow on save. */
export const REMARK_ON_SAVE_TOML = `[flow]
version = 1
name = "public-api-drift"
on = "save"
enabled = true
start = ["touches_api"]

[[node]]
id = "touches_api"
type = "noul"
instructions = "Does this diff change a public signature that the intent record says must stay unchanged?"
threshold = 0.7
[node.inputs]
diff = "event.diff"
intent = "intent.record"
[node.next]
yes = ["remark_drift"]

[[node]]
id = "remark_drift"
type = "action"
action = "remark"
[node.params]
prompt = "The diff changes a public signature; the intent record says the public API stays unchanged."

[layout]
trigger = { x = 16, y = 120 }
touches_api = { x = 256, y = 90 }
remark_drift = { x = 536, y = 90 }
`;

/** A built-in, read-only flow. */
export const BUILTIN_TOML = `[flow]
version = 1
name = "turn-digest"
on = "turn.end"
enabled = false
start = ["failed"]
[flow.inputs]
status = "event.status"

[[node]]
id = "failed"
type = "expression"
condition = 'status == "failed"'
[node.inputs]
status = "event.status"
[node.next]
true = ["wake_cognition"]

[[node]]
id = "wake_cognition"
type = "action"
action = "wake"
[node.params]
note = "The last turn failed; look at what went wrong before going on."
`;

/** One recorded live run of guard-mutations on a `write` the Captain did not ask for. */
export function sampleRunRecord(digest: string): RunRecord {
  const pending = (id: string) => ({ id, status: "not_reached" as const, port: null, duration_ms: null, answer: null, result: null, variables: {}, batch_id: null, error: null });
  return {
    version: 1,
    run_id: "01JA7Q3M9ZK4V8R2N6T5W1XHCE",
    flow_name: "guard-mutations",
    definition_digest: digest,
    mode: "live",
    started_at: "2026-10-07T09:41:12Z",
    duration_ms: 614,
    budget_ms: 800,
    event: { trigger: "tool.pre_call", payload: { tool: "write", args: { path: "src/config.rs", content: "pub fn load(p: &Path, strict: bool) -> Config { … }" }, call_id: "call_7f3" } },
    identity: { model: "typesafe/jev-1.13", variant: null },
    inputs: {
      "session.agent": { value: "build", cut: false },
      "conversation.last_captain": { value: "Swap the toml parser, keep Config::load(p).", cut: false },
      "intent.record": { value: "Incremental TOML parser, public API unchanged.", cut: false },
    },
    nodes: [
      {
        id: "effect", status: "completed", port: "local_change", duration_ms: 318,
        answer: { type: "choice", choice: "local_change", probabilities: { read_only: 0.06, local_change: 0.81, network: 0.02, destructive: 0.11 }, confidence: 0.81 },
        result: null, variables: {}, batch_id: "b1", error: null,
      },
      { id: "asked_for", status: "completed", port: "no", duration_ms: 296, answer: { type: "noul", noul: 0.31 }, result: null, variables: {}, batch_id: "b2", error: null },
      pending("trusted_host"),
      pending("blast_radius"),
      { id: "plan_overreach", status: "completed", port: "true", duration_ms: 0, answer: null, result: true, variables: { THRESHOLD: 0.6 }, batch_id: null, error: null },
      pending("continue"),
      pending("ask"),
      { id: "deny", status: "completed", port: "then", duration_ms: 0, answer: null, result: null, variables: {}, batch_id: null, error: null },
      { id: "audit", status: "completed", port: null, duration_ms: 0, answer: null, result: null, variables: {}, batch_id: null, error: null },
    ],
    batches: [
      {
        id: "b1", node_ids: ["effect"],
        state: 'agent = "build"\nargs = {"content":"pub fn load(p: &Path, strict: bool) -> Config { … }","path":"src/config.rs"}\ntool = "write"',
        questions: { effect: { type: "choice", instructions: "What will this call do? Choose its most consequential effect.", criteria: { read_only: "Only reads files or prints information.", local_change: "Changes files inside the workspace.", network: "Reaches a host outside the machine.", destructive: "Deletes, force-pushes or rewrites history." } } },
        duration_ms: 318, usage: { input_tokens: 454, output_tokens: 30 }, cost_usd: null,
      },
      {
        id: "b2", node_ids: ["asked_for"],
        state: 'args = {"content":"pub fn load(p: &Path, strict: bool) -> Config { … }","path":"src/config.rs"}\nintent = "Incremental TOML parser, public API unchanged."\nlast_captain = "Swap the toml parser, keep Config::load(p)."',
        questions: { asked_for: { type: "noul", instructions: "Did the Captain or the intent record ask for this exact change?" } },
        duration_ms: 296, usage: { input_tokens: 358, output_tokens: 12 }, cost_usd: null,
      },
    ],
    status: "success",
    failure: null,
    actions: [
      { node_id: "deny", action: "deny", params: { note: "Change not requested, or destructive reach too broad. Propose it to the Captain." } },
      { node_id: "audit", action: "log", params: { text: "guard-mutations proposed denial" } },
    ],
    fallback_actions: [],
    usage: { input_tokens: 812, output_tokens: 42 },
    cost_usd: null,
    application: {
      status: "applied",
      actions: [
        { node_id: "audit", action: "log", status: "applied", message: null },
      ],
    },
  };
}

/** A live wake that the host held back during its cooldown. */
export function suppressedRunRecord(): RunRecord {
  return {
    ...sampleRunRecord("sha256:mock-turn-digest"),
    run_id: "01JA7Q3M9ZK4V8R2N6T5W1XHCF",
    flow_name: "turn-digest",
    budget_ms: null,
    event: { trigger: "turn.end", payload: { status: "failed" } },
    nodes: [
      { id: "failed", status: "completed", port: "true", duration_ms: 0, answer: null, result: true, variables: {}, batch_id: null, error: null },
      { id: "wake_cognition", status: "completed", port: null, duration_ms: 0, answer: null, result: null, variables: {}, batch_id: null, error: null },
    ],
    batches: [], duration_ms: 1, inputs: {}, usage: null,
    actions: [{ node_id: "wake_cognition", action: "wake", params: { note: "The last turn failed; look at what went wrong before going on." } }],
    application: { status: "suppressed", actions: [{ node_id: "wake_cognition", action: "wake", status: "suppressed", message: "wake cooldown" }] },
  };
}

/** Logging succeeds even when the wake in the same run is suppressed. */
export function mixedApplicationRunRecord(): RunRecord {
  const run = suppressedRunRecord();
  return {
    ...run,
    run_id: "01JA7Q3M9ZK4V8R2N6T5W1XHCG",
    nodes: [...run.nodes, { id: "audit", status: "completed", port: null, duration_ms: 0, answer: null, result: null, variables: {}, batch_id: null, error: null }],
    actions: [...run.actions, { node_id: "audit", action: "log", params: { text: "wake requested" } }],
    application: {
      status: "applied",
      actions: [...run.application.actions, { node_id: "audit", action: "log", status: "applied", message: null }],
    },
  };
}
