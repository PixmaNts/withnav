// Projection of a session's event stream into what the transcript shows.
//
// One pure reducer handles history replay and live events alike: the server
// replays durable events first on every (re)connection, then streams live ones,
// so a reopened conversation and a running one reach the same state. Replay
// carries no `llm.delta` (it is live-only), so `llm.response` is authoritative
// for a step's text and reasoning; deltas only fill it in while it streams.

import type { ContextSnapshot, Envelope } from "../api/types";
import { outcomeText } from "./outcome";
import { parseWake, type WakeNote } from "./wake";

export type ToolStatus = "running" | "ok" | "error" | "denied" | "interrupted";

export interface ToolCall {
  id: string;
  name: string;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  args: any;
  status: ToolStatus;
  output?: string;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  metadata?: any;
}

export type TurnPart =
  | { type: "reasoning"; stepId: string }
  | { type: "text"; stepId: string; text: string }
  | { type: "tools"; calls: string[] }
  | { type: "permission"; requestId: string }
  | { type: "question"; text: string };

export interface Turn {
  kind: "turn";
  key: string;
  turnId: string;
  /** The task (Captain message) the turn works on. */
  taskId: string;
  /** The agent in force when the turn started, when the stream said so. */
  agent: string | null;
  parts: TurnPart[];
  reasoning: Record<string, string>;
  status: "running" | "done" | "error" | "interrupted" | "yielded";
  error?: string;
  durationMs?: number;
}

export interface Permission {
  requestId: string;
  toolCallId: string;
  tool: string;
  agent: string;
  reason: string;
  scope: { kind: string; value: string };
  arguments: string;
  childSessionId?: string;
  /** `allow_once` | `allow_folder` | `deny` | `cancelled` | `expired`, absent while waiting. */
  resolution?: string;
  message?: string;
}

export type Item =
  | { kind: "captain"; key: string; text: string; skill?: string }
  | { kind: "wake"; key: string; text: string; note: WakeNote }
  | { kind: "beacon"; key: string; text: string }
  | { kind: "subagent-result"; key: string; text: string; source: string; agent?: string; outcome?: string; childId?: string }
  | Turn
  /** A permission asked outside any turn shown here (a stream cut mid-turn). */
  | { kind: "permission"; key: string; requestId: string }
  | { kind: "notice"; key: string; text: string; tone: "info" | "error" };

export interface Child {
  id: string;
  agent: string;
  goal: string;
  toolCallId: string;
  delivery?: string;
  status: "running" | "done" | "failed" | "stopped" | "doubt";
  reason?: string;
  spawnedAt: string;
  endedAt?: string;
  spawnedBy: string;
}

export type Activity = "idle" | "running" | "stopped";

export interface SessionView {
  bypass: boolean;
  bypassSeq: number;
  items: Item[];
  tools: Record<string, ToolCall>;
  permissions: Record<string, Permission>;
  children: Record<string, Child>;
  activity: Activity;
  agent: string | null;
  model: string | null;
  variant: string | null;
  context: ContextSnapshot | null;
  lastSeq: number | null;
  currentTurn: string | null;
  /** A turn parked for the owner: out of steps, or held by the watchdog. */
  pause: { kind: "budget" | "watchdog"; token?: { incarnation: string; hold_id: string } } | null;
  /** The workspace from `session.start`. */
  workspace: string | null;
  startedAt: string | null;
  endedAt: string | null;
}

export const emptyView = (): SessionView => ({
  bypass: false,
  bypassSeq: -1,
  items: [],
  tools: {},
  permissions: {},
  children: {},
  activity: "idle",
  agent: null,
  model: null,
  variant: null,
  context: null,
  lastSeq: null,
  currentTurn: null,
  pause: null,
  workspace: null,
  startedAt: null,
  endedAt: null,
});

const str = (value: unknown, fallback = ""): string => (typeof value === "string" ? value : fallback);

export function authorOf(source: string): "captain" | "navigator" | "subagent" {
  if (source === "captain" || source === "driver" || source.startsWith("captain:") || source.startsWith("user:") && source !== "user:subagent") return "captain";
  if (source === "nav" || source.startsWith("nav:") || source === "navigator" || source.startsWith("navigator:")) return "navigator";
  return "subagent";
}

