import * as vscode from "vscode";
import { ChatBackend, HostToWebview } from "./chatProtocol";
import type { ServerInfoParams } from "./protocol";

/** One SSE frame off the event stream: the seq id plus the parsed envelope. */
interface EventFrame {
  id: string;
  event: string;
  envelope: any;
}

/** The wire `EventEnvelope` this bridge maps onto webview messages. */
interface WireEnvelope {
  seq: number;
  kind: string;
  actor_id: string;
  payload: any;
}

const ROOT_ACTOR = "agent:main";
const RECONNECT_DELAY_MS = 500;

/**
 * Extension-host bridge to the cognition: the language server announces the
 * in-process session server (`navigator/serverInfo`), this class creates a
 * navigator-profile session on first use, posts the driver's messages and
 * maps the SSE event stream onto the webview's message vocabulary. All
 * networking lives here; the webview never sees HTTP.
 */
export class CognitionBackend implements ChatBackend {
  private endpoint?: ServerInfoParams;
  private listeners = new Set<(message: HostToWebview) => void>();
  private transcript: HostToWebview[] = [];
  private session?: string;
  /** Last delivered seq, sent back as `Last-Event-ID` on reconnect. */
  private lastSeq?: string;
  /** Envelope seqs already delivered, so a resync's fresh full replay does
   *  not duplicate messages already in the transcript. */
  private delivered = new Set<number>();
  private currentTurn?: string;
  private streaming = false;

  setServerInfo(info: ServerInfoParams): void {
    // A restart of the language server publishes a NEW endpoint: the old
    // session and stream belong to a dead server, so drop them. The next
    // send creates a fresh session against the new port.
    if (this.endpoint && (this.endpoint.port !== info.port || this.endpoint.token !== info.token)) {
      this.session = undefined;
      this.lastSeq = undefined;
      this.delivered.clear();
      this.currentTurn = undefined;
      this.emit({ type: "status", state: "connecting" });
    }
    this.endpoint = info;
    if (this.session) void this.connect();
  }

  history(): HostToWebview[] {
    return [...this.transcript];
  }

