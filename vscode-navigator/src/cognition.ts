import * as vscode from "vscode";
import { ChatBackend, ConversationSummary, HostToWebview, ModelInfo, PermissionDecision } from "./chatProtocol";
import { ConversationControls, eventView, permissionCall, PermissionCallReplay, permissionEvidence, WireEnvelope } from "./eventView";
import type { ServerInfoParams } from "./protocol";
import { CLIENT_COMMANDS, routeComposer } from "./commands";
import type { CommandInfo } from "./chatProtocol";

/** One SSE frame off the event stream: the seq id plus the parsed envelope. */
interface EventFrame {
  id: string;
  event: string;
  envelope: any;
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
class ConversationState {
  commands: Map<string, { draft: string; name: string; arguments: string }> = new Map();
  commandResults: Map<string, any> = new Map();
  acceptedMessages: Set<string> = new Set();
  pendingModel?: { alias: string; operationId: string };
  agent = "build";
  startVariant?: string | null;
  transcript: HostToWebview[] = [];
  session: string | undefined;
  lastSeq: string | undefined;
  delivered: Set<number> = new Set();
  currentTurn: string | undefined;
  streaming: boolean = false;
  selectedModel: string | undefined;
  lastUsed: number | undefined;
  resumeId: string | undefined;
  streamAbort: AbortController | undefined;
  streamGeneration: number = 0;
  turnTextDelta: Set<string> = new Set();
  controls: ConversationControls = new ConversationControls();
  controlOwners: Map<string, { session: string; generation: number }> = new Map();
  pendingControls: Map<string, symbol> = new Map();
  childReads: Map<string, AbortController> = new Map();
  childReadQueue: Set<string> = new Set();
  childReadsAttempted: Set<string> = new Set();
  childReadTimer: ReturnType<typeof setTimeout> | undefined;
  replayThrough: number | undefined;
  previousRunThrough: number = -1;
  toolSummaries: Map<string, string> = new Map();
  toolEvidence: Map<string, NonNullable<Extract<HostToWebview, { type: "tool" }>["evidence"]>> = new Map();
}

export class CognitionBackend implements ChatBackend {
  private state = new ConversationState();
  private nextGeneration = 0;
  private endpoint?: ServerInfoParams;
  private listeners = new Set<(message: HostToWebview) => void>();
  /** Last delivered seq, sent back as `Last-Event-ID` on reconnect. */
  /** Envelope seqs already delivered, so a resync's fresh full replay does
   *  not duplicate messages already in the transcript. */
  /** Saved conversations of the workspace, newest first (last listing). */
  private conversations: ConversationSummary[] = [];
  /** Models from GET /v1/models, and the alias the next new conversation uses
   *  (or the open conversation's, once it exists). */
  private models: ModelInfo[] = [];
  private commands: CommandInfo[] = [];

  async refreshCommands(): Promise<void> {
    const generation = this.currentGeneration();
    if (!this.endpoint) return;
    const body = await this.request("/v1/commands", { method: "GET" });
    this.checkGeneration(generation);
    this.commands = (Array.isArray(body?.commands) ? body.commands : []).filter((c: any) =>
      typeof c?.name === "string" && /^[a-z][a-z0-9_-]*$/.test(c.name) && typeof c.description === "string" && typeof c.args_hint === "string");
    this.emit({ type: "commands", items: [...this.commands, ...CLIENT_COMMANDS.filter(c => !this.commands.some(s => s.name === c.name))] });
  }