/** Applies one envelope. Returns the same object when nothing changed. */
export function reduce(view: SessionView, envelope: Envelope): SessionView {
  // An event at or before the cursor was already applied (a reconnect overlap).
  if (view.lastSeq !== null && envelope.seq <= view.lastSeq) return view;
  const next: SessionView = { ...view, lastSeq: envelope.seq };
  const p = envelope.payload ?? {};
  switch (envelope.kind) {
    case "session.start":
      next.workspace = str(p.workspace) || next.workspace;
      next.startedAt = envelope.occurred_at;
      next.activity = "idle";
      return next;
    case "session.end":
      if (envelope.seq > next.bypassSeq) {
        next.bypass = false;
        next.bypassSeq = envelope.seq;
      }
      next.activity = "stopped";
      next.endedAt = envelope.occurred_at;
      next.currentTurn = null;
      next.pause = null;
      next.items = next.items.map((item) => (item.kind === "turn" && item.status === "running" ? { ...item, status: "interrupted" } : item));
      next.permissions = mapRecord(next.permissions, (perm) => (perm.resolution ? perm : { ...perm, resolution: "cancelled" }));
      next.tools = mapRecord(next.tools, (call) => (call.status === "running" ? { ...call, status: "interrupted" } : call));
      return next;
    case "message.received": {
      const source = str(p.source);
      const text = str(p.content);
      const key = `m:${envelope.seq}`;
      const author = authorOf(source);
      if (source === "metacognition" || source === "metacognition:remark") {
        next.items = [...next.items, { kind: "beacon", key, text }];
      } else if (author === "captain") {
        const skill = ["captain", "driver"].includes(p.selected_skill?.origin) ? str(p.selected_skill?.name) || undefined : undefined;
        next.items = [...next.items, { kind: "captain", key, text, skill }];
      } else if (author === "navigator") next.items = [...next.items, { kind: "wake", key, text, note: parseWake(text) }];
      else next.items = [...next.items, { kind: "subagent-result", key, source, ...subagentResult(text, p) }];
      return next;
    }
    case "turn.start": {
      const turnId = str(p.turn_id, `t:${envelope.seq}`);
      const taskId = str(p.task_id);
      next.currentTurn = turnId;
      next.activity = "running";
      next.pause = null;
      // A turn that yielded for a permission resumes as a new turn of the same
      // task once the Captain answers: it continues the same block.
      const last = next.items[next.items.length - 1];
      if (last?.kind === "turn" && last.status === "yielded" && taskId && last.taskId === taskId) {
        next.items = [...next.items.slice(0, -1), { ...last, turnId, status: "running" }];
        return next;
      }
      next.items = [
        ...next.items,
        { kind: "turn", key: `turn:${turnId}`, turnId, taskId, agent: next.agent, parts: [], reasoning: {}, status: "running" },
      ];
      return next;
    }
    case "llm.delta": {
      const delta = p.delta ?? {};
      const stepId = str(p.step_id);
      if (delta.delta_type === "text") return updateTurn(next, str(p.turn_id), (turn) => appendText(turn, stepId, str(delta.text)));
      if (delta.delta_type === "thinking")
        return updateTurn(next, str(p.turn_id), (turn) =>
          setReasoning(turn, stepId, (turn.reasoning[stepId] ?? "") + str(delta.text)),
        );
      return next;
    }
    case "llm.response": {
      const message = p.message ?? {};
      const stepId = str(p.step_id);
      return updateTurn(next, str(p.turn_id), (turn) => {
        const out = setText(turn, stepId, str(message.text));
        const thinking = str(message.thinking);
        return thinking ? setReasoning(out, stepId, thinking) : out;
      });
    }
    case "tool.pre_call": {
      const id = str(p.tool_call_id, `c:${envelope.seq}`);
      next.tools = { ...next.tools, [id]: { id, name: str(p.name, "tool"), args: p.args, status: "running" } };
      return updateTurn(next, next.currentTurn ?? "", (turn) => {
        const last = turn.parts[turn.parts.length - 1];
        if (last?.type === "tools") {
          if (last.calls.includes(id)) return turn;
          return { ...turn, parts: [...turn.parts.slice(0, -1), { type: "tools", calls: [...last.calls, id] }] };
        }
        return { ...turn, parts: [...turn.parts, { type: "tools", calls: [id] }] };
      });
    }
    case "tool.post_call": {
      const id = str(p.tool_call_id, `c:${envelope.seq}`);
      const existing = next.tools[id] ?? { id, name: str(p.name, "tool"), args: undefined, status: "running" as ToolStatus };
      const status: ToolStatus = p.status === "ok" ? "ok" : p.status === "denied" ? "denied" : p.status === "interrupted" ? "interrupted" : "error";
      next.tools = { ...next.tools, [id]: { ...existing, status, output: str(p.output), metadata: p.metadata } };
      return next;
    }
    case "turn.end": {
      const turnId = str(p.turn_id, next.currentTurn ?? "");
      const outcome = str(p.outcome);
      const status: Turn["status"] =
        outcome === "error" ? "error" : outcome === "interrupted" ? "interrupted" : outcome === "yielded" ? "yielded" : "done";
      const error = p.error ? str(p.error.message, JSON.stringify(p.error)) : undefined;
      next.currentTurn = null;
      // A turn yielded to a waiting permission is not over for the Captain.
      const waiting = status === "yielded" && Object.values(next.permissions).some((perm) => !perm.resolution);
      next.activity = next.activity === "stopped" ? "stopped" : waiting ? "running" : "idle";
      const updated = updateTurn(next, turnId, (turn) => ({ ...turn, status, error, durationMs: typeof p.duration_ms === "number" ? p.duration_ms : undefined }));
      updated.tools = mapRecord(updated.tools, (call) => (call.status === "running" && status !== "done" && status !== "yielded" ? { ...call, status: "interrupted" } : call));
      return updated;
    }
    case "agent.state":
      if (p.actor_id && p.actor_id !== "agent:main") return next;
      // `yielded` (waiting on the Captain) leaves the activity turn.end set.
      if (p.to === "running") next.activity = "running";
      else if (p.to === "idle") next.activity = next.activity === "stopped" ? "stopped" : "idle";
      return next;
    case "agent.yield": {
      if (p.kind === "budget" || p.kind === "watchdog") {
        const token = p.details?.pause_token;
        next.pause = {
          kind: p.kind,
          token: typeof token?.incarnation === "string" && typeof token?.hold_id === "string" ? { incarnation: token.incarnation, hold_id: token.hold_id } : undefined,
        };
        return notice(
          next,
          envelope.seq,
          p.kind === "budget" ? "Paused: this turn used all its steps." : `Paused by the watchdog: ${str(p.question, str(p.details?.reason))}`,
          "info",
        );
      }
      return updateTurn(next, str(p.turn_id, next.currentTurn ?? ""), (turn) => ({
        ...turn,
        parts: [...turn.parts, { type: "question", text: str(p.question) }],
      }));
    }
    case "permission.requested": {
      const requestId = str(p.request_id);
      if (!requestId || next.permissions[requestId]) return next;
      next.permissions = {
        ...next.permissions,
        [requestId]: {
          requestId,
          toolCallId: str(p.tool_call_id),
          tool: str(p.tool, "tool"),
          agent: str(p.agent, "build"),
          reason: str(p.reason),
          scope: { kind: str(p.scope?.kind), value: str(p.scope?.value) },
          arguments: str(p.arguments),
          childSessionId: typeof p.child_session_id === "string" ? p.child_session_id : undefined,
        },
      };
      // Shown where it happened: inside the running turn, else on its own.
      const running = next.currentTurn;
      if (running && next.items.some((item) => item.kind === "turn" && item.turnId === running))
        return updateTurn(next, running, (turn) => ({ ...turn, parts: [...turn.parts, { type: "permission", requestId }] }));
      next.activity = next.activity === "stopped" ? "stopped" : "running";
      next.items = [...next.items, { kind: "permission", key: `perm:${requestId}`, requestId }];
      return next;
    }
    case "permission.resolved": {
      const requestId = str(p.request_id);
      const perm = next.permissions[requestId];
      if (!perm) return next;
      next.permissions = {
        ...next.permissions,
        [requestId]: { ...perm, resolution: str(p.resolution, "expired"), message: typeof p.message === "string" ? p.message : undefined },
      };
      return next;
    }
    case "model.changed": {
      const identity = p.new ?? {};
      if (typeof identity.model === "string") next.model = identity.model;
      if (typeof identity.agent === "string") next.agent = identity.agent;
      next.variant = typeof p.variant === "string" ? p.variant : null;
      if (p.reason === "initial" || p.reason === "resume-refresh") return next;
      const what = p.reason === "variant" ? `Variant: ${next.variant ?? "off"}` : p.reason === "agent" ? `Agent: ${next.agent} · ${next.model}` : `Model: ${next.model}${next.variant ? ` · ${next.variant}` : ""}`;
      return notice(next, envelope.seq, what, "info");
    }
    case "session.bypass": {
      if (typeof p.enabled !== "boolean") return next;
      if (envelope.seq > next.bypassSeq) {
        next.bypass = p.enabled;
        next.bypassSeq = envelope.seq;
      }
      return str(p.by) === "system:resume" ? notice(next, envelope.seq, bypassNotice(p.enabled, str(p.by)), "info") : next;
    }
    case "session.agent_changed": {
      // Turns before the first change ran as `previous`: label them now.
      const previous = str(p.previous);
      if (previous) next.items = next.items.map((item) => (item.kind === "turn" && item.agent === null ? { ...item, agent: previous } : item));
      next.agent = str(p.agent, next.agent ?? "");
      return next;
    }
    case "context.updated":
      if (typeof p.window === "number" && typeof p.percent === "number") {
        const current = next.context;
        // Revisions only order snapshots of one incarnation; a new one replaces.
        if (current && current.incarnation === p.incarnation && current.revision > p.revision) return next;
        next.context = p as ContextSnapshot;
      }
      return next;
    case "context.compacted":
      return notice(next, envelope.seq, `Context compacted: about ${tokens(p.estimate_before)} → ${tokens(p.estimate_after)} tokens`, "info");
    case "command.result": {
      // Selection switches already show up as `model.changed`.
      const text = outcomeText(str(p.name), p.ok !== false, p.data, typeof p.error === "string" ? p.error : undefined);
      if (text === null) return next;
      return notice(next, envelope.seq, `/${str(p.name, "command")} · ${text}`, p.ok === false ? "error" : "info");
    }
    case "subagent.spawned": {
      const id = str(p.child_session_id);
      if (!id) return next;
      next.children = {
        ...next.children,
        [id]: {
          id,
          agent: str(p.agent, "subagent"),
          goal: str(p.goal),
          toolCallId: str(p.tool_call_id),
          delivery: typeof p.delivery === "string" ? p.delivery : undefined,
          status: "running",
          spawnedAt: envelope.occurred_at,
          spawnedBy: next.agent ?? "build",
        },
      };
      return next;
    }
    case "subagent.done": {
      const id = str(p.child_session_id);
      const child = next.children[id];
      const outcome = str(p.outcome);
      const status: Child["status"] = outcome === "done" ? "done" : outcome === "failed" ? "failed" : "stopped";
      next.children = {
        ...next.children,
        [id]: {
          ...(child ?? { id, agent: str(p.agent, "subagent"), goal: "", toolCallId: "", spawnedAt: envelope.occurred_at, spawnedBy: next.agent ?? "build" }),
          status,
          reason: typeof p.reason === "string" ? p.reason : undefined,
          endedAt: envelope.occurred_at,
        },
      };
      return next;
    }
    case "subagent.watchdog": {
      const id = str(p.child_session_id);
      const child = next.children[id];
      if (!child || child.status !== "running" && child.status !== "doubt") return next;
      const verdict = str(p.verdict);
      next.children = { ...next.children, [id]: { ...child, status: verdict === "doubt" ? "doubt" : verdict === "active" ? "running" : child.status } };
      return next;
    }
    default:
      return next;
  }
}

