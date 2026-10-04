import type { HostToWebview, PermissionCard, PermissionEvidence, PauseToken } from "./chatProtocol";

/** Network-free projection shared by live SSE and durable replay. */
export interface WireEnvelope { seq: number; kind: string; payload: any }

export function permissionEvidence(value: any): PermissionEvidence | undefined {
  if (!value || typeof value.policy_version !== "number" ||
      !["jev", "metacognition", "driver"].includes(value.source)) return undefined;
  const result: PermissionEvidence = { policy_version: value.policy_version, source: value.source };
  if (typeof value.reason === "string") result.reason = value.reason;
  for (const key of ["verdicts", "overrides", "models"] as const) {
    if (value[key] !== undefined) result[key] = value[key];
  }
  if (typeof value.latency_ms === "number") result.latency_ms = value.latency_ms;
  if (typeof value.truncated === "boolean") result.truncated = value.truncated;
  return result;
}

/** Bound file contents by UTF-8 bytes without splitting a character. Commands stay whole. */
export function boundedContent(text: string, limit = 4096): string {
  const encoder = new TextEncoder();
  const total = encoder.encode(text).length;
  if (total <= limit) return text;
  let bytes = 0;
  let shown = "";
  for (const char of text) {
    const size = encoder.encode(char).length;
    if (bytes + size > limit) break;
    bytes += size;
    shown += char;
  }
  return `${shown}\n(${total - bytes} bytes omitted)`;
}

export function permissionCall(tool: string, argumentsText: string): string {
  let args: any;
  try { args = JSON.parse(argumentsText); }
  catch { return `${argumentsText}\n(Arguments shortened by the server; full call unavailable.)`; }
  const field = (key: string): string => typeof args?.[key] === "string" ? args[key] : "(not provided)";
  if (tool === "bash") return field("command");
  if (tool === "write") return `${field("path")}\n\nContent\n${boundedContent(field("content"))}`;
  if (tool === "edit") return `${field("path")}\n\nOld\n${boundedContent(field("old_string"))}\n\nNew\n${boundedContent(field("new_string"))}`;
  return JSON.stringify(args, null, 2);
}

function pauseToken(value: any): PauseToken | undefined {
  return typeof value?.incarnation === "string" && typeof value?.hold_id === "string"
    ? { incarnation: value.incarnation, hold_id: value.hold_id } : undefined;
}

export function eventView(envelope: WireEnvelope, currentTurn?: string, calls?: ReadonlyMap<string, unknown>): HostToWebview | undefined {
  const p = envelope.payload ?? {};
  switch (envelope.kind) {
    case "message.received": {
      const source = String(p.source ?? "");
      const author = source === "user:http" || source === "user:cli" ? "driver"
        : source === "navigator" || source.startsWith("navigator:") ? "navigator" : "subagent";
      const selectedSkill = author === "driver" && typeof p.selected_skill?.name === "string"
        ? { name: p.selected_skill.name, origin: String(p.selected_skill.origin ?? "") } : undefined;
      return { type: "message", id: String(p.command_id ?? envelope.seq), author, source,
        text: String(p.content ?? ""), selectedSkill };
    }
    case "permission.requested": {
      if (typeof p.request_id !== "string") return undefined;
      const tool = String(p.tool ?? "tool");
      const agent = String(p.agent ?? "agent");
      return { type: "permission", card: {
        requestId: p.request_id, callId: String(p.tool_call_id ?? ""), tool, agent,
        childSessionId: typeof p.child_session_id === "string" ? p.child_session_id : undefined,
        scope: { kind: String(p.scope?.kind ?? ""), value: String(p.scope?.value ?? "") },
        reason: String(p.reason ?? ""), call: permissionCall(tool,
          !p.child_session_id && calls?.has(p.tool_call_id) ? JSON.stringify(calls.get(p.tool_call_id)) : String(p.arguments ?? "")),
        allowFolder: agent !== "plan" && !["bash", "write", "edit"].includes(tool),
        evidence: permissionEvidence(p.permission_gate),
      } };
    }
    case "permission.resolved":
      return typeof p.request_id === "string" ? { type: "permissionState", requestId: p.request_id,
        resolution: String(p.resolution ?? "expired"), message: typeof p.message === "string" ? p.message : undefined } : undefined;
    case "agent.yield": {
      const id = `pause:${envelope.seq}`;
      const token = pauseToken(p.details?.pause_token);
      if (p.kind === "budget") return { type: "pause", id, kind: "budget", text: "Out of steps", token };
      if (p.kind === "watchdog") return { type: "pause", id, kind: "watchdog",
        text: `Paused by the watchdog: ${String(p.question ?? p.details?.reason ?? "")}`, token };
      return { type: "question", id: String(p.turn_id ?? currentTurn ?? envelope.seq), text: String(p.question ?? "") };
    }
    case "command.result":
      if (p.name === "watchdog") return { type: "notice", text: String(p.data?.message ?? "Watchdog notice") };
      return undefined;
    default: return undefined;
  }
}

/** Authoritative open controls: webview ids are checked against this conversation's replay. */
export class ConversationControls {
  readonly calls = new Map<string, unknown>();
  readonly permissions = new Map<string, PermissionCard>();
  readonly pauses = new Map<string, Extract<HostToWebview, { type: "pause" }>>();
  envelopeUpdates(envelope: WireEnvelope): HostToWebview[] {
    const p = envelope.payload ?? {};
    if (envelope.kind === "llm.response" && Array.isArray(p.message?.tool_calls)) {
      for (const call of p.message.tool_calls) {
        if (typeof call?.id === "string" && call.args && typeof call.args === "object") this.calls.set(call.id, call.args);
      }
    }
    const results: HostToWebview[] = [];
    for (const [id, pause] of this.pauses) {
      const startedOrEnded = envelope.kind === "turn.start" || (envelope.kind === "turn.end" && p.outcome !== "yielded");
      const watchdogResolved = envelope.kind === "command.result" && p.name === "watchdog" &&
        typeof p.data?.resolution === "string" && pause.token &&
        pause.token.incarnation === p.data?.pause_token?.incarnation && pause.token.hold_id === p.data?.pause_token?.hold_id;
      if (startedOrEnded || watchdogResolved) results.push({ type: "pauseState", id, state: "resolved", message: watchdogResolved ? p.data.message : "No longer paused" });
    }
    return results;
  }
  apply(message: HostToWebview): void {
    if (message.type === "permission") this.permissions.set(message.card.requestId, message.card);
    if (message.type === "permissionState" && !["pending", "error"].includes(message.resolution)) this.permissions.delete(message.requestId);
    if (message.type === "pause") this.pauses.set(message.id, message);
    if (message.type === "pauseState" && message.state === "resolved") this.pauses.delete(message.id);
  }
  reset(): void { this.permissions.clear(); this.pauses.clear(); this.calls.clear(); }
}

export function permissionOutcome(resolution: string, message?: string): string {
  switch (resolution) {
    case "allow_once": return "Allowed once";
    case "allow_folder": return "Allowed for this scope";
    case "deny": return message ? `Denied: ${message}` : "Denied";
    case "expired": return "Expired: the session restarted";
    case "cancelled": return "Cancelled";
    case "stale": return "No longer waiting: this request was answered, cancelled or expired.";
    case "pending": return "Answer sent · waiting for the server";
    case "error": return message ?? "Could not send the answer. Try again.";
    default: return resolution;
  }
}
