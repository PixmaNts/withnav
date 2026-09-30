import * as vscode from "vscode";
import { ChatBackend, ConversationSummary, HostToWebview } from "./chatProtocol";
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
  /** Saved conversations of the workspace, newest first (last listing). */
  private conversations: ConversationSummary[] = [];
  /** A stopped conversation to resume on the next send (Stop keeps it). */
  private resumeId?: string;
  /** Cancels the SSE fetch of the running stream, if any. */
  private streamAbort?: AbortController;
  /** Bumped by every deliberate stream cancel: stale connects go quiet. */
  private streamGeneration = 0;
  /** Turn ids that received a live text delta (replayed turns get none). */
  private turnTextDelta = new Set<string>();

  setServerInfo(info: ServerInfoParams): void {
    // A restart of the language server publishes a NEW endpoint: the old
    // session and stream belong to a dead server, so drop them and reopen
    // the newest saved conversation against the new port.
    const restarted =
      !!this.endpoint && (this.endpoint.port !== info.port || this.endpoint.token !== info.token);
    const first = !this.endpoint;
    if (restarted) {
      this.abortStream();
      this.session = undefined;
      this.resumeId = undefined;
      this.lastSeq = undefined;
      this.delivered.clear();
      this.currentTurn = undefined;
      this.emit({ type: "status", state: "connecting" });
    }
    this.endpoint = info;
    if (this.session) {
      void this.connect();
      return;
    }
    if (first || restarted) void this.bootstrapConversations();
  }

  history(): HostToWebview[] {
    return [...this.transcript];
  }

  subscribe(listener: (message: HostToWebview) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  currentConversationId(): string | undefined {
    return this.session ?? this.resumeId;
  }

  async listConversations(): Promise<ConversationSummary[]> {
    // Before the first announcement there is nothing to list; the bootstrap
    // on the announcement pushes the real list.
    if (!this.endpoint) {
      this.emitConversations();
      return [];
    }
    const body = await this.request(`/v1/sessions?${this.workspaceQuery()}`, { method: "GET" });
    this.conversations = parseSessionList(body);
    this.emitConversations();
    return [...this.conversations];
  }

  async openConversation(id: string): Promise<void> {
    if (!this.endpoint) await this.waitForEndpoint();
    const summary = this.conversations.find((c) => c.id === id);
    // Resume first: an unknown id (404) must leave the current view intact.
    const created = await this.request("/v1/sessions", {
      method: "POST",
      body: JSON.stringify({ workspace: this.workspacePath(), profile: "navigator", resume: id }),
    });
    const sessionId = String(created?.session_id ?? "");
    if (!sessionId) throw new Error("cognition server: resume returned no id");
    // Stop streaming the previous conversation before switching.
    this.abortStream();
    this.resetTranscript();
    this.session = sessionId;
    this.resumeId = undefined;
    this.emit({ type: "conversationOpened", id: sessionId, title: summary?.title ?? sessionId });
    this.emit({ type: "history", items: [] });
    this.emit({ type: "status", state: "ready" });
    // No Last-Event-ID: the stored events replay the whole conversation.
    void this.connect();
    void this.listConversations().catch(() => undefined);
  }

  async newConversation(): Promise<void> {
    // The previous conversation stays on disk; the next send creates fresh.
    this.abortStream();
    this.session = undefined;
    this.resumeId = undefined;
    this.resetTranscript();
    this.emit({ type: "conversationOpened", title: "New conversation" });
    this.emit({ type: "history", items: [] });
    this.emit({ type: "status", state: "ready" });
    this.emitConversations();
  }

  async deleteConversation(id: string): Promise<void> {
    await this.request(`/v1/sessions/${encodeURIComponent(id)}`, { method: "DELETE" });
    this.conversations = this.conversations.filter((c) => c.id !== id);
    if (this.session === id || this.resumeId === id) {
      // The open conversation vanished: land in a fresh one (which pushes
      // the updated list itself).
      await this.newConversation();
    } else {
      this.emitConversations();
    }
  }

  async send(text: string, context?: string): Promise<void> {
    // The driver's message arrives as `message.received` on the stream; do
    // NOT emit an optimistic copy here (that duplicated it).
    const session = await this.ensureSession();
    await this.request(`/v1/sessions/${session}/messages`, {
      method: "POST",
      body: JSON.stringify({
        target_actor_id: ROOT_ACTOR,
        content: [{ type: "text", text: context ? `${text}\n\n${context}` : text }],
      }),
    });
  }

  async stop(): Promise<void> {
    if (!this.session || !this.endpoint) return;
    const session = this.session;
    this.session = undefined;
    // A stopped session accepts no new messages on the same id, but the
    // conversation survives: the next send resumes it under the same id,
    // so keep the stream cursor to continue where this one ended.
    this.resumeId = session;
    await this.request(`/v1/sessions/${session}/stop`, { method: "POST" });
    this.emit({ type: "status", state: "ready" });
  }

  // ---- session lifecycle ----

  private workspacePath(): string {
    return vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? process.cwd();
  }

  private workspaceQuery(): string {
    return `workspace=${encodeURIComponent(this.workspacePath())}`;
  }

  private waitForEndpoint(): Promise<void> {
    if (this.endpoint) return Promise.resolve();
    return new Promise((resolve) => {
      const timer = setInterval(() => {
        if (this.endpoint) {
          clearInterval(timer);
          resolve();
        }
      }, 100);
    });
  }

  /** Lists the workspace's conversations and resumes the newest one, if any. */
  private async bootstrapConversations(): Promise<void> {
    try {
      const body = await this.request(`/v1/sessions?${this.workspaceQuery()}`, { method: "GET" });
      this.conversations = parseSessionList(body);
      const newest = this.conversations[0];
      if (newest) await this.openConversation(newest.id);
      else this.emitConversations();
    } catch {
      // The endpoint announcement is our only trigger; if listing fails
      // (e.g. a server without the history store), stay empty and calm.
      this.emitConversations();
    }
  }

  private emitConversations(): void {
    this.emit({
      type: "conversations",
      items: [...this.conversations],
      currentId: this.currentConversationId(),
    });
  }

  /** Drops everything that described the previous conversation. */
  private resetTranscript(): void {
    this.transcript = [];
    this.lastSeq = undefined;
    this.delivered.clear();
    this.currentTurn = undefined;
    this.toolSummaries.clear();
    this.turnTextDelta.clear();
  }

  /** Stops the SSE fetch of the running stream, if any. */
  private abortStream(): void {
    this.streamGeneration++;
    this.streaming = false;
    this.streamAbort?.abort();
    this.streamAbort = undefined;
  }

  private async connect(): Promise<void> {
    if (!this.endpoint || !this.session || this.streaming) return;
    this.streaming = true;
    const generation = this.streamGeneration;
    const endpoint = this.endpoint;
    const session = this.session;
    const abort = new AbortController();
    this.streamAbort = abort;
    try {
      const headers: Record<string, string> = {
        authorization: `Bearer ${endpoint.token}`,
      };
      if (this.lastSeq !== undefined) headers["last-event-id"] = this.lastSeq;
      const response = await fetch(
        `http://127.0.0.1:${endpoint.port}/v1/sessions/${session}/events`,
        { headers, signal: abort.signal },
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
        // A newer stream took over (open/new/delete): drop this one quietly.
        if (generation !== this.streamGeneration) return;
        buffer += decoder.decode(part.value, { stream: true });
        let split: number;
        while ((split = buffer.indexOf("\n\n")) !== -1) {
          const [frame, rest] = [buffer.slice(0, split), buffer.slice(split + 2)];
          buffer = rest;
          this.handleFrame(this.parseFrame(frame));
        }
      }
    } catch (error) {
      // A deliberate cancel (open/new/delete) is not an error.
      if (generation === this.streamGeneration && !isAbort(error)) {
        this.emit({ type: "status", state: "error", message: this.errorText(error) });
      }
    }
    if (generation !== this.streamGeneration) return; // a newer stream took over
    if (this.streamAbort === abort) this.streamAbort = undefined;
    this.streaming = false;
    // The stream ended (server drain or drop): reconnect — resuming this
    // session with the cursor, or picking up whatever session is current
    // (a send after Stop may have created one while this drained), unless
    // there is none (stop / new conversation / server restart).
    if (this.session !== undefined) {
      setTimeout(() => void this.connect(), RECONNECT_DELAY_MS);
    }
  }

  private async ensureSession(): Promise<string> {
    if (this.session) return this.session;
    this.emit({ type: "status", state: "connecting" });
    await this.waitForEndpoint();
    const resume = this.resumeId;
    const created = await this.request("/v1/sessions", {
      method: "POST",
      body: JSON.stringify(
        resume
          ? { workspace: this.workspacePath(), profile: "navigator", resume }
          : { workspace: this.workspacePath(), profile: "navigator" },
      ),
    });
    const sessionId = String(created?.session_id ?? "");
    if (!sessionId) throw new Error("cognition server: create session returned no id");
    this.session = sessionId;
    this.resumeId = undefined;
    if (!resume) {
      // A brand-new session restarts its seq space. (On resume: same
      // conversation, same seq space — keep the cursor and the delivered
      // set so the resumed stream continues instead of replaying.)
      this.lastSeq = undefined;
      this.delivered.clear();
      this.turnTextDelta.clear();
    }
    this.emit({ type: "status", state: "ready" });
    void this.connect();
    void this.listConversations().catch(() => undefined);
    return this.session;
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
          this.turnTextDelta.add(id);
          return { type: "delta", id, text: String(delta.text ?? "") };
        }
        if (delta.delta_type === "thinking") {
          return { type: "reasoning", id, text: String(delta.text ?? "") };
        }
        return undefined; // tool_call deltas ride the tool events instead
      }
      case "llm.response": {
        // During a history replay no `llm.delta` arrives: a turn that got
        // no live text renders its final message as one delta. Live turns
        // (flagged above) must not be duplicated.
        const id = String(payload.turn_id ?? this.currentTurn ?? envelope.seq);
        const message = payload.message ?? {};
        const text = typeof message.text === "string" ? message.text : "";
        const thinking = typeof message.thinking === "string" ? message.thinking : "";
        if (this.turnTextDelta.has(id)) return undefined;
        if (!text && !thinking) return undefined;
        if (thinking) this.emit({ type: "reasoning", id, text: thinking });
        return text ? { type: "delta", id, text } : undefined;
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
        // A non-zero shell exit is reported as status error, but it is
        // often harmless: `grep` finding nothing exits 1, and in a `;` chain
        // the last command's code decides for all the output above it. Show
        // the code so the driver can judge (observed 2026-09-30: a
        // `wc; ls; cat …` chain marked failed although its output was fine).
        const exitCode = payload.metadata?.exit_code;
        const shown = typeof exitCode === "number" && exitCode !== 0 ? `${summary} (exit ${exitCode})` : summary;
        return { type: "tool", id, callId, phase: "end", name, summary: shown, ok: payload.status === "ok" };
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
    // Conversation-list metadata is pushed, never replayed as transcript.
    const ephemeral =
      message.type === "history" ||
      message.type === "conversations" ||
      message.type === "conversationOpened";
    if (!ephemeral) this.transcript.push(message);
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
      if (response.status !== 204) {
        try {
          const body = await response.json();
          if (body?.error) detail = String(body.error);
        } catch {
          /* keep the status-line detail */
        }
      }
      const message = `cognition server: ${detail}`;
      this.emit({ type: "status", state: "error", message });
      throw new Error(message);
    }
    // DELETE answers 204 and other routes may answer empty: undefined, not a
    // JSON parse crash.
    if (response.status === 204) return undefined;
    const raw = await response.text();
    return raw ? JSON.parse(raw) : undefined;
  }
}

/** A fetch aborted on purpose (open/new/delete switching streams). */
function isAbort(error: unknown): boolean {
  return error instanceof Error && error.name === "AbortError";
}

/** Maps the wire session list onto the panel's ConversationSummary. */
function parseSessionList(body: any): ConversationSummary[] {
  const sessions = Array.isArray(body?.sessions) ? body.sessions : [];
  return sessions
    .filter((s: any) => s && typeof s.id === "string")
    .map((s: any) => ({
      id: s.id,
      title: typeof s.title === "string" && s.title ? s.title : "Untitled conversation",
      updatedAt: typeof s.updated_at === "string" ? s.updated_at : "",
      messageCount: typeof s.message_count === "number" ? s.message_count : 0,
    }));
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
      // One line, whitespace collapsed: the row truncates it with an ellipsis
      // and shows the whole command on hover.
      const oneLine = command.replace(/\s+/g, " ").trim();
      return oneLine ? `bash: ${oneLine}` : (fallback ?? tool);
    }
    default:
      return fallback ?? tool;
  }
}