/**
 * A child's result reaches the parent as a message whose content is JSON
 * (`{agent, outcome, reason, result, untrusted_subagent_output}`); older or
 * other senders may send plain text, shown as is.
 */
// eslint-disable-next-line @typescript-eslint/no-explicit-any
function subagentResult(text: string, p: any): { text: string; agent?: string; outcome?: string; childId?: string } {
  const out = { text, agent: typeof p.agent === "string" ? p.agent : undefined, outcome: typeof p.outcome === "string" ? p.outcome : undefined, childId: typeof p.child_session_id === "string" ? p.child_session_id : undefined };
  try {
    const value = JSON.parse(text);
    if (value && typeof value === "object" && typeof value.result === "string")
      return { ...out, text: value.result || (typeof value.reason === "string" ? value.reason : ""), agent: out.agent ?? value.agent, outcome: out.outcome ?? value.outcome };
  } catch {
    /* plain text */
  }
  return out;
}

export function reduceAll(view: SessionView, envelopes: Envelope[]): SessionView {
  return envelopes.reduce(reduce, view);
}

/** The transcript line for a bypass change, in the TUI's words; `by` is the server's origin tag. */
function bypassNotice(enabled: boolean, by: string): string {
  if (by === "system:resume") return "Bypass off: the session was reopened";
  const from = by.startsWith("parent:") ? " (from the parent session)" : by === "user:cli" ? " (nav run --yes)" : "";
  return enabled
    ? `Bypass on${from}: ordinary permission prompts are skipped; protected requests still ask`
    : `Bypass off${from}`;
}

