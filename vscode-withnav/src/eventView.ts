import type { HostToWebview, PermissionCard, PermissionEvidence, PauseToken } from "./chatProtocol";

/** Network-free projection shared by live SSE and durable replay. */
export interface WireEnvelope { seq: number; kind: string; payload: any }

export function permissionEvidence(value: any): PermissionEvidence | undefined {
  if (!value || typeof value.policy_version !== "number" ||
      !["instinct", "jev", "metacognition", "driver", "captain"].includes(value.source)) return undefined;
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
  const extra = (excluded: string[]): string => Object.entries(args ?? {})
    .filter(([key]) => !excluded.includes(key))
    .map(([key, value]) => key === "replace_all" && value === true
      ? "replace_all: true — replace every occurrence" : `${key}: ${JSON.stringify(value)}`).join("\n");
  if (tool === "bash") return field("command");
  if (tool === "write") return `${field("path")}${extra(["path", "content"]) ? `\n${extra(["path", "content"])}` : ""}\n\nContent\n${boundedContent(field("content"))}`;
  if (tool === "edit") return `${field("path")}${extra(["path", "old_string", "new_string"]) ? `\n${extra(["path", "old_string", "new_string"])}` : ""}\n\nOld\n${boundedContent(field("old_string"))}\n\nNew\n${boundedContent(field("new_string"))}`;
  return JSON.stringify(args, null, 2);
}

export function evidenceLabel(evidence: PermissionEvidence, context: "tool" | "permission", resolution?: string): string {
  if ((evidence.source === "driver" || evidence.source === "captain")) return context === "tool" || resolution === "allow_once" || resolution === "allow_folder"
    ? "approved · Captain" : "Captain review required";
  return (evidence.source === "instinct" || evidence.source === "jev") ? "auto · instinct" : "auto · metacognition";
}

/** Select from the latest response only: provider call ids may be reused. */
export class PermissionCallReplay {
  private latest: any[] = [];
  observe(envelope: WireEnvelope): void {
    if (envelope.kind === "llm.response") this.latest = Array.isArray(envelope.payload?.message?.tool_calls)
      ? envelope.payload.message.tool_calls : [];
  }
  argumentsFor(request: any): string | undefined {
    const matches = this.latest.filter(call => call.id === request.tool_call_id);
    if (matches.length !== 1 || matches[0].name !== request.tool) return undefined;
    const args = matches[0].args;
    if (!args || typeof args !== "object" || Array.isArray(args)) return undefined;
    const serialized = JSON.stringify(args);
    const reviewed = String(request.arguments ?? "");
    try {
      if (!jsonEqual(JSON.parse(reviewed), args)) return undefined;
    } catch {
      // Rust bound_output keeps a JSON head and tail around this exact marker.
      // serde_json sorts object keys; preserve that order for the wire comparison.
      const parts = reviewed.split("\n...[output elided]...\n");
      const canonical = JSON.stringify(sortedJson(args));
      if (parts.length !== 2 || !parts[0] || !parts[1] ||
          !canonical.startsWith(parts[0]) || !canonical.endsWith(parts[1])) return undefined;
    }
    return serialized;
  }
}

function sortedJson(value: any): any {
  if (Array.isArray(value)) return value.map(sortedJson);
  if (!value || typeof value !== "object") return value;
  return Object.fromEntries(Object.keys(value).sort().map(key => [key, sortedJson(value[key])]));
}

function jsonEqual(a: any, b: any): boolean {
  if (a === b) return true;
  if (!a || !b || typeof a !== "object" || typeof b !== "object" || Array.isArray(a) !== Array.isArray(b)) return false;
  const keys = Object.keys(a);
  return keys.length === Object.keys(b).length && keys.every(key => Object.hasOwn(b, key) && jsonEqual(a[key], b[key]));
}

function pauseToken(value: any): PauseToken | undefined {
  return typeof value?.incarnation === "string" && typeof value?.hold_id === "string"
    ? { incarnation: value.incarnation, hold_id: value.hold_id } : undefined;
}

export function eventView(envelope: WireEnvelope, currentTurn?: string, calls?: PermissionCallReplay): HostToWebview | undefined {
  const p = envelope.payload ?? {};
  switch (envelope.kind) {
    case "message.received": {
      const source = String(p.source ?? "");
      const author = source === "captain" || source === "driver" || source === "user:http" || source === "user:cli" ? "captain"
        : source === "nav" || source.startsWith("nav:") || source === "navigator" || source.startsWith("navigator:") ? "navigator" : "subagent";
      const selectedSkill = author === "captain" && typeof p.selected_skill?.name === "string"
        ? { name: p.selected_skill.name, origin: String(p.selected_skill.origin ?? "") } : undefined;
      return { type: "message", id: String(p.command_id ?? envelope.seq), author, source,
         text: String(p.content ?? ""), selectedSkill, answer: author === "captain" ? p.answer : undefined };
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
          (!p.child_session_id ? calls?.argumentsFor(p) : undefined) ?? String(p.arguments ?? "")),
        arguments: String(p.arguments ?? ""),
        allowFolder: agent !== "plan" && tool === "fetch",
        allowConfigSession: p.allow_config_session === true,
          skillName: typeof p.skill_script?.skill === "string" ? p.skill_script.skill : undefined,
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
       return { type: "question", id: String(p.turn_id ?? currentTurn ?? envelope.seq), text: String(p.question ?? ""), ...(p.question_call_id ? { questionCallId: p.question_call_id } : {}), ...(p.choices ? { choices: p.choices } : {}) };
    }
    case "command.result":
      if (p.name === "watchdog") return { type: "notice", text: String(p.data?.message ?? "Watchdog notice") };
      return undefined;
    default: return undefined;
  }
}

/** Authoritative open controls: webview ids are checked against this conversation's replay. */
export class ConversationControls {
  calls = new PermissionCallReplay();
  readonly permissions = new Map<string, PermissionCard>();
  readonly pauses = new Map<string, Extract<HostToWebview, { type: "pause" }>>();
  envelopeUpdates(envelope: WireEnvelope): HostToWebview[] {
    const p = envelope.payload ?? {};
    this.calls.observe(envelope);
    const results: HostToWebview[] = [];
    for (const [id, pause] of this.pauses) {
      const startedOrEnded = envelope.kind === "session.end" || envelope.kind === "turn.start" || (envelope.kind === "turn.end" && p.outcome !== "yielded");
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
  reset(): void { this.permissions.clear(); this.pauses.clear(); this.calls = new PermissionCallReplay(); }
}

export function permissionOutcome(resolution: string, message?: string): string {
  switch (resolution) {
    case "allow_once": return "Allowed once";
    case "allow_folder": return "Allowed for this scope";
    case "allow_config_session": return "Allowed WithNav config for this session";
    case "allow_skill_machine": return "Allowed this user skill's script on this machine";
    case "deny": return message ? `Denied: ${message}` : "Denied";
    case "expired": return "Expired: the session restarted";
    case "cancelled": return "Cancelled";
    case "stale": return "No longer waiting: this request was answered, cancelled or expired.";
    case "pending": return "Answer sent · waiting for the server";
    case "error": return message ?? "Could not send the answer. Try again.";
    default: return resolution;
  }
}