  subscribe(listener: (message: HostToWebview) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  async send(text: string): Promise<void> {
    // The driver's message arrives as `message.received` on the stream; do
    // NOT emit an optimistic copy here (that duplicated it).
    const session = await this.ensureSession();
    await this.request(`/v1/sessions/${session}/messages`, {
      method: "POST",
      body: JSON.stringify({
        target_actor_id: ROOT_ACTOR,
        content: [{ type: "text", text }],
      }),
    });
  }

  async stop(): Promise<void> {
    if (!this.session || !this.endpoint) return;
    const session = this.session;
    this.session = undefined;
    this.lastSeq = undefined;
    this.delivered.clear();
    await this.request(`/v1/sessions/${session}/stop`, { method: "POST" });
    // A stopped session accepts no messages; the next send starts fresh.
    this.emit({ type: "status", state: "ready" });
  }

  // ---- session lifecycle ----

  private async ensureSession(): Promise<string> {
    if (this.session) return this.session;
    this.emit({ type: "status", state: "connecting" });
    while (!this.endpoint) {
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    const workspace = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? process.cwd();
    const created = await this.request("/v1/sessions", {
      method: "POST",
      body: JSON.stringify({ workspace, profile: "navigator" }),
    });
    const sessionId = String(created?.session_id ?? "");
    if (!sessionId) throw new Error("cognition server: create session returned no id");
    this.session = sessionId;
    this.delivered.clear();
    this.emit({ type: "status", state: "ready" });
    void this.connect();
    return this.session;
  }

  // ---- SSE stream ----

  private async connect(): Promise<void> {
    if (!this.endpoint || !this.session || this.streaming) return;
    this.streaming = true;
    const endpoint = this.endpoint;
    const session = this.session;
    try {
      const headers: Record<string, string> = {
        authorization: `Bearer ${endpoint.token}`,
      };
      if (this.lastSeq !== undefined) headers["last-event-id"] = this.lastSeq;
      const response = await fetch(
        `http://127.0.0.1:${endpoint.port}/v1/sessions/${session}/events`,
        { headers },
      );
      if (!response.ok || !response.body) {
        throw new Error(`event stream: HTTP ${response.status}`);
      }
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = "";
      for (;;) {
        const part = await reader.read();
        if (part.done) break;
        buffer += decoder.decode(part.value, { stream: true });
        let split: number;
        while ((split = buffer.indexOf("\n\n")) !== -1) {
          const [frame, rest] = [buffer.slice(0, split), buffer.slice(split + 2)];
          buffer = rest;
          this.handleFrame(this.parseFrame(frame));
        }
      }
    } catch (error) {
      this.emit({ type: "status", state: "error", message: this.errorText(error) });
    }
    this.streaming = false;
    // The stream ended (server drain or drop): reconnect with the cursor,
    // unless the session is gone (stop / server restart).
    if (this.session === session) {
      setTimeout(() => void this.connect(), RECONNECT_DELAY_MS);
    }
  }

  /** Parses one SSE frame: `id:`, optional `event:`, joined `data:` lines. */
  private parseFrame(raw: string): EventFrame | undefined {
    let id: string | undefined;
    let event: string | undefined;
    const data: string[] = [];
    for (const line of raw.split("\n")) {
      if (line.startsWith("id:")) id = line.slice(3).trim();
      else if (line.startsWith("event:")) event = line.slice(6).trim();
      else if (line.startsWith("data:")) data.push(line.slice(5).trimStart());
    }
    if (id === undefined && data.length === 0) return undefined;
    let envelope: any;
    try {
      envelope = data.length ? JSON.parse(data.join("\n")) : undefined;
    } catch {
      envelope = undefined;
    }
    return { id: id ?? "", event: event ?? "", envelope };
  }

  private handleFrame(frame: EventFrame | undefined): void {
    if (!frame) return;
    if (frame.id) this.lastSeq = frame.id;
    if (frame.event === "resync") {
      // The cursor is out of range: drop it, rebuild the view from what the
      // bridge already holds, and let the reconnect's full replay fill any
      // gap (already-delivered seqs are skipped below).
      this.lastSeq = undefined;
      this.emit({ type: "history", items: this.transcript.filter((m) => m.type !== "history") });
      return;
    }
    if (!frame.envelope) return;
    const seq = frame.envelope.seq;
    if (typeof seq === "number") {
      if (this.delivered.has(seq)) return;
      this.delivered.add(seq);
    }
    const message = this.mapEvent(frame.envelope);
    if (message) this.emit(message);
  }

  // ---- event -> webview mapping ----

  private mapEvent(envelope: WireEnvelope): HostToWebview | undefined {
    const payload = envelope.payload ?? {};
    switch (envelope.kind) {
      case "message.received": {
        const author = String(payload.source ?? "").startsWith("user:") ? "driver" : "navigator";
        return {
          type: "message",
          id: String(payload.command_id ?? envelope.seq),
          author,
          text: String(payload.content ?? ""),
        };
      }
      case "turn.start": {
        this.currentTurn = String(payload.turn_id ?? envelope.seq);
        this.emit({ type: "status", state: "busy" });
        return { type: "turnStart", id: this.currentTurn };
      }
      case "llm.delta": {
        // One turn groups its deltas, tools and turnEnd under one id.
        const id = String(payload.turn_id ?? this.currentTurn ?? envelope.seq);
        const delta = payload.delta ?? {};
        if (delta.delta_type === "text") {
          return { type: "delta", id, text: String(delta.text ?? "") };
        }
        if (delta.delta_type === "thinking") {
          return { type: "reasoning", id, text: String(delta.text ?? "") };
        }
        return undefined; // tool_call deltas ride the tool events instead
      }
      case "tool.pre_call": {
        const id = this.currentTurn ?? String(envelope.seq);
        const callId = String(payload.tool_call_id ?? envelope.seq);
        const name = String(payload.name ?? "tool");
        const summary = toolSummary(name, payload.args);
        this.toolSummaries.set(callId, summary);
        return { type: "tool", id, callId, phase: "start", name, summary };
      }
      case "tool.post_call": {
        const id = this.currentTurn ?? String(envelope.seq);
        const callId = String(payload.tool_call_id ?? envelope.seq);
        const name = String(payload.name ?? "tool");
        // The post_call payload carries no args: reuse the pre_call summary
        // so the finished row keeps saying what was touched.
        const summary = toolSummary(name, undefined, this.toolSummaries.get(callId));
        this.toolSummaries.delete(callId);
        return { type: "tool", id, callId, phase: "end", name, summary, ok: payload.status === "ok" };
      }
      case "agent.yield":
        return {
          type: "question",
          id: String(payload.turn_id ?? this.currentTurn ?? envelope.seq),
          text: String(payload.question ?? ""),
        };
      case "turn.end": {
        const id = String(payload.turn_id ?? this.currentTurn ?? envelope.seq);
        const outcome =
          payload.outcome === "interrupted"
            ? "stopped"
            : payload.outcome === "error"
              ? "error"
              : "done";
        const error =
          outcome === "error" && payload.error
            ? String(payload.error.message ?? payload.error)
            : undefined;
        const message: HostToWebview = { type: "turnEnd", id, outcome };
        if (error !== undefined) (message as any).error = error;
        this.currentTurn = undefined;
        this.emit({ type: "status", state: outcome === "error" ? "error" : "ready" });
        return message;
      }
      default:
        return undefined;
    }
  }

  /** callId -> the pre_call summary, so post_call rows keep their detail. */
  private toolSummaries = new Map<string, string>();

  private emit(message: HostToWebview): void {
    if (message.type !== "history") this.transcript.push(message);
    for (const listener of this.listeners) listener(message);
  }

  private errorText(error: unknown): string {
    if (error instanceof Error) return error.message;
    return String(error);
  }

  private async request(path: string, init: RequestInit): Promise<any> {
    if (!this.endpoint) throw new Error("cognition server not announced yet");
    let response: Response;
    try {
      response = await fetch(`http://127.0.0.1:${this.endpoint.port}${path}`, {
        ...init,
        headers: {
          authorization: `Bearer ${this.endpoint.token}`,
          "content-type": "application/json",
          ...init.headers,
        },
      });
    } catch (error) {
      const message = this.errorText(error);
      this.emit({ type: "status", state: "error", message });
      throw new Error(`cognition server unreachable: ${message}`);
    }
    if (response.status === 401) {
      const message = "cognition server rejected the token (401)";
      this.emit({ type: "status", state: "error", message });
      throw new Error(message);
    }
    if (!response.ok) {
      let detail = `HTTP ${response.status}`;
      try {
        const body = await response.json();
        if (body?.error) detail = String(body.error);
      } catch {
        /* keep the status-line detail */
      }
      const message = `cognition server: ${detail}`;
      this.emit({ type: "status", state: "error", message });
      throw new Error(message);
    }
    return response.json();
  }
}

/** The human line shown for a tool call: what it touched, capped short. */
function toolSummary(name: any, args: any, fallback?: string): string {
  const tool = String(name ?? "tool");
  const arg = (key: string): string | undefined =>
    args && typeof args === "object" && typeof args[key] === "string" ? args[key] : undefined;
  switch (tool) {
    case "read":
    case "write":
    case "edit": {
      const path = arg("path");
      return path !== undefined ? `${tool} ${path}` : (fallback ?? tool);
    }
    case "grep": {
      const pattern = arg("pattern");
      return pattern !== undefined ? `grep ${pattern}` : (fallback ?? tool);
    }
    case "bash": {
      const command = arg("command") ?? "";
      const first = command.split("\n")[0] ?? "";
      return first ? `bash: ${first.slice(0, 60)}` : (fallback ?? tool);
    }
    default:
      return fallback ?? tool;
  }
}