function notice(view: SessionView, seq: number, text: string, tone: "info" | "error"): SessionView {
  return { ...view, items: [...view.items, { kind: "notice", key: `n:${seq}`, text, tone }] };
}

function mapRecord<T>(record: Record<string, T>, f: (value: T) => T): Record<string, T> {
  let changed = false;
  const out: Record<string, T> = {};
  for (const [id, value] of Object.entries(record)) {
    const mapped = f(value);
    if (mapped !== value) changed = true;
    out[id] = mapped;
  }
  return changed ? out : record;
}

/** Reasoning shows before the step's text: its part is placed there once. */
function setReasoning(turn: Turn, stepId: string, text: string): Turn {
  const reasoning = { ...turn.reasoning, [stepId]: text };
  if (turn.parts.some((part) => part.type === "reasoning" && part.stepId === stepId)) return { ...turn, reasoning };
  const at = turn.parts.findIndex((part) => part.type === "text" && part.stepId === stepId);
  const parts = turn.parts.slice();
  parts.splice(at === -1 ? parts.length : at, 0, { type: "reasoning", stepId });
  return { ...turn, reasoning, parts };
}

/** Applies `f` to the turn `turnId`, or to the last turn when the id is unknown. */
function updateTurn(view: SessionView, turnId: string, f: (turn: Turn) => Turn): SessionView {
  let index = -1;
  for (let i = view.items.length - 1; i >= 0; i--) {
    const item = view.items[i]!;
    if (item.kind === "turn" && (item.turnId === turnId || !turnId)) {
      index = i;
      break;
    }
  }
  if (index === -1) {
    // A step whose turn.start was not seen (a stream cut mid-turn): open one.
    if (!turnId) return view;
    const turn: Turn = { kind: "turn", key: `turn:${turnId}`, turnId, taskId: "", agent: view.agent, parts: [], reasoning: {}, status: "running" };
    return { ...view, items: [...view.items, f(turn)] };
  }
  const turn = view.items[index] as Turn;
  const updated = f(turn);
  if (updated === turn) return view;
  const items = view.items.slice();
  items[index] = updated;
  return { ...view, items };
}