  async runComposerCommand(draft: string, generation = this.currentGeneration()): Promise<boolean> {
    this.checkGeneration(generation);
    const routed = routeComposer(draft);
    if (routed.kind !== "command") throw new Error("Not a command");
    const { name, arguments: args } = routed;
    if (typeof args !== "string") throw new Error("Command arguments must be text");
    if (!this.commands.length) { await this.refreshCommands(); this.checkGeneration(generation); }
    if (![...this.commands, ...CLIENT_COMMANDS].some(c => c.name === name)) throw new Error(`Unknown command: /${name}. Use /help.`);
    if (["help", "skills", "stop"].includes(name) && args) throw new Error(`/${name} takes no arguments`);
    if (name === "help") {
      this.emit({ type: "notice", text: [...this.commands, ...CLIENT_COMMANDS.filter(c => !this.commands.some(s => s.name === c.name))].map(c => `/${c.name} ${c.args_hint} — ${c.description}`).join("\n") });
      return true;
    }
    if (name === "new") {
      if (args && !this.models.some(m => m.alias === args)) throw new Error("Unknown model alias");
      await this.newConversation();
      if (args) this.selectModel(args);
      return true;
    }
    if (name === "model" && !args) {
      this.emit({ type: "commandPicker", kind: "model", items: this.models.map(m => ({ name: m.alias, description: m.model })) });
      return true;
    }
    if (name === "variant" && !args) {
      const model = this.models.find(m => m.alias === this.state.selectedModel);
      this.emit({ type: "commandPicker", kind: "variant", items: [...(model?.variants ?? []), "off"].map(name => ({ name, description: name === "off" ? "No variant" : "Model variant" })) });
      return true;
    }
    if (name === "skills") {
      if (!this.state.session) throw new Error("A conversation is needed. Send a message first.");
      const session = this.state.session;
      const body = await this.request(`/v1/sessions/${encodeURIComponent(session)}/skills`, { method: "GET" });
      this.checkGeneration(generation);
      if (session !== this.state.session) throw new Error("Conversation changed");
      this.emit({ type: "commandPicker", kind: "skills", items: (body?.skills ?? []).filter((s: any) => typeof s?.name === "string" && typeof s.description === "string"), disabledReason: body?.disabled_reason ?? undefined });
      return true;
    }
    if (name === "skill") {
      if (!this.state.session) throw new Error("A conversation is needed. Send a message first.");
      const match = /^(\S+)(?:\s+([\s\S]*))?$/.exec(args);
      if (!match) throw new Error("Usage: /skill <name> [request]");
      const session = this.state.session;
      const result = await this.request(`/v1/sessions/${encodeURIComponent(session)}/messages`, { method: "POST", body: JSON.stringify({ target_actor_id: ROOT_ACTOR, content: [{ type: "text", text: match[2] || `Apply the ${match[1]} skill.` }, { type: "skill", name: match[1] }] }) });
      this.checkGeneration(generation);
      const operation = result?.command_id ?? result?.operation_id;
      if (typeof operation === "string") this.trackCommand(operation, { draft, name, arguments: args });
      else if (result?.accepted === false || result?.ok === false) throw new Error(String(result.error ?? "Message refused"));
      return typeof operation !== "string";
    }
    if (name === "stop") { if (!this.state.session) throw new Error("A conversation is needed."); await this.stop(); return true; }
    if (!this.state.session && !this.state.resumeId) {
      if (!["model", "variant", "agent"].includes(name)) throw new Error("A conversation is needed. Send a message first.");
      if (name === "model") { if (!this.models.some(m => m.alias === args)) throw new Error("Unknown model alias"); this.selectModel(args); return true; }
      if (name === "variant") {
        const model = this.models.find(m => m.alias === this.state.selectedModel);
        if (args !== "off" && !model?.variants?.includes(args)) throw new Error("Unknown variant for this model");
        this.state.startVariant = args === "off" ? null : args;
        this.emit({ type: "notice", text: `Starting variant: ${args}` });
        return true;
      }
      if (!args || /\s/.test(args)) throw new Error("Usage: /agent <name>");
      this.state.agent = args;
      this.emit({ type: "agent", name: args });
      return true;
    }
    if (!this.state.session) throw new Error("The conversation is stopped. Send a message to resume it first.");
    if (name === "model" && this.hasOpenPermissions()) throw new Error("Answer the pending permission before switching model.");
    const session = this.state.session;
    const result = await this.request(`/v1/sessions/${encodeURIComponent(session)}/commands`, { method: "POST", body: JSON.stringify({ name, arguments: args }) });
    this.checkGeneration(generation);
    if (session !== this.state.session) throw new Error("Conversation changed");
    if (typeof result?.operation_id !== "string") throw new Error("Command returned no operation id");
    if (name === "model" && !this.state.commandResults.has(result.operation_id)) {
      this.state.pendingModel = { alias: args, operationId: result.operation_id };
      this.emit({ type: "modelPending", alias: args });
    }
    this.trackCommand(result.operation_id, { draft, name, arguments: args });
    return false; // Only the durable outcome consumes the draft.
  }

  private trackCommand(operation: string, command: { draft: string; name: string; arguments: string }): void {
    if (command.name === "skill" && this.state.acceptedMessages.has(operation)) {
      this.emit({ type: "sendResult", text: command.draft, accepted: true });
      return;
    }
    this.state.commands.set(operation, command);
    const outcome = this.state.commandResults.get(operation);
    if (outcome) this.settleCommand(outcome);
  }

  private settleCommand(p: any): void {
    const operation = p.operation_id;
    if (typeof operation !== "string") return;
    this.state.commandResults.set(operation, p);
    const command = this.state.commands.get(operation);
    if (command) {
      this.emit({ type: "sendResult", text: command.draft, accepted: p.ok === true });
      if (p.ok !== true) this.emit({ type: "commandError", text: String(p.error ?? "Command refused. Your draft was kept.") });
      this.state.commands.delete(operation);
    }
    const deferred = p.ok === true && p.data?.accepted === true && p.data?.applies === "after_turn";
    if (this.state.pendingModel?.operationId === operation && !deferred) {
      this.state.pendingModel = undefined;
      this.emit({ type: "modelPending" });
    }
  }
  /** Default alias the server announced, restored by "new conversation". */
  private defaultModel?: string;
  /** Tokens of the last model call, re-sent when the model or list changes. */
  /** A stopped conversation to resume on the next send (Stop keeps it). */
  /** Cancels the SSE fetch of the running stream, if any. */
  /** Bumped by every deliberate stream cancel: stale connects go quiet. */
  /** Turn ids that received a live text delta (replayed turns get none). */
  private disposed = false;

  currentGeneration(): number { return this.state.streamGeneration; }
  private checkGeneration(generation: number): void {
    if (this.disposed || generation !== this.state.streamGeneration) throw new Error("Send cancelled: the conversation changed. Your draft was kept.");
  }