function appendText(turn: Turn, stepId: string, text: string): Turn {
  if (!text) return turn;
  const last = turn.parts[turn.parts.length - 1];
  if (last?.type === "text" && last.stepId === stepId)
    return { ...turn, parts: [...turn.parts.slice(0, -1), { ...last, text: last.text + text }] };
  return { ...turn, parts: [...turn.parts, { type: "text", stepId, text }] };
}

/** The step's final text replaces what streamed; a step never seen streaming is appended. */
function setText(turn: Turn, stepId: string, text: string): Turn {
  const index = turn.parts.findIndex((part) => part.type === "text" && part.stepId === stepId);
  if (index === -1) return text ? { ...turn, parts: [...turn.parts, { type: "text", stepId, text }] } : turn;
  const parts = turn.parts.slice();
  if (text) parts[index] = { type: "text", stepId, text };
  else parts.splice(index, 1);
  return { ...turn, parts };
}

export function tokens(n: unknown): string {
  if (typeof n !== "number" || !Number.isFinite(n)) return "?";
  if (n >= 999_500) return `${Math.round(n / 100_000) / 10}M`;
  if (n >= 1000) return `${Math.round(n / 1000)}k`;
  return String(n);
}

/** The permissions still waiting for an answer. */
export function openPermissions(view: SessionView): Permission[] {
  return Object.values(view.permissions).filter((perm) => !perm.resolution);
}