  setServerInfo(info: ServerInfoParams): void {
    // A restart of the language server publishes a NEW endpoint: the old
    // session and stream belong to a dead server, so drop them and reopen
    // the newest saved conversation against the new port.
    const restarted =
      !!this.endpoint && (this.endpoint.port !== info.port || this.endpoint.token !== info.token);
    const first = !this.endpoint;
    if (restarted) {
      this.abortStream();
      this.state.session = undefined;
      this.state.resumeId = undefined;
      this.resetTranscript();
      this.conversations = [];
      this.emit({ type: "conversationOpened", title: "Server restarted" });
      this.emit({ type: "history", items: [] });
      this.emit({ type: "status", state: "connecting" });
    }
    this.endpoint = info;
    if (this.state.session) {
      void this.connect();
      return;
    }
    if (first || restarted) {
      void this.fetchModels();
      void this.bootstrapConversations();
    }
  }

  history(): HostToWebview[] {
    return [...this.state.transcript];
  }

  subscribe(listener: (message: HostToWebview) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  currentConversationId(): string | undefined {
    return this.state.session ?? this.state.resumeId;
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
    const previous = this.state;
    this.abortStream();
    const generation = this.state.streamGeneration;
    this.state = new ConversationState();
    this.state.streamGeneration = generation;
    this.state.session = undefined;
    this.state.resumeId = undefined;
    this.emit({ type: "conversationOpened", title: "Opening conversation" });
    this.emit({ type: "history", items: [] });
    try {
      if (!this.endpoint) await this.waitForEndpoint();
      this.checkGeneration(generation);
      const summary = this.conversations.find((c) => c.id === id);
      const before = await this.reconcileSession(id, generation);
      this.checkGeneration(generation);
      this.state.previousRunThrough = before.actor_state === "stopped" ? before.current_seq : -1;
      // Resume first: an unknown id (404) must leave the current view intact.
      const created = await this.request("/v1/sessions", {
        method: "POST",
        body: JSON.stringify({ workspace: this.workspacePath(), profile: "navigator", resume: id }),
      });
      const sessionId = String(created?.session_id ?? "");
      this.checkGeneration(generation);
      if (!sessionId) throw new Error("cognition server: resume returned no id");
      // Stop streaming the previous conversation before switching.
      this.state.session = sessionId;
      this.state.resumeId = undefined;
      this.state.replayThrough = Infinity;
      this.emit({ type: "conversationOpened", id: sessionId, title: summary?.title ?? sessionId });
      this.emit({ type: "history", items: [] });
      this.emit({ type: "status", state: "ready" });
      void this.loadSessionModel(sessionId);
      // No Last-Event-ID: the stored events replay the whole conversation.
      void this.connect();
      void this.listConversations().catch(() => undefined);
    } catch (error) {
      if (generation === this.state.streamGeneration) {
        this.state = previous;
        this.state.streamGeneration = generation;
        this.state.transcript = this.state.transcript.map(message => ({ ...message, generation }));
        this.bindControlOwners();
        this.emit({ type: "conversationOpened", id: this.currentConversationId(), title: "Conversation" });
        this.emit({ type: "history", items: this.history() });
        this.emit({ type: "status", state: "error", message: `Could not open conversation: ${this.errorText(error)}` });
        void this.connect();
        this.resumePermissionReads();
      }
      throw error;
    }
  }

  async newConversation(): Promise<void> {
    // The previous conversation stays on disk; the next send creates fresh.
    this.abortStream();
    this.state.session = undefined;
    this.state.resumeId = undefined;
    this.resetTranscript();
    this.state.selectedModel = this.defaultModel ?? this.state.selectedModel;
    this.emit({ type: "conversationOpened", title: "New conversation" });
    this.emit({ type: "history", items: [] });
    this.emit({ type: "status", state: "ready" });
    this.emitConversations();
    this.emitModels();
  }

  async deleteConversation(id: string): Promise<void> {
    await this.request(`/v1/sessions/${encodeURIComponent(id)}`, { method: "DELETE" });
    this.conversations = this.conversations.filter((c) => c.id !== id);
    if (this.state.session === id || this.state.resumeId === id) {
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
    const { session, generation } = await this.ensureSession();
    this.checkGeneration(generation);
    await this.request(`/v1/sessions/${session}/messages`, {
      method: "POST",
      body: JSON.stringify({
        target_actor_id: ROOT_ACTOR,
        content: [{ type: "text", text: context ? `${text}\n\n${context}` : text }],
      }),
    });
  }

  async stop(): Promise<void> { await this.stopState(this.state); }

  private async stopState(origin: ConversationState): Promise<void> {
    if (!origin.session || !this.endpoint) return;
    const session = origin.session;
    const result = await this.request(`/v1/sessions/${encodeURIComponent(session)}/stop`, { method: "POST" }, "operation");
    if (result?.accepted !== true) throw new Error(`Stop refused: ${result?.actor_state ?? "not accepted"}`);
    if (origin.session !== session) return;
    origin.session = undefined;
    // A stopped session accepts no new messages on the same id, but the
    // conversation survives: the next send resumes it under the same id,
    // so keep the stream cursor to continue where this one ended.
    origin.resumeId = session;
    origin.pendingModel = undefined;
    this.emitForState(origin, { type: "modelPending" });
    this.emitForState(origin, { type: "status", state: "ready" });
  }

  hasOpenPermissions(): boolean { return this.state.controls.permissions.size > 0; }

  async answerPermission(sessionId: string, requestId: string, decision: PermissionDecision, message?: string, generation = this.state.streamGeneration): Promise<void> {
    this.checkGeneration(generation);
    const card = this.state.controls.permissions.get(requestId);
    const owner = this.state.controlOwners.get(`permission:${requestId}`);
    if (sessionId !== this.state.session || owner?.session !== sessionId || owner.generation !== generation || !card ||
        !["allow_once", "allow_folder", "deny"].includes(decision) ||
        (decision === "allow_folder" && !card.allowFolder) ||
        (message !== undefined && typeof message !== "string")) {
      throw new Error("This permission request does not belong to the open conversation.");
    }
    const key = `${sessionId}:permission:${requestId}`;
    const origin = this.state;
    const operation = Symbol(key);
    if (origin.pendingControls.has(key)) return;
    this.state.pendingControls.set(key, operation);
    this.emit({ type: "permissionState", requestId, resolution: "pending" });
    try {
      const result = await this.request(`/v1/sessions/${encodeURIComponent(sessionId)}/permissions/${encodeURIComponent(requestId)}`, {
        method: "POST", body: JSON.stringify({ decision, ...(decision === "deny" && message ? { message } : {}) }),
      }, "operation");
      if (result?.accepted === false) throw new Error("Permission answer refused by the server.");
    } catch (error) {
      if (origin.pendingControls.get(key) !== operation) return;
      origin.pendingControls.delete(key);
      if (origin.controls.permissions.has(requestId)) {
        this.emitForState(origin, { type: "permissionState", requestId,
          resolution: error instanceof HttpError && error.status === 409 ? "stale" : "error",
          message: this.errorText(error) });
      }
      if (!(error instanceof HttpError && error.status === 409)) throw error;
    }
  }

  async controlPause(sessionId: string, id: string, action: "continue" | "stop", generation = this.state.streamGeneration): Promise<void> {
    this.checkGeneration(generation);
    const pause = this.state.controls.pauses.get(id);
    const owner = this.state.controlOwners.get(id);
    if (sessionId !== this.state.session || owner?.session !== sessionId || owner.generation !== generation || !pause || !["continue", "stop"].includes(action)) {
      throw new Error("This pause does not belong to the open conversation.");
    }
    if (pause.kind === "watchdog" && action === "continue" && !pause.token) {
      throw new Error("The server did not provide a pause token. Send a new message or stop instead.");
    }
    const key = `${sessionId}:${id}`;
    const origin = this.state;
    const operation = Symbol(key);
    if (origin.pendingControls.has(key)) return;
    this.state.pendingControls.set(key, operation);
    this.emit({ type: "pauseState", id, state: "pending" });
    try {
      if (action === "stop") await this.stopState(origin);
      else {
        const session = encodeURIComponent(sessionId);
        const result = await this.request(`/v1/sessions/${session}/children/${session}/continue`, {
          method: "POST", body: JSON.stringify({ pause_token: pause.token ?? null }),
        }, "operation");
        if (result?.accepted !== true) throw new Error("Continue refused by the server.");
      }
      if (origin.session === sessionId || origin.resumeId === sessionId) this.emitForState(origin, { type: "pauseState", id, state: "resolved", message: action === "stop" ? "Stopped" : "Continued" });
    } catch (error) {
      if (origin.controls.pauses.has(id)) {
        this.emitForState(origin, { type: "pauseState", id, state: "error", message: this.errorText(error) });
      }
      throw error;
    } finally { if (origin.pendingControls.get(key) === operation) origin.pendingControls.delete(key); }
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
    const generation = this.state.streamGeneration;
    try {
      const body = await this.request(`/v1/sessions?${this.workspaceQuery()}`, { method: "GET" });
      this.conversations = parseSessionList(body);
      const newest = this.conversations[0];
      if (newest) await this.openConversation(newest.id);
      else this.emitConversations();
    } catch {
      if (generation !== this.state.streamGeneration) return;
      // The endpoint announcement is our only trigger; if listing fails
      // (e.g. a server without the history store), stay empty and calm.
      this.emitConversations();
    }
  }

  /** Fetches the models the cognition can use and preselects the default. */
  private async fetchModels(): Promise<void> {
    const endpoint = this.endpoint;
    try {
      const body = await this.request(`/v1/models?${this.workspaceQuery()}`, { method: "GET" }, "endpoint");
      this.models = parseModelList(body);
      if (typeof body?.default === "string") this.defaultModel = body.default;
      if (!this.state.selectedModel) this.state.selectedModel = this.defaultModel;
    } catch {
      if (endpoint !== this.endpoint) return;
      // No model list (older server, unreadable models.json): the picker
      // stays empty and new sessions use the server's default.
      this.models = [];
    }
    this.emitModels();
  }

  /** A conversation that exists on the server keeps its model (option A). */
  private modelLocked(): boolean {
    return this.state.session !== undefined || this.state.resumeId !== undefined;
  }

  private emitModels(): void {
    this.emit({
      type: "models",
      items: [...this.models],
      current: this.state.selectedModel,
      locked: false,
    });
    // The window depends on the model: a replay can report usage before the
    // list or the conversation's model arrives, so re-send it with both.
    if (this.state.lastUsed !== undefined) this.emitUsage();
  }

  selectModel(alias: string): void {
    if (this.modelLocked()) { void this.runComposerCommand(`/model ${alias}`).catch(error => this.emit({ type: "commandError", text: this.errorText(error) })); return; }
    if (!this.models.some((m) => m.alias === alias)) return;
    this.state.selectedModel = alias;
    this.state.startVariant = undefined;
    this.emitModels();
  }

  /** Reads the open conversation's model from GET /v1/sessions/{id}. */
  private async loadSessionModel(id: string): Promise<void> {
    const generation = this.state.streamGeneration;
    try {
      const summary = await this.reconcileSession(id, generation);
      if (id !== this.state.session) return;
      if (typeof summary?.model === "string") this.state.selectedModel = summary.model;
      this.state.replayThrough = typeof summary?.current_seq === "number" ? summary.current_seq : Infinity;
    } catch {
      // Keep the current selection: the picker is informative only here.
    }
    if (generation !== this.state.streamGeneration) return;
    if (id === this.state.session) this.drainChildReads();
    this.emitModels();
  }

  private async reconcileSession(id: string, generation: number): Promise<any> {
    for (let attempt = 0; attempt < 3; attempt++) {
      this.checkGeneration(generation);
      try {
        const summary = await this.request(`/v1/sessions/${encodeURIComponent(id)}`, { method: "GET" });
        this.checkGeneration(generation);
        if (!Number.isSafeInteger(summary?.current_seq) || summary.current_seq < 0) throw new Error("Invalid session summary");
        return summary;
      } catch (error) {
        this.checkGeneration(generation);
        this.emit({ type: "status", state: "error", message: "Could not reconcile conversation history; retrying…" });
        if (attempt === 2) throw error;
        await new Promise(resolve => setTimeout(resolve, 100 * (attempt + 1)));
      }
    }
  }

  /** Records the context used by a model call (prompt + completion tokens). */
  private recordUsage(usage: any): void {
    const prompt = typeof usage?.prompt_tokens === "number" ? usage.prompt_tokens : 0;
    const completion = typeof usage?.completion_tokens === "number" ? usage.completion_tokens : 0;
    if (prompt + completion <= 0) return;
    this.state.lastUsed = prompt + completion;
    this.emitUsage();
  }

  /** The last usage against the current model's window (absent when unknown). */
  private emitUsage(): void {
    if (this.state.lastUsed === undefined) return;
    const window = this.models.find((m) => m.alias === this.state.selectedModel)?.contextWindow;
    this.emit({ type: "usage", used: this.state.lastUsed, window });
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
    this.state.commands.clear();
    this.state.commandResults.clear();
    this.state.acceptedMessages.clear();
    this.state.pendingModel = undefined;
    this.state.agent = "build";
    this.state.startVariant = undefined;
    this.state.transcript = [];
    this.state.lastSeq = undefined;
    this.state.delivered.clear();
    this.state.currentTurn = undefined;
    this.state.toolSummaries.clear();
    this.state.toolEvidence.clear();
    this.state.turnTextDelta.clear();
    this.state.lastUsed = undefined;
    this.state.controls.reset();
    this.state.controlOwners.clear();
    this.state.pendingControls.clear();
    this.state.replayThrough = undefined;
    this.state.previousRunThrough = -1;
  }

  /** Stops the SSE fetch of the running stream, if any. */
  private abortStream(): void {
    this.cancelPermissionReads();
    this.state.streamGeneration = ++this.nextGeneration;
    this.state.streaming = false;
    this.state.streamAbort?.abort();
    this.state.streamAbort = undefined;
  }

  private async connect(): Promise<void> {
    if (this.disposed || !this.endpoint || !this.state.session || this.state.streaming) return;
    this.state.streaming = true;
    const generation = this.state.streamGeneration;
    const endpoint = this.endpoint;
    const session = this.state.session;
    const abort = new AbortController();
    this.state.streamAbort = abort;
    try {
      const headers: Record<string, string> = {
        authorization: `Bearer ${endpoint.token}`,
      };
      if (this.state.lastSeq !== undefined) headers["last-event-id"] = this.state.lastSeq;
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
        if (generation !== this.state.streamGeneration) return;
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
      if (generation === this.state.streamGeneration && !isAbort(error)) {
        this.emit({ type: "status", state: "error", message: this.errorText(error) });
      }
    }
    if (generation !== this.state.streamGeneration) return; // a newer stream took over
    if (this.state.streamAbort === abort) this.state.streamAbort = undefined;
    this.state.streaming = false;
    // The stream ended (server drain or drop): reconnect — resuming this
    // session with the cursor, or picking up whatever session is current
    // (a send after Stop may have created one while this drained), unless
    // there is none (stop / new conversation / server restart).
    if (this.state.session !== undefined) {
      setTimeout(() => void this.connect(), RECONNECT_DELAY_MS);
    }
  }

  private async ensureSession(): Promise<{ session: string; generation: number }> {
    if (this.state.session) return { session: this.state.session, generation: this.state.streamGeneration };
    this.abortStream();
    const generation = this.state.streamGeneration;
    if (!this.state.resumeId) {
      const agent = this.state.agent;
      const variant = this.state.startVariant;
      this.resetTranscript();
      this.state.agent = agent;
      this.state.startVariant = variant;
      this.emit({ type: "history", items: [] });
    } else {
      this.state.transcript = this.state.transcript.map(message => ({ ...message, generation }));
      this.bindControlOwners();
    }
    this.emit({ type: "conversationOpened", id: this.state.resumeId, title: "Starting conversation" });
    this.emit({ type: "status", state: "connecting" });
    await this.waitForEndpoint();
    this.checkGeneration(generation);
    const resume = this.state.resumeId;
    const created = await this.request("/v1/sessions", {
      method: "POST",
      body: JSON.stringify(
        resume
          ? { workspace: this.workspacePath(), profile: "navigator", resume }
           : { workspace: this.workspacePath(), profile: "navigator", model: this.state.selectedModel, agent: this.state.agent, ...(this.state.startVariant !== undefined ? { variant: this.state.startVariant } : {}) },
      ),
    });
    const sessionId = String(created?.session_id ?? "");
    this.checkGeneration(generation);
    if (!sessionId) throw new Error("cognition server: create session returned no id");
    this.state.session = sessionId;
    this.state.resumeId = undefined;
    if (!resume) {
      // A brand-new session restarts its seq space. (On resume: same
      // conversation, same seq space — keep the cursor and the delivered
      // set so the resumed stream continues instead of replaying.)
      this.state.lastSeq = undefined;
      this.state.delivered.clear();
      this.state.turnTextDelta.clear();
    }
    this.emit({ type: "status", state: "ready" });
    this.emitModels();
    this.emit({ type: "conversationOpened", id: sessionId, title: this.conversations.find(c => c.id === sessionId)?.title ?? "Conversation" });
    this.emit({ type: "agent", name: this.state.agent });
    void this.connect();
    void this.listConversations().catch(() => undefined);
    return { session: sessionId, generation };
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
    if (frame.id) this.state.lastSeq = frame.id;
    if (frame.event === "resync") {
      // The cursor is out of range: drop it, rebuild the view from what the
      // bridge already holds, and let the reconnect's full replay fill any
      // gap (already-delivered seqs are skipped below).
      this.state.lastSeq = undefined;
      this.emit({ type: "history", items: this.history().filter((m) => m.type !== "history") });
      return;
    }
    if (!frame.envelope) return;
    const seq = frame.envelope.seq;
    if (typeof seq === "number") {
      if (this.state.delivered.has(seq)) return;
      this.state.delivered.add(seq);
    }
    const message = this.mapEvent(frame.envelope);
    if (message) {
      this.emit(message);
      if (message.type === "permission" && message.card.childSessionId) this.scheduleChildRead(message.card.childSessionId);
    }
    if (this.state.replayThrough !== undefined && Number(frame.id) >= this.state.replayThrough) {
      this.state.replayThrough = undefined;
      this.drainChildReads();
    }
  }

  cancelPermissionReads(): void {
    if (this.state.childReadTimer) clearTimeout(this.state.childReadTimer);
    this.state.childReadTimer = undefined;
    this.state.childReadQueue.clear();
    this.state.childReadsAttempted.clear();
    for (const abort of this.state.childReads.values()) abort.abort();
    this.state.childReads.clear();
  }

  resumePermissionReads(): void {
    const completed = new Set(this.state.transcript.filter(m => m.type === "permissionCall").map(m => m.type === "permissionCall" ? m.requestId : ""));
    for (const card of this.state.controls.permissions.values()) {
      if (card.childSessionId && !completed.has(card.requestId)) this.scheduleChildRead(card.childSessionId);
    }
  }

  dispose(): void {
    this.disposed = true;
    this.abortStream();
    this.listeners.clear();
  }

  private scheduleChildRead(child: string): void {
    this.state.childReadQueue.add(child);
    if (this.state.childReadTimer) clearTimeout(this.state.childReadTimer);
    // Let the current replay batch settle before opening reads for unresolved cards.
    this.state.childReadTimer = setTimeout(() => { this.state.childReadTimer = undefined; this.drainChildReads(); }, 100);
  }

  private drainChildReads(): void {
    if (this.disposed || (this.state.replayThrough !== undefined &&
        ![...this.state.delivered].some(seq => seq >= this.state.replayThrough!))) return;
    for (const child of this.state.childReadQueue) {
      if (this.state.childReads.size >= 2) return;
      if (this.state.childReads.has(child)) continue;
      this.state.childReadQueue.delete(child);
      const cards = [...this.state.controls.permissions.values()].filter(card => card.childSessionId === child && !this.state.childReadsAttempted.has(card.requestId));
      if (!cards.length) continue;
      for (const card of cards) this.state.childReadsAttempted.add(card.requestId);
      const abort = new AbortController();
      this.state.childReads.set(child, abort);
      void this.loadChildPermissionCalls(child, cards, abort).finally(() => {
        if (this.state.childReads.get(child) === abort) {
          this.state.childReads.delete(child);
          this.drainChildReads();
        }
      });
    }
  }

  /** One bounded, read-only replay per child; substitute only at the matching request. */
  private async loadChildPermissionCalls(child: string, cards: import("./chatProtocol").PermissionCard[], abort: AbortController): Promise<void> {
    const session = this.state.session;
    const generation = this.state.streamGeneration;
    const endpoint = this.endpoint;
    if (!endpoint) return;
    const remaining = new Map(cards.map(card => [card.requestId, card]));
    const replay = new PermissionCallReplay();
    const timer = setTimeout(() => abort.abort(), 5000);
    let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
    const cancel = (): void => { void reader?.cancel().catch(() => undefined); };
    abort.signal.addEventListener("abort", cancel, { once: true });
    try {
      const response = await fetch(`http://127.0.0.1:${endpoint.port}/v1/sessions/${encodeURIComponent(child)}/events`, {
        headers: { authorization: `Bearer ${endpoint.token}` }, signal: abort.signal,
      });
      if (!response.ok || !response.body) return;
      reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = "";
      let bytes = 0;
      for (;;) {
        const part = await reader.read();
        if (part.done || abort.signal.aborted || generation !== this.state.streamGeneration) return;
        bytes += part.value.byteLength;
        if (bytes > 2 * 1024 * 1024) return;
        buffer += decoder.decode(part.value, { stream: true });
        let split: number;
        while ((split = buffer.indexOf("\n\n")) !== -1) {
          const raw = buffer.slice(0, split);
          buffer = buffer.slice(split + 2);
          const event = this.parseFrame(raw)?.envelope;
          if (!event) continue;
          replay.observe(event);
          if (event.kind === "permission.requested") {
            const p = event.payload;
            const card = remaining.get(p?.request_id);
            if (!card) continue;
            const args = p.tool_call_id === card.callId && p.tool === card.tool && p.arguments === card.arguments ? replay.argumentsFor(p) : undefined;
            if (args && session === this.state.session && generation === this.state.streamGeneration && this.state.controls.permissions.has(card.requestId)) {
              this.emit({ type: "permissionCall", requestId: card.requestId, call: permissionCall(card.tool, args) });
            }
            remaining.delete(card.requestId);
            if (!remaining.size) return;
          }
        }
      }
    } catch { /* The card explicitly says when full arguments are unavailable. */ }
    finally { clearTimeout(timer); abort.signal.removeEventListener("abort", cancel); abort.abort(); await reader?.cancel().catch(() => undefined); }
  }

  // ---- event -> webview mapping ----

  private mapEvent(envelope: WireEnvelope): HostToWebview | undefined {
    const payload = envelope.payload ?? {};
    if (envelope.kind === "message.received" && typeof payload.command_id === "string") {
      this.state.acceptedMessages.add(payload.command_id);
      const command = this.state.commands.get(payload.command_id);
      if (command?.name === "skill") {
        this.emit({ type: "sendResult", text: command.draft, accepted: true });
        this.state.commands.delete(payload.command_id);
      }
    }
    if (envelope.kind === "session.end") {
      this.state.pendingModel = undefined;
      this.emit({ type: "modelPending" });
    }
    if (envelope.kind === "command.result") {
      this.settleCommand(payload);
      return { type: "notice", operationId: payload.operation_id, name: String(payload.name ?? "command"), ok: payload.ok === true,
        text: `/${String(payload.name ?? "command")} · ${payload.ok === false ? String(payload.error ?? "Refused") : payload.data?.applies === "after_turn" ? "Accepted · switches after this turn" : typeof payload.data?.message === "string" ? payload.data.message : JSON.stringify(payload.data ?? "Done", null, 2)}` };
    }
    if (envelope.kind === "session.agent_changed") {
      this.state.agent = String(payload.agent ?? "build");
      return { type: "agent", name: this.state.agent };
    }
    if (envelope.kind === "context.compacted") return { type: "notice", text: `Context compacted: ~${payload.estimate_before ?? "?"} → ~${payload.estimate_after ?? "?"} tokens` };
    if (envelope.kind === "model.changed") {
      if (typeof payload.new?.alias === "string") this.state.selectedModel = payload.new.alias;
      if (payload.operation_id === this.state.pendingModel?.operationId) {
        this.state.pendingModel = undefined;
        this.emit({ type: "modelPending" });
      }
      this.emitModels();
      return { type: "notice", text: `Model: ${payload.new?.alias ?? "unknown"}${payload.variant ? ` · ${payload.variant}` : ""}` };
    }
    for (const update of this.state.controls.envelopeUpdates(envelope)) this.emit(update);
    const projected = eventView(envelope, this.state.currentTurn, this.state.controls.calls);
    if (projected?.type === "pause" && envelope.seq <= this.state.previousRunThrough) {
      this.emit(projected);
      return { type: "pauseState", id: projected.id, state: "resolved", message: "Ended with the previous run" };
    }
    if (projected) return projected;
    switch (envelope.kind) {
      case "turn.start": {
        this.state.currentTurn = String(payload.turn_id ?? envelope.seq);
        this.emit({ type: "status", state: "busy" });
        return { type: "turnStart", id: this.state.currentTurn };
      }
      case "llm.delta": {
        // One turn groups its deltas, tools and turnEnd under one id.
        const id = String(payload.turn_id ?? this.state.currentTurn ?? envelope.seq);
        const delta = payload.delta ?? {};
        if (delta.delta_type === "text") {
          this.state.turnTextDelta.add(id);
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
        const id = String(payload.turn_id ?? this.state.currentTurn ?? envelope.seq);
        this.recordUsage(payload.usage ?? payload.message?.usage);
        const message = payload.message ?? {};
        const text = typeof message.text === "string" ? message.text : "";
        const thinking = typeof message.thinking === "string" ? message.thinking : "";
        if (this.state.turnTextDelta.has(id)) return undefined;
        if (!text && !thinking) return undefined;
        if (thinking) this.emit({ type: "reasoning", id, text: thinking });
        return text ? { type: "delta", id, text } : undefined;
      }
      case "tool.pre_call": {
        const id = this.state.currentTurn ?? String(envelope.seq);
        const callId = String(payload.tool_call_id ?? envelope.seq);
        const name = String(payload.name ?? "tool");
        const summary = toolSummary(name, payload.args);
        this.state.toolSummaries.set(callId, summary);
        const evidence = permissionEvidence(payload.permission_gate);
        if (evidence) this.state.toolEvidence.set(callId, evidence);
        return { type: "tool", id, callId, phase: "start", name, summary, evidence };
      }
      case "tool.post_call": {
        const id = this.state.currentTurn ?? String(envelope.seq);
        const callId = String(payload.tool_call_id ?? envelope.seq);
        const name = String(payload.name ?? "tool");
        // The post_call payload carries no args: reuse the pre_call summary
        // so the finished row keeps saying what was touched.
        const summary = toolSummary(name, undefined, this.state.toolSummaries.get(callId));
        this.state.toolSummaries.delete(callId);
        // A non-zero shell exit is reported as status error, but it is
        // often harmless: `grep` finding nothing exits 1, and in a `;` chain
        // the last command's code decides for all the output above it. Show
        // the code so the driver can judge (observed 2026-09-30: a
        // `wc; ls; cat …` chain marked failed although its output was fine).
        const exitCode = payload.metadata?.exit_code;
        const shown = typeof exitCode === "number" && exitCode !== 0 ? `${summary} (exit ${exitCode})` : summary;
        const evidence = permissionEvidence(payload.metadata?.permission_gate) ?? this.state.toolEvidence.get(callId);
        this.state.toolEvidence.delete(callId);
        return { type: "tool", id, callId, phase: "end", name, summary: shown, ok: payload.status === "ok", evidence };
      }
      case "turn.end": {
        const id = String(payload.turn_id ?? this.state.currentTurn ?? envelope.seq);
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
        this.state.currentTurn = undefined;
        this.emit({ type: "status", state: outcome === "error" ? "error" : "ready" });
        return message;
      }
      default:
        return undefined;
    }
  }

  /** callId -> the pre_call summary, so post_call rows keep their detail. */

  private emitForState(origin: ConversationState, message: HostToWebview): void {
    if (origin === this.state) this.emit(message);
    else {
      const tagged = { ...message, generation: origin.streamGeneration };
      origin.controls.apply(tagged);
      origin.transcript.push(tagged);
    }
  }

  private emit(message: HostToWebview): void {
    if (message.generation !== undefined && message.generation !== this.state.streamGeneration) return;
    message = { ...message, generation: this.state.streamGeneration };
    if (message.type === "permission" && this.state.session) this.state.controlOwners.set(`permission:${message.card.requestId}`, { session: this.state.session, generation: this.state.streamGeneration });
    if (message.type === "pause" && this.state.session) this.state.controlOwners.set(message.id, { session: this.state.session, generation: this.state.streamGeneration });
    if (message.type === "permissionState" && !["pending", "error"].includes(message.resolution)) {
      this.state.pendingControls.delete(`${this.currentConversationId()}:permission:${message.requestId}`);
    }
    this.state.controls.apply(message);
    // Conversation-list, model and usage metadata is pushed, never replayed
    // as transcript (the view caches the last ones for a reopen).
    const ephemeral =
      message.type === "history" ||
      message.type === "conversations" ||
      message.type === "conversationOpened" ||
      message.type === "models" ||
      message.type === "usage";
    if (message.type === "sendResult") {
      for (const listener of this.listeners) listener(message);
      return;
    }
    if (!ephemeral) this.state.transcript.push(message);
    for (const listener of this.listeners) listener(message);
  }

  private errorText(error: unknown): string {
    if (error instanceof Error) return error.message;
    return String(error);
  }

  private bindControlOwners(): void {
    this.state.controlOwners.clear();
    const session = this.currentConversationId();
    if (!session) return;
    for (const id of this.state.controls.permissions.keys()) this.state.controlOwners.set(`permission:${id}`, { session, generation: this.state.streamGeneration });
    for (const id of this.state.controls.pauses.keys()) this.state.controlOwners.set(id, { session, generation: this.state.streamGeneration });
  }

  private async request(path: string, init: RequestInit, scope: "conversation" | "endpoint" | "operation" = "conversation"): Promise<any> {
    const generation = this.state.streamGeneration;
    const endpoint = this.endpoint;
    const check = (): void => {
      if (endpoint !== this.endpoint || this.disposed) throw new Error("Server endpoint changed");
      if (scope === "conversation") this.checkGeneration(generation);
    };
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
      check();
      const message = this.errorText(error);
      this.emit({ type: "status", state: "error", message });
      throw new Error(`cognition server unreachable: ${message}`);
    }
    check();
    if (response.status === 401) {
      const message = "cognition server rejected the token (401)";
      this.emit({ type: "status", state: "error", message });
      throw new HttpError(response.status, message);
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
      throw new HttpError(response.status, message);
    }
    // DELETE answers 204 and other routes may answer empty: undefined, not a
    // JSON parse crash.
    if (response.status === 204) return undefined;
    const raw = await response.text();
    check();
    return raw ? JSON.parse(raw) : undefined;
  }
}

class HttpError extends Error {
  constructor(readonly status: number, message: string) { super(message); }
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

/** `GET /v1/models` body -> the picker's list; malformed entries are dropped. */
function parseModelList(body: any): ModelInfo[] {
  const models = Array.isArray(body?.models) ? body.models : [];
  return models
    .filter((m: any) => m && typeof m.alias === "string" && typeof m.model === "string")
    .map((m: any) => ({
      alias: m.alias,
      model: m.model,
      contextWindow: typeof m.context_window === "number" ? m.context_window : undefined,
      variants: Array.isArray(m.variants) ? m.variants.filter((v: unknown) => typeof v === "string") : [],
      defaultVariant: typeof m.default_variant === "string" ? m.default_variant : undefined,
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
