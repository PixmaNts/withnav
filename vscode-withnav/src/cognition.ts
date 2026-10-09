import * as vscode from "vscode";
import { ContextSnapshot, parseContext } from "./contextMeter";
import { ChatBackend, ConversationSummary, HostToWebview, ModelInfo, PermissionDecision } from "./chatProtocol";
import { ConversationControls, eventView, permissionCall, PermissionCallReplay, permissionEvidence, WireEnvelope } from "./eventView";
import type { ServerInfoParams } from "./protocol";
import { CLIENT_COMMANDS, routeComposer } from "./commands";
import type { CommandInfo } from "./chatProtocol";
import type { ComposerOperation } from "./chatProtocol";
import type { ChildInfo } from "./chatProtocol";
import type { AgentInfo, LayerInfo, Selection } from "./chatProtocol";

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
 * in-process session server (`withnav/serverInfo`), this class creates a
 * editor session on first use, posts the Captain's messages and
 * maps the SSE event stream onto the webview's message vocabulary. All
 * networking lives here; the webview never sees HTTP.
 */
class ConversationState {
  bypass = false;
  bypassSeq = -1;
  bypassEvents: { seq: number; enabled: boolean }[] = [];
  incarnation: string | null = null;
  bypassPending = false;
  agentChosen = false;
  modelOverride?: string;
  selection?: Selection;
  compaction?: { request: symbol; operationId: string; pending: boolean; text: string; ok?: boolean };
  title = "Parent conversation";
  children = new Map<string, ChildInfo>();
  childStateCursors = new Map<string, string>();
  childStateSeq = new Map<string, number>();
  childControlTokens = new Map<string, number>();
  parent?: ConversationState;
  readOnly = false;
  starting = false;
  archiveReason?: string;
  modelRequestToken = 0;
  settledModelOps = new Set<string>();
  commands: Map<string, { draft: string; name: string; arguments: string }> = new Map();
  commandResults: Map<string, any> = new Map();
  acceptedMessages: Set<string> = new Set();
  pendingModel?: { name: string; operationId: string; agent: string };
  selectionSeq = -1;
  agent = "build";
  startVariant?: string | null;
  transcript: HostToWebview[] = [];
  session: string | undefined;
  lastSeq: string | undefined;
  delivered: Set<number> = new Set();
  currentTurn: string | undefined;
  streaming: boolean = false;
  selectedModel: string | undefined;
  context: ContextSnapshot | undefined;
  contextEpoch = 0;
  contextRefresh: { session: string; generation: number; pending: boolean; done: Promise<void> } | undefined;
  retiredIncarnations: string[] = [];
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
  childRetryTimers = new Map<string, ReturnType<typeof setTimeout>>();
  replayThrough: number | undefined;
  previousRunThrough: number = -1;
  toolSummaries: Map<string, string> = new Map();
  toolEvidence: Map<string, NonNullable<Extract<HostToWebview, { type: "tool" }>["evidence"]>> = new Map();
}

export class CognitionBackend implements ChatBackend {
  private bypassConfirmations = new Set<string>();
  publishBypass(): void {
    this.emit({ type: "bypass", enabled: this.state.bypass, pending: this.state.bypassPending,
      disabled: this.state.readOnly || this.state.starting });
  }
  private acceptBypassSnapshot(summary: any): void {
    if (typeof summary?.bypass !== "boolean" || !Number.isSafeInteger(summary.bypass_seq) || summary.bypass_seq < 0 ||
        !(typeof summary.incarnation === "string" || summary.incarnation === null)) return;
    if (summary.bypass_seq < this.state.bypassSeq) return;
    this.state.bypass = summary.bypass;
    this.state.bypassSeq = summary.bypass_seq;
    this.state.incarnation = summary.incarnation;
    for (const event of this.state.bypassEvents) if (event.seq > this.state.bypassSeq) {
      this.state.bypass = event.enabled; this.state.bypassSeq = event.seq;
    }
    this.state.bypassEvents = [];
    this.publishBypass();
  }
  async toggleBypassPermissions(): Promise<void> { await this.setBypassPermissions(!this.state.bypass); }
  private async setBypassPermissions(enabled: boolean, operation?: ComposerOperation): Promise<void> {
    const origin = this.state;
    if (origin.readOnly || origin.starting) throw new Error("Bypass controls are unavailable in this view.");
    if (origin.bypassPending) throw new Error("Bypass change is already pending.");
    origin.bypassPending = true; this.publishBypass();
    try {
      const { session, generation } = await this.ensureSession(operation);
      const summary = await this.reconcileSession(session, generation);
      if (origin !== this.state) throw new Error("Conversation changed.");
      this.acceptBypassSnapshot(summary);
      if (!origin.incarnation) throw new Error("The server did not provide a live runtime incarnation.");
      const key = JSON.stringify([session, origin.incarnation]);
      if (enabled && !this.bypassConfirmations.has(key)) {
        const answer = await vscode.window.showWarningMessage("Bypass ordinary permission prompts; protected requests still ask", { modal: true }, "Enable bypass");
        this.checkGeneration(generation);
        if (origin !== this.state) throw new Error("Conversation changed.");
        if (key !== JSON.stringify([session, origin.incarnation])) throw new Error("Runtime changed; try again.");
        if (answer !== "Enable bypass") throw new Error("Bypass enable cancelled.");
        this.bypassConfirmations.add(key);
      }
      const result = await this.request(`/v1/sessions/${encodeURIComponent(session)}/bypass`, { method: "POST", body: JSON.stringify({ enabled }) }, "operation");
      if (origin !== this.state || generation !== this.currentGeneration()) return;
      if (typeof result?.enabled !== "boolean" || !Number.isSafeInteger(result.bypass_seq)) throw new Error("Invalid bypass acknowledgement.");
      if (result.bypass_seq >= origin.bypassSeq) { origin.bypass = result.enabled; origin.bypassSeq = result.bypass_seq; }
    } finally { origin.bypassPending = false; if (origin === this.state) this.publishBypass(); }
  }
  private agents: AgentInfo[] = [];
  private layers: LayerInfo[] = [];
  private defaultAgent = "build";
  private assignmentToken = 0;
  private recomputeInitialSelection(): void {
    if (this.currentConversationId()) return;
    const base = this.agents.find(a => a.id === this.state.agent)?.assignment ?? this.agents.find(a => a.id === this.state.agent)?.effective;
    const model = this.state.modelOverride ?? base?.model;
    if (!model) return;
    const catalogDefault = this.models.find(m => m.name === model)?.defaultVariant ?? null;
    const inheritedVariant = this.state.modelOverride ? catalogDefault : base && Object.hasOwn(base, "variant") ? base.variant : catalogDefault;
    this.state.selectedModel = model;
    this.state.selection = {
      model, source: this.state.modelOverride ? "override" : base?.source ?? "config",
      variant: this.state.startVariant !== undefined ? this.state.startVariant : inheritedVariant,
      variant_source: this.state.startVariant !== undefined ? "override" : this.state.modelOverride ? "model_default" : base?.variant_source ?? "model_default",
    };
  }
  private emitAssignments(): void {
    this.emit({ type: "assignments", agents: this.agents, layers: this.layers, current: this.state.agent,
      models: this.models.filter(m => m.api === "chat" && m.enabled !== false), locked: this.state.starting || this.state.readOnly, variant_source: this.state.selection?.variant_source, model: this.state.selectedModel, variant: this.state.selection?.variant, source: this.state.modelOverride ? "override" : this.state.selection?.source });
  }
  async refreshAssignments(): Promise<void> {
    if (this.isReadOnly() || !this.endpoint) return;
    const generation = this.currentGeneration();
    const session = this.currentConversationId();
    const token = ++this.assignmentToken;
    const [agents, layers] = await Promise.all([
      this.request(`/v1/agents?${this.workspaceQuery()}${session ? `&session_id=${encodeURIComponent(session)}` : ""}`, { method: "GET" }),
      this.request(`/v1/layers?${this.workspaceQuery()}`, { method: "GET" }),
    ]);
    this.checkGeneration(generation);
    if (token !== this.assignmentToken || session !== this.currentConversationId()) return;
    this.agents = (Array.isArray(agents?.agents) ? agents.agents : []).filter((a: any) => typeof a?.id === "string" && typeof a.description === "string" && ["primary", "all"].includes(a.mode));
    this.layers = (Array.isArray(layers?.layers) ? layers.layers : []).filter((l: any) => ["reflex", "instinct", "metacognition"].includes(l?.layer) && ["usable", "disabled"].includes(l.status));
    this.defaultAgent = typeof agents?.default_agent === "string" ? agents.default_agent : "build";
    if (!session) {
      if (!this.state.agentChosen || !this.agents.some(a => a.id === this.state.agent)) this.state.agent = this.defaultAgent;
      this.recomputeInitialSelection();
    }
    this.emitAssignments(); this.emitModels();
  }
  async selectAgent(id: string): Promise<void> {
    if (this.isReadOnly() || this.state.starting) throw new Error("Agent selection unavailable here.");
    if (!this.agents.some(a => a.id === id)) throw new Error("Unknown primary agent");
    if (this.currentConversationId()) { await this.runComposerCommand(`/agent ${id}`); return; }
    this.state.agent = id; this.state.modelOverride = undefined; this.state.startVariant = undefined;
    this.state.agentChosen = true;
    this.state.selection = this.agents.find(a => a.id === id)?.effective ?? undefined;
    this.state.selectedModel = this.state.selection?.model;
    this.recomputeInitialSelection();
    this.emit({ type: "agent", name: id }); this.emitAssignments(); this.emitModels();
  }
  publishCompaction(): void { if (this.state.compaction) { const { request: _request, ...view } = this.state.compaction; this.emit({ type: "compaction", ...view }); } }
  publishSubagents(): void { this.emit({ type: "viewMode", readOnly: this.state.readOnly, archiveReason: this.state.archiveReason }); if (!this.state.archiveReason) this.emitChildren(); }
  isReadOnly(): boolean { return this.state.readOnly; }
  private emitChildren(): void {
    if (this.state.archiveReason) { this.emit({ type: "viewMode", readOnly: true, archiveReason: this.state.archiveReason }); return; }
    const parent = this.parentState();
    if (parent.session ?? parent.resumeId) this.emit({ type: "subagents", parentId: (parent.session ?? parent.resumeId)!, items: [...parent.children.values()].map(c => ({ ...c })) });
    this.emit({ type: "childView", child: this.state.readOnly ? parent.children.get(this.state.session!) : undefined,
      parentId: this.state.readOnly ? parent.session ?? parent.resumeId : undefined, parentTitle: parent.title });
  }
  private validateChild(parentId: string, childId: string, generation: number): { parent: ConversationState; child: ChildInfo } {
    this.checkGeneration(generation);
    const parent = this.parentState();
    const child = parent.children.get(childId);
    if ((parent.session ?? parent.resumeId) !== parentId || !child) throw new Error("This subagent does not belong to the open parent conversation.");
    return { parent, child };
  }
  private panels = new Set<CognitionBackend>();
  private panelOwner?: CognitionBackend;
  private terminalChild = false;
  private childReconciliations = new Map<string, { abort: AbortController; trailing: boolean; done: Promise<void> }>();
  private parentState(): ConversationState { return this.state.parent ?? this.state; }
  async openChild(parentId: string, childId: string, generation = this.currentGeneration()): Promise<ChatBackend> {
    const { parent } = this.validateChild(parentId, childId, generation);
    const metadata = await this.request(`/v1/sessions/${encodeURIComponent(childId)}`, { method: "GET" });
    this.validateChild(parentId, childId, generation);
    if (metadata?.parent_session_id !== parentId || metadata?.promptable !== false) throw new Error("The server did not confirm a read-only child of this parent.");
    this.seedChild(parent, childId, metadata);
    const panel = new CognitionBackend();
    panel.endpoint = this.endpoint;
    panel.state.session = childId;
    panel.state.parent = parent;
    panel.state.readOnly = true;
    panel.acceptBypassSnapshot(metadata);
    panel.state.streamGeneration = generation;
    panel.panelOwner = this;
    panel.terminalChild = this.isTerminal(parent.children.get(childId)!.state);
    this.panels.add(panel);
    this.emitChildren();
    return panel;
  }
  private isTerminal(status: ChildInfo["state"]): boolean { return ["done", "failed", "stopped"].includes(status); }
  private seedChild(parent: ConversationState, id: string, metadata: any): void {
    const child = parent.children.get(id);
    if (!child) return;
    const status = this.childStatus(metadata?.child_status);
    if (this.isTerminal(status) || (status !== "unknown" && !this.isTerminal(child.state) &&
        !(status === "running" && ["permission", "budget", "watchdog"].includes(child.state)))) child.state = status;
    if (typeof metadata?.agent === "string") child.agent = metadata.agent;
    child.revision = (child.revision ?? 0) + 1;
    this.emitChildren();
    this.panelOwner?.emitChildren();
  }
  captureChildControl(parentId: string, childId: string, action: "stop" | "continue", generation = this.currentGeneration()): object {
    if (this.panelOwner) return this.panelOwner.captureChildControl(parentId, childId, action, generation);
    const { parent, child } = this.validateChild(parentId, childId, generation);
    if (child.pending || child.state === "unknown" || this.isTerminal(child.state) ||
        (action === "continue" && !["budget", "watchdog"].includes(child.state))) throw new Error("This subagent cannot perform that action now.");
    return { parent, child, revision: child.revision ?? 0, state: child.state, action, token: parent.childControlTokens.get(childId) ?? 0 };
  }
  async controlChild(parentId: string, childId: string, action: "stop" | "continue", generation = this.currentGeneration(), descriptor?: object): Promise<void> {
    if (this.panelOwner) return this.panelOwner.controlChild(parentId, childId, action, generation, descriptor);
    const { parent, child } = this.validateChild(parentId, childId, generation);
    if (descriptor) {
      const d = descriptor as any;
      if (d.parent !== parent || d.child !== child || d.revision !== (child.revision ?? 0) || d.state !== child.state || d.action !== action || d.token !== (parent.childControlTokens.get(childId) ?? 0)) throw new Error("The subagent changed, try again.");
    }
    if (!["stop", "continue"].includes(action) || ["done", "failed", "stopped"].includes(child.state) ||
        (action === "continue" && !["budget", "watchdog"].includes(child.state))) throw new Error("This subagent cannot perform that action now.");
    if (child.pending) return;
    parent.childControlTokens.set(childId, (parent.childControlTokens.get(childId) ?? 0) + 1);
    child.pending = true; child.error = undefined; this.emitChildren();
    try {
      const result = await this.request(action === "stop" ? `/v1/sessions/${encodeURIComponent(childId)}/stop` :
        `/v1/sessions/${encodeURIComponent(parentId)}/children/${encodeURIComponent(childId)}/continue`,
      { method: "POST", ...(action === "continue" ? { body: JSON.stringify({ pause_token: null }) } : {}) }, "operation");
      if (result?.accepted !== true) throw new Error(String(result?.error ?? `${action} refused: ${result?.actor_state ?? "not accepted"}`));

    } catch (error) { child.error = this.errorText(error); }
    finally { child.pending = false; if (parent === this.parentState() && generation === this.currentGeneration()) this.emitChildren(); }
  }
  private state = new ConversationState();
  private nextGeneration = 0;
  private endpoint?: ServerInfoParams;
  /**
   * An authenticated request for the settings page. The page may open before
   * withnav-lsp announces its server: wait for it a while, then say so rather
   * than leave the page loading forever.
   */
  async settingsRequest(path: string, init: RequestInit = {}, signal?: AbortSignal): Promise<Response> {
    if (!this.endpoint) await this.waitForEndpoint(signal);
    const endpoint = this.endpoint;
    if (!endpoint) throw new Error("the WithNav server has not started (is withnav-lsp running?)");
    const response = await fetch(`http://127.0.0.1:${endpoint.port}${path}`, {
      ...init, headers: { authorization: `Bearer ${endpoint.token}`, "content-type": "application/json" },
      signal: signal ? AbortSignal.any([signal, AbortSignal.timeout(15000)]) : AbortSignal.timeout(15000), redirect: "error",
    });
    if (endpoint !== this.endpoint) throw new Error("WithNav restarted; reload settings");
    return response;
  }
  private listeners = new Set<(message: HostToWebview) => void>();
  /** Last delivered seq, sent back as `Last-Event-ID` on reconnect. */
  /** Envelope seqs already delivered, so a resync's fresh full replay does
   *  not duplicate messages already in the transcript. */
  /** Saved conversations of the workspace, newest first (last listing). */
  private conversations: ConversationSummary[] = [];
  /** Models from GET /v1/models, and the name the next new conversation uses
   *  (or the open conversation's, once it exists). */
  private models: ModelInfo[] = [];
  private commands: CommandInfo[] = [];

  async refreshCommands(): Promise<void> {
    this.emit({ type: "commands", items: [...this.commands, ...CLIENT_COMMANDS.filter(c => !this.commands.some(s => s.name === c.name))] });
    if (!this.endpoint) return;
    const endpoint = this.endpoint;
    const body = await this.request("/v1/commands", { method: "GET" }, "endpoint");
    if (endpoint !== this.endpoint) return;
    this.commands = (Array.isArray(body?.commands) ? body.commands : []).filter((c: any) =>
      typeof c?.name === "string" && /^[a-z][a-z0-9_-]*$/.test(c.name) && typeof c.description === "string" && typeof c.args_hint === "string");
    this.emit({ type: "commands", items: [...this.commands, ...CLIENT_COMMANDS.filter(c => !this.commands.some(s => s.name === c.name))] });
  }

  captureOperation(): ComposerOperation { return { owner: this.state, generation: this.currentGeneration() }; }
  ownsOperation(operation: ComposerOperation): boolean { return !this.disposed && operation.owner === this.state && operation.generation === this.currentGeneration(); }

  async runComposerCommand(draft: string, generation = this.currentGeneration(), operation?: ComposerOperation): Promise<boolean> {
    if (this.isReadOnly()) throw new Error("Subagent transcripts are read-only. Return to the parent to send messages or commands.");
    this.checkGeneration(generation);
    const routed = routeComposer(draft);
    if (routed.kind !== "command") throw new Error("Not a command");
    const { name, arguments: args } = routed;
    if (name === "bypass") {
      if (!["", "on", "off"].includes(args)) throw new Error("Usage: /bypass [on|off]");
      if (args) await this.setBypassPermissions(args === "on", operation);
      else this.emit({ type: "notice", text: `Bypass ${this.state.bypass ? "ON" : "OFF"} · Bypass ordinary permission prompts; protected requests still ask` });
      return true;
    }
    if (this.state.starting && ["model", "variant", "agent", "skill"].includes(name)) throw new Error("Conversation is starting or opening. Try again when it is ready.");
    if (typeof args !== "string") throw new Error("Command arguments must be text");
    if (!this.commands.length) { await this.refreshCommands(); this.checkGeneration(generation); }
    if (![...this.commands, ...CLIENT_COMMANDS].some(c => c.name === name)) throw new Error(`Unknown command: /${name}. Use /help.`);
    if (["help", "skills", "stop"].includes(name) && args) throw new Error(`/${name} takes no arguments`);
    if (name === "help") {
      this.emit({ type: "notice", text: [...this.commands, ...CLIENT_COMMANDS.filter(c => !this.commands.some(s => s.name === c.name))].map(c => `/${c.name} ${c.args_hint} — ${c.description}`).join("\n") });
      return true;
    }
    if (name === "new") {
      if (args && !this.agents.some(a => a.id === args)) throw new Error("Unknown primary agent");
      const reset = this.newConversation();
      const newGeneration = this.currentGeneration();
      await reset;
      this.checkGeneration(newGeneration);
      if (args) await this.selectAgent(args);
      this.emit({ type: "sendResult", text: draft, accepted: true, generation: newGeneration });
      return true;
    }
    if (name === "model" && !args) {
      this.emit({ type: "commandPicker", kind: "model", items: this.models.filter(m => m.api === "chat" && m.enabled !== false).map(m => ({ name: m.name, description: m.upstream_id, gateway: m.gateway, provider_group: m.provider_group })) });
      return true;
    }
    if (name === "agent" && !args) {
      await this.refreshAssignments();
      this.emit({ type: "commandPicker", kind: "agent", items: this.agents.map(a => ({ name: a.id, description: `${a.description} · ${a.effective?.model ?? "Unassigned"}${a.effective?.source === "override" ? " · override" : ""}` })) });
      return true;
    }
    if (name === "variant" && !args && !this.currentConversationId()) {
      const model = this.models.find(m => m.name === this.state.selectedModel);
      this.emit({ type: "commandPicker", kind: "variant", items: [...(model?.variants ?? []), "off", "inherit"].map(name => ({ name, description: name === "off" ? "No variant" : "Model variant" })) });
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
      const match = /^(\S+)(?:\s+([\s\S]*))?$/.exec(args);
      if (!match) throw new Error("Usage: /skill <name> [request]");
      const ensured = await this.ensureSession(operation);
      const session = ensured.session;
      generation = ensured.generation;
      this.checkGeneration(generation);
      const result = await this.request(`/v1/sessions/${encodeURIComponent(session)}/messages`, { method: "POST", body: JSON.stringify({ target_actor_id: ROOT_ACTOR, content: [{ type: "text", text: match[2] || `Apply the ${match[1]} skill.` }, { type: "skill", name: match[1] }] }) });
      this.checkGeneration(generation);
      const operationId = result?.command_id ?? result?.operation_id;
      if (typeof operationId === "string") this.trackCommand(operationId, { draft, name, arguments: args });
      else if (result?.accepted === false || result?.ok === false) throw new Error(String(result.error ?? "Message refused"));
      return typeof operationId !== "string";
    }
    if (name === "stop") { if (!this.state.session) throw new Error("A conversation is needed."); await this.stop(); this.checkGeneration(generation); return true; }
    if (!this.state.session && !this.state.resumeId) {
      if (!["model", "variant", "agent"].includes(name)) throw new Error("A conversation is needed. Send a message first.");
      if (name === "model") { if (!this.models.some(m => m.name === args)) throw new Error("Unknown model name"); this.selectModel(args); return true; }
      if (name === "variant") {
        const model = this.models.find(m => m.name === this.state.selectedModel);
        if (!["off", "inherit"].includes(args) && !model?.variants?.includes(args)) throw new Error("Unknown variant for this model");
        this.state.startVariant = args === "inherit" ? undefined : args === "off" ? null : args;
        this.recomputeInitialSelection();
        this.emitAssignments();
        this.emit({ type: "notice", text: `Starting variant: ${args}` });
        return true;
      }
      if (!args || /\s/.test(args)) throw new Error("Usage: /agent <name>");
      await this.selectAgent(args);
      return true;
    }
    if (!this.state.session) throw new Error("The conversation is stopped. Send a message to resume it first.");
    if (name === "model" && this.hasOpenPermissions()) throw new Error("Answer the pending permission before switching model.");
    const session = this.state.session;
    const origin = this.state;
    const requestAgent = origin.agent;
    const compactRequest = name === "compact" ? Symbol("compact") : undefined;
    if (name === "compact") {
      if (origin.compaction?.pending) throw new Error("Compaction is already pending in this conversation.");
      origin.compaction = { request: compactRequest!, operationId: `request:${generation}:${Date.now()}`, pending: true, text: "Compacting the conversation… Messages you send now are queued until compaction ends." };
      this.emitForState(origin, { type: "compaction", operationId: origin.compaction.operationId, pending: origin.compaction.pending, text: origin.compaction.text, ok: origin.compaction.ok });
    }
    const modelToken = name === "model" ? ++origin.modelRequestToken : undefined;
    if (name === "model") { origin.pendingModel = undefined; this.emitForState(origin, { type: "modelPending" }); }
    let result: any;
    try { result = await this.request(`/v1/sessions/${encodeURIComponent(session)}/commands`, { method: "POST", body: JSON.stringify({ name, arguments: args }) }, "operation"); }
    catch (error) {
      if (name === "compact" && origin.compaction && origin.compaction.request === compactRequest && origin.compaction.pending) {
        origin.compaction = { ...origin.compaction, pending: false, ok: false, text: `Compaction failed: ${this.errorText(error)}` };
        this.emitForState(origin, { type: "compaction", operationId: origin.compaction.operationId, pending: origin.compaction.pending, text: origin.compaction.text, ok: origin.compaction.ok });
      }
      throw error;
    }
    if (session !== origin.session) throw new Error("Conversation changed");
    if (typeof result?.operation_id !== "string") throw new Error("Command returned no operation id");
    if (name === "compact" && origin.compaction && origin.compaction.request === compactRequest && origin.compaction.pending) {
      origin.compaction.operationId = result.operation_id;
      this.emitForState(origin, { type: "compaction", operationId: origin.compaction.operationId, pending: origin.compaction.pending, text: origin.compaction.text, ok: origin.compaction.ok });
    }
    if (name === "model" && requestAgent === origin.agent && modelToken === origin.modelRequestToken && !origin.settledModelOps.has(result.operation_id)) {
      origin.pendingModel = { name: args, operationId: result.operation_id, agent: requestAgent };
      this.emitForState(origin, { type: "modelPending", name: args });
    }
    if (origin === this.state) this.trackCommand(result.operation_id, { draft, name, arguments: args });
    else origin.commands.set(result.operation_id, { draft, name, arguments: args });
    return false; // Only the durable outcome consumes the draft.
  }

  private trackCommand(operation: string, command: { draft: string; name: string; arguments: string }): void {
    this.state.commands.set(operation, command);
    const outcome = this.state.commandResults.get(operation);
    if (outcome) this.settleCommand(outcome);
    else if (command.name === "skill" && this.state.acceptedMessages.has(operation)) this.emit({ type: "sendResult", text: command.draft, accepted: true });
  }

  private settleCommand(p: any, seq?: number): void {
    const operation = p.operation_id;
    if (typeof operation !== "string") return;
    this.state.commandResults.set(operation, p);
    if (p.ok === true && p.data?.selection) this.applySelection(p.data.selection, seq);
    if (p.name === "compact" && this.state.compaction?.pending && this.state.compaction.operationId === operation) {
      const deferred = p.ok === true && p.data?.accepted === true && p.data?.applies === "after_turn";
      this.state.compaction = { ...this.state.compaction, pending: deferred, ok: p.ok === true,
        text: deferred ? "Compacting the conversation… · waiting for this turn to finish. Messages you send now are queued until compaction ends." : p.ok === false ? `Compaction failed: ${p.error ?? "Refused"}` : "Conversation compacted." };
      this.publishCompaction();
    }
    const command = this.state.commands.get(operation);
    if (command) {
      this.emit({ type: "sendResult", text: command.draft, accepted: p.ok === true, restoreDraft: command.name === "skill" && p.ok !== true });
      if (p.ok !== true) this.emit({ type: "commandError", text: String(p.error ?? "Command refused. Your draft was kept.") });
      this.state.commands.delete(operation);
    }
    const deferred = p.ok === true && p.data?.accepted === true && p.data?.applies === "after_turn";
    if (p.name === "model" && !deferred) this.state.settledModelOps.add(operation);
    if (command?.name === "variant" && !command.arguments && p.ok === true && Array.isArray(p.data?.offered)) {
      this.emit({ type: "commandPicker", kind: "variant", items: [...p.data.offered.filter((v: unknown) => typeof v === "string"), "off", "inherit"].map(name => ({ name, description: name === (p.data.variant ?? "off") ? "Current variant" : "Model variant" })) });
    }
    if (this.state.pendingModel?.operationId === operation && !deferred) {
      this.state.pendingModel = undefined;
      this.emit({ type: "modelPending" });
    }
  }
  /** Default name the server announced, restored by "new conversation". */
  /** Tokens of the last model call, re-sent when the model or list changes. */
  /** A stopped conversation to resume on the next send (Stop keeps it). */
  /** Cancels the SSE fetch of the running stream, if any. */
  /** Bumped by every deliberate stream cancel: stale connects go quiet. */
  /** Turn/step keys that received live text, so response replay cannot duplicate it. */
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
      this.terminalChild = false;
      this.abortStream();
      const generation = this.currentGeneration();
      this.state = new ConversationState();
      this.state.streamGeneration = generation;
      this.conversations = [];
      this.emit({ type: "conversationOpened", title: "Server restarted" });
      this.emit({ type: "history", items: [] });
      this.emit({ type: "status", state: "connecting" });
    }
    this.endpoint = info;
    if (first || restarted) {
      this.commands = [];
      void this.refreshCommands().catch(() => undefined);
    }
    if (this.state.session) {
      void this.connect();
      return;
    }
    if (first || restarted) {
      void this.fetchModels();
      void this.refreshAssignments().catch(() => undefined);
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
    const parent = this.parentState();
    const listedIds = new Set<string>((Array.isArray(body?.sessions) ? body.sessions : []).map((s: any) => s.id));
    for (const childId of parent.children.keys()) {
      if (!listedIds.has(childId)) parent.children.delete(childId);
    }
    for (const s of Array.isArray(body?.sessions) ? body.sessions : []) {
      if (typeof s.id === "string" && s.parent_session_id === (parent.session ?? parent.resumeId) && s.promptable === false) {
        if (!parent.children.has(s.id)) parent.children.set(s.id, { id: s.id, agent: typeof s.agent === "string" ? s.agent : "subagent", goal: "", state: this.childStatus(s.child_status) });
        else if (this.isTerminal(this.childStatus(s.child_status))) this.seedChild(parent, s.id, s);
      }
    }
    this.emitChildren();
    if (!this.isReadOnly()) this.resumePermissionReads();
    this.conversations = parseSessionList(body);
    this.emitConversations();
    return [...this.conversations];
  }

  async openConversation(id: string): Promise<void> {
    const previous = this.state;
    const previousTerminal = this.terminalChild;
    this.terminalChild = false;
    this.abortStream();
    // Its create/open operation is now abandoned. A rollback must restore an
    // idle origin, not depend on that obsolete operation's cleanup.
    previous.starting = false;
    const generation = this.state.streamGeneration;
    this.state = new ConversationState();
    this.state.streamGeneration = generation;
    this.state.starting = true;
    this.emitModels();
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
      if (before?.parent_session_id || before?.promptable === false) throw new Error("Open subagents from their parent; they cannot be resumed.");
      if (before?.read_only === true) {
        this.state.session = id; this.state.readOnly = true; this.state.starting = false;
        this.emit({ type: "conversationOpened", id, title: `${before.title ?? id} · archive` });
        this.emit({ type: "history", items: [] });
        this.state.archiveReason = String(before.resume_disabled_reason ?? "Archived history is read-only");
        this.publishSubagents();
        this.emit({ type: "status", state: "ready" });
        this.terminalChild = true;
        void this.connect();
        return;
      }
      if (typeof before.agent === "string") {
        this.state.agent = before.agent;

      }
      this.state.previousRunThrough = before.actor_state === "stopped" ? before.current_seq : -1;
      // Resume first: an unknown id (404) must leave the current view intact.
      const created = await this.request("/v1/sessions", {
        method: "POST",
        body: JSON.stringify({ workspace: this.workspacePath(), resume: id }),
      });
      const sessionId = String(created?.session_id ?? "");
      this.checkGeneration(generation);
      if (!sessionId) throw new Error("cognition server: resume returned no id");
      // Stop streaming the previous conversation before switching.
      this.state.session = sessionId;
      this.state.starting = false;
      this.emitModels();
      this.state.resumeId = undefined;
      this.state.replayThrough = Infinity;
      this.emit({ type: "conversationOpened", id: sessionId, title: summary?.title ?? sessionId });
      this.emit({ type: "agent", name: this.state.agent });
      this.emit({ type: "history", items: [] });
      this.emit({ type: "status", state: "ready" });
      void this.loadSessionModel(sessionId);
      void this.fetchContext();
      // No Last-Event-ID: the stored events replay the whole conversation.
      void this.connect();
      void this.listConversations().catch(() => undefined);
    } catch (error) {
      if (generation === this.state.streamGeneration) {
        this.state = previous;
        this.terminalChild = previousTerminal;
        this.state.streamGeneration = generation;
        this.state.transcript = this.state.transcript.map(message => ({ ...message, generation }));
        this.bindControlOwners();
        this.emit({ type: "conversationOpened", id: this.currentConversationId(), title: "Conversation" });
        this.emitChildren();
        this.emit({ type: "history", items: this.history() });
        this.emitModels();
        this.emit({ type: "modelPending", name: this.state.pendingModel?.name });
        for (const result of this.state.commandResults.values()) this.settleCommand(result);
        this.publishCompaction();
        if (this.state.context) this.emit({ type: "usage", snapshot: this.state.context });
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
    const generation = this.currentGeneration();
    this.state = new ConversationState();
    this.terminalChild = false;
    this.state.streamGeneration = generation;
    this.agents = this.agents.map(agent => ({ ...agent, effective: agent.assignment ?? (agent.effective?.source === "override" ? null : agent.effective) }));
    this.state.agent = this.defaultAgent;
    this.state.selection = this.agents.find(a => a.id === this.defaultAgent)?.effective ?? undefined;
    this.state.selectedModel = this.state.selection?.model;
    this.emit({ type: "conversationOpened", title: "New conversation" });
    this.recomputeInitialSelection();
    this.emit({ type: "history", items: [] });
    this.emit({ type: "status", state: "ready" });
    this.emitConversations();
    this.emitModels();
    this.emitAssignments();
  }

  async deleteConversation(id: string): Promise<void> {
    try {
      await this.request(`/v1/sessions/${encodeURIComponent(id)}`, { method: "DELETE" });
    } catch (error) {
      if (!(error instanceof HttpError && error.status === 404)) throw error;
    }
    this.conversations = this.conversations.filter((c) => c.id !== id);
    if (this.state.session === id || this.state.resumeId === id) {
      // The open conversation vanished: land in a fresh one (which pushes
      // the updated list itself).
      await this.newConversation();
    } else {
      this.emitConversations();
    }
    await this.listConversations();
  }

  async send(text: string, context?: string, operation?: ComposerOperation): Promise<void> {
    if (this.isReadOnly()) throw new Error("Subagent transcripts are read-only. Return to the parent.");
    // The Captain's message arrives as `message.received` on the stream; do
    // NOT emit an optimistic copy here (that duplicated it).
    const { session, generation } = await this.ensureSession(operation);
    this.checkGeneration(generation);
    await this.request(`/v1/sessions/${encodeURIComponent(session)}/messages`, {
      method: "POST",
      body: JSON.stringify({
        target_actor_id: ROOT_ACTOR,
        content: [{ type: "text", text: context ? `${text}\n\n${context}` : text }],
      }),
    });
  }

  async stop(): Promise<void> { if (this.isReadOnly()) throw new Error("Use the confirmed subagent Stop control."); await this.stopState(this.state); }

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
    if (origin.compaction?.pending) { origin.compaction = { ...origin.compaction, pending: false, ok: false, text: "Compaction stopped." }; this.emitForState(origin, { type: "compaction", operationId: origin.compaction.operationId, pending: origin.compaction.pending, text: origin.compaction.text, ok: origin.compaction.ok }); }
    this.emitForState(origin, { type: "modelPending" });
    this.emitForState(origin, { type: "status", state: "ready" });
  }

  hasOpenPermissions(): boolean { return this.state.controls.permissions.size > 0; }

  async answerPermission(sessionId: string, requestId: string, decision: PermissionDecision, message?: string, generation = this.state.streamGeneration): Promise<void> {
    if (this.isReadOnly()) throw new Error("Answer subagent permissions in the parent conversation.");
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
    if (this.isReadOnly()) throw new Error("Use the confirmed subagent controls in the parent.");
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

  private waitForEndpoint(signal?: AbortSignal): Promise<void> {
    if (signal?.aborted) return Promise.reject(new Error("Endpoint wait cancelled"));
    if (this.endpoint) return Promise.resolve();
    return new Promise((resolve, reject) => {
      const finish = (error?: Error) => {
        clearInterval(timer);
        clearTimeout(deadline);
        signal?.removeEventListener("abort", cancel);
        if (error) reject(error);
        else resolve();
      };
      const cancel = () => finish(new Error("Endpoint wait cancelled"));
      const timer = setInterval(() => {
        if (this.endpoint) finish();
      }, 100);
      const deadline = setTimeout(() => finish(new Error("WithNav's language server is not running")), 30000);
      signal?.addEventListener("abort", cancel, { once: true });
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
      if (typeof body?.default_agent === "string") this.defaultAgent = body.default_agent;
    } catch {
      if (endpoint !== this.endpoint) return;
      // No model list (older server, unreadable models.json): the picker
      // stays empty and new sessions use the server's default.
      this.models = [];
    }
    this.emitModels();
    this.recomputeInitialSelection(); this.emitAssignments();
  }

  /** A conversation that exists on the server keeps its model (option A). */
  private modelLocked(): boolean {
    return this.state.starting || this.state.session !== undefined || this.state.resumeId !== undefined;
  }

  private emitModels(): void {
    this.emit({
      type: "models",
      items: this.models.filter(m => m.api === "chat" && m.enabled !== false),
      current: this.state.selectedModel,
      locked: this.state.starting,
    });
  }

  selectModel(name: string): void {
    if (this.isReadOnly()) return;
    if (this.state.starting) { this.emit({ type: "commandError", text: "Conversation is starting or opening. Try again when it is ready." }); return; }
    if (this.modelLocked()) {
      const generation = this.currentGeneration();
      const origin = this.state;
      void this.runComposerCommand(`/model ${name}`, generation).catch(error => {
        if (origin === this.state && generation === this.currentGeneration()) this.emit({ type: "commandError", text: this.errorText(error), generation });
      }); return;
    }
    if (!this.models.some((m) => m.name === name)) return;
    this.state.selectedModel = name;
    this.state.modelOverride = name;
    this.state.startVariant = undefined;
    this.recomputeInitialSelection(); this.emitAssignments();
    this.emitModels();
  }

  /** Reads the open conversation's model from GET /v1/sessions/{id}. */
  private async loadSessionModel(id: string): Promise<void> {
    const generation = this.state.streamGeneration;
    try {
      const summary = await this.reconcileSession(id, generation);
      if (id !== this.state.session) return;
      this.applySelection(summary, summary.current_seq);
      void this.refreshAssignments().catch(() => undefined);
      this.state.replayThrough = typeof summary?.current_seq === "number" ? summary.current_seq : Infinity;
    } catch {
      // Keep the current selection: the picker is informative only here.
    }
    if (generation !== this.state.streamGeneration) return;
    if (id === this.state.session) this.drainChildReads();
    this.emitModels();
    this.emitAssignments();
  }

  private async reconcileSession(id: string, generation: number): Promise<any> {
    for (let attempt = 0; attempt < 3; attempt++) {
      this.checkGeneration(generation);
      try {
        const summary = await this.request(`/v1/sessions/${encodeURIComponent(id)}`, { method: "GET" });
        this.checkGeneration(generation);
        if (!Number.isSafeInteger(summary?.current_seq) || summary.current_seq < 0) throw new Error("Invalid session summary");
        this.acceptBypassSnapshot(summary);
        return summary;
      } catch (error) {
        this.checkGeneration(generation);
        this.emit({ type: "status", state: "error", message: "Could not reconcile conversation history; retrying…" });
        if (attempt === 2) throw error;
        await new Promise(resolve => setTimeout(resolve, 100 * (attempt + 1)));
      }
    }
  }

  currentContext(): ContextSnapshot | undefined { return this.state.context; }

  private acceptContext(value: unknown, authoritative = false): void {
    const snapshot = parseContext(value);
    if (!snapshot || !this.currentConversationId()) return;
    const held = this.state.context;
    if (!authoritative && held && held.incarnation !== snapshot.incarnation) {
      void this.fetchContext();
      return;
    }
    if (authoritative) this.state.retiredIncarnations = this.state.retiredIncarnations.filter(id => id !== snapshot.incarnation);
    if (held) {
      if (held.incarnation === snapshot.incarnation && snapshot.revision <= held.revision) return;
      if (held.incarnation && held.incarnation !== snapshot.incarnation) {
        this.state.retiredIncarnations.push(held.incarnation);
        this.state.retiredIncarnations = this.state.retiredIncarnations.slice(-8);
      }
    }
    this.state.context = snapshot;
    this.emit({ type: "usage", snapshot });
  }

  private async fetchContext(): Promise<void> {
    const origin = this.state;
    const session = this.currentConversationId();
    const generation = origin.streamGeneration;
    ++origin.contextEpoch;
    if (!session) return;
    const active = origin.contextRefresh;
    if (active?.session === session && active.generation === generation) {
      active.pending = true;
      return active.done;
    }
    const refresh = { session, generation, pending: false, done: Promise.resolve() };
    origin.contextRefresh = refresh;
    const current = (): boolean => origin === this.state && generation === this.state.streamGeneration && session === this.currentConversationId();
    refresh.done = (async () => {
      do {
        refresh.pending = false;
        const epoch = origin.contextEpoch;
        try {
          const snapshot = await this.request(`/v1/sessions/${encodeURIComponent(session)}/context`, { method: "GET" });
          if (current() && epoch === origin.contextEpoch) this.acceptContext(snapshot, true);
        } catch { /* Keep the last estimate; never substitute last-call usage. */ }
      } while (current() && refresh.pending);
    })().finally(() => {
      if (origin.contextRefresh === refresh) origin.contextRefresh = undefined;
    });
    return refresh.done;
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
    this.state.children.clear();
    this.state.commands.clear();
    this.state.commandResults.clear();
    this.state.acceptedMessages.clear();
    this.state.pendingModel = undefined;
    this.state.settledModelOps.clear();
    this.state.selectionSeq = -1;
    this.state.agent = "build";
    this.state.startVariant = undefined;
    this.state.transcript = [];
    this.state.lastSeq = undefined;
    this.state.delivered.clear();
    this.state.currentTurn = undefined;
    this.state.toolSummaries.clear();
    this.state.toolEvidence.clear();
    this.state.turnTextDelta.clear();
    this.state.context = undefined;
    this.state.contextEpoch++;
    this.state.retiredIncarnations = [];
    this.state.controls.reset();
    this.state.controlOwners.clear();
    this.state.pendingControls.clear();
    this.state.replayThrough = undefined;
    this.state.previousRunThrough = -1;
  }

  /** Stops the SSE fetch of the running stream, if any. */
  private abortStream(): void {
    for (const operation of this.childReconciliations.values()) operation.abort.abort();
    this.childReconciliations.clear();
    for (const panel of this.panels) panel.dispose();
    this.panels.clear();
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
        `http://127.0.0.1:${endpoint.port}/v1/sessions/${encodeURIComponent(session)}/events`,
        { headers, signal: abort.signal },
      );
      if (!response.ok || !response.body) {
        throw new Error(`event stream: HTTP ${response.status}`);
      }
      if (generation !== this.state.streamGeneration) return;
      void this.fetchContext();
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
    if (this.state.readOnly && this.state.session && !this.terminalChild) await this.reconcileChild(this.state.session, this.parentState());
    if (this.state.session !== undefined && !this.terminalChild) {
      setTimeout(() => void this.connect(), RECONNECT_DELAY_MS);
    }
  }

  private async ensureSession(operation?: ComposerOperation): Promise<{ session: string; generation: number }> {
    if (this.isReadOnly()) throw new Error("Subagents cannot be resumed or prompted.");
    if (this.state.starting) throw new Error("Conversation is starting or opening. Try again when it is ready.");
    if (this.state.session) return { session: this.state.session, generation: this.state.streamGeneration };
    this.abortStream();
    const generation = this.state.streamGeneration;
    if (operation && operation.owner === this.state) operation.generation = generation;
    this.state.starting = true;
    this.emitModels();
    try {
    if (!this.state.resumeId) {
      const agent = this.state.agent;
      const selection = this.state.selection;
      const override = this.state.modelOverride;
      const variant = this.state.startVariant;
      this.resetTranscript();
      this.state.agent = agent;
      this.state.startVariant = variant;
      this.state.selection = selection;
      this.state.modelOverride = override;
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
          ? { workspace: this.workspacePath(), resume }
           : { workspace: this.workspacePath(), editor: true, ...(this.state.modelOverride ? { model: this.state.modelOverride } : {}), ...(this.state.agentChosen ? { agent: this.state.agent } : {}), ...(this.state.startVariant !== undefined ? { variant: this.state.startVariant } : {}) },
      ),
    });
    const sessionId = String(created?.session_id ?? "");
    this.checkGeneration(generation);
    if (!sessionId) throw new Error("cognition server: create session returned no id");
    this.state.session = sessionId;
    this.state.starting = false;
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
    void this.fetchContext();
    void this.connect();
    void this.loadSessionModel(sessionId);
    void this.listConversations().catch(() => undefined);
    return { session: sessionId, generation };
    } finally {
      if (generation === this.currentGeneration()) { this.state.starting = false; this.emitModels(); }
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
    if (frame.envelope?.session_id !== undefined && frame.envelope.session_id !== this.currentConversationId()) return;
    if (frame.id) this.state.lastSeq = frame.id;
    if (frame.event === "resync") {
      // The cursor is out of range: drop it, rebuild the view from what the
      // bridge already holds, and let the reconnect's full replay fill any
      // gap (already-delivered seqs are skipped below).
      this.state.lastSeq = undefined;
      this.emit({ type: "history", items: this.history().filter((m) => m.type !== "history") });
      this.publishCompaction();
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
      if (!this.isReadOnly() && message.type === "permission" && message.card.childSessionId) this.scheduleChildRead(message.card.childSessionId);
    }
    if (this.state.replayThrough !== undefined && Number(frame.id) >= this.state.replayThrough) {
      this.state.replayThrough = undefined;
      this.drainChildReads();
    }
  }

  cancelPermissionReads(): void {
    for (const timer of this.state.childRetryTimers.values()) clearTimeout(timer);
    this.state.childRetryTimers.clear();
    if (this.state.childReadTimer) clearTimeout(this.state.childReadTimer);
    this.state.childReadTimer = undefined;
    this.state.childReadQueue.clear();
    this.state.childReadsAttempted.clear();
    for (const abort of this.state.childReads.values()) abort.abort();
    this.state.childReads.clear();
  }

  resumePermissionReads(): void {
    if (this.isReadOnly()) { void this.connect(); return; }
    for (const child of this.state.children.values()) if (!["done", "failed", "stopped"].includes(child.state)) this.scheduleChildRead(child.id);
    const completed = new Set(this.state.transcript.filter(m => m.type === "permissionCall").map(m => m.type === "permissionCall" ? m.requestId : ""));
    for (const card of this.state.controls.permissions.values()) {
      if (card.childSessionId && !completed.has(card.requestId)) this.scheduleChildRead(card.childSessionId);
    }
  }

  dispose(): void {
    this.disposed = true;
    this.panelOwner?.panels.delete(this);
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
      const row = this.state.children.get(child);
      if (!cards.length && (!row || this.state.childReadsAttempted.has(`state:${child}`))) continue;
      this.state.childReadsAttempted.add(`state:${child}`);
      for (const card of cards) this.state.childReadsAttempted.add(card.requestId);
      const abort = new AbortController();
      this.state.childReads.set(child, abort);
      const readGeneration = this.currentGeneration();
      void this.loadChildPermissionCalls(child, cards, abort).finally(() => {
        if (this.currentGeneration() === readGeneration && this.state.childReads.get(child) === abort) {
          this.state.childReads.delete(child);
          const row = this.state.children.get(child);
          if (row && !["done", "failed", "stopped"].includes(row.state)) {
            this.state.childReadsAttempted.delete(`state:${child}`);
            const origin = this.state;
            const generation = this.currentGeneration();
            this.state.childRetryTimers.set(child, setTimeout(() => {
              origin.childRetryTimers.delete(child);
              if (this.state === origin && this.currentGeneration() === generation && !this.disposed) this.scheduleChildRead(child);
            }, RECONNECT_DELAY_MS));
          }
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
      const cursor = !cards.length ? this.state.childStateCursors.get(child) : undefined;
      const response = await fetch(`http://127.0.0.1:${endpoint.port}/v1/sessions/${encodeURIComponent(child)}/events`, {
        headers: { authorization: `Bearer ${endpoint.token}`, ...(cursor ? { "last-event-id": cursor } : {}) }, signal: abort.signal,
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
          const frame = this.parseFrame(raw);
          if (frame?.event === "resync") { this.state.childStateCursors.delete(child); return; }
          const event = frame?.envelope;
          if (!event) continue;
          if (frame?.id && generation === this.currentGeneration()) this.state.childStateCursors.set(child, frame.id);
          if (session === this.state.session && generation === this.currentGeneration()) {
            const last = this.state.childStateSeq.get(child) ?? -1;
            if (typeof event.seq === "number" && event.seq > last) {
              this.state.childStateSeq.set(child, event.seq);
              this.observeChildren({ ...event, payload: { ...event.payload, child_session_id: child } });
            }
          }
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
            if (!remaining.size && cards.length) return;
          }
        }
      }
    } catch { /* The card explicitly says when full arguments are unavailable. */ }
    finally { clearTimeout(timer); abort.signal.removeEventListener("abort", cancel); abort.abort(); await reader?.cancel().catch(() => undefined); }
  }

  // ---- event -> webview mapping ----
  private applySelection(selection: any, seq?: number): void {
    if (seq === undefined || seq <= this.state.selectionSeq) return;
    if (typeof selection?.agent !== "string" || typeof selection.model !== "string" || typeof selection.source !== "string") return;
    if (selection.agent !== this.state.agent && this.state.pendingModel) { this.state.pendingModel = undefined; this.emit({ type: "modelPending" }); }
    this.state.selectionSeq = seq;
    this.state.agent = selection.agent;
    this.state.selectedModel = selection.model;
    this.state.selection = selection;
    this.state.modelOverride = undefined;
    this.emitAssignments();
  }

  private childStatus(status: unknown): ChildInfo["state"] {
    switch (status) {
      case "running": case "active": return "running";
      case "done": case "completed": return "done";
      case "failed": return "failed";
      case "stopped": return "stopped";
      case "waiting_for_permission": case "permission": case "waiting_permission": case "awaiting_permission": return "permission";
      case "out_of_steps": case "step_budget_exhausted": case "parked": return "budget";
      case "doubt": case "watchdog_paused": return "watchdog";
      default: return "unknown";
    }
  }
  private observeChildren(e: WireEnvelope): void {
    const p = e.payload ?? {};
    const parent = this.parentState();
    let id = typeof p.child_session_id === "string" ? p.child_session_id : this.state.readOnly ? this.state.session : undefined;
    if (e.kind.startsWith("subagent.")) {
      if (p.parent_session_id !== undefined && p.parent_session_id !== (parent.session ?? parent.resumeId)) return;
      if (!id) return;
      const child: ChildInfo = parent.children.get(id) ?? { id, agent: "subagent", goal: "", state: "running" };
      if (typeof p.agent === "string") child.agent = p.agent;
      if (typeof p.goal === "string") child.goal = p.goal.slice(0, 500);
      if (e.kind === "subagent.done") { child.state = this.childStatus(p.outcome); child.pauseToken = undefined; }
      else if (e.kind === "subagent.watchdog" && !["done", "failed", "stopped"].includes(child.state)) {
        if (p.verdict === "doubt" || p.verdict === "stuck") child.state = "watchdog";
        if (p.verdict === "active" && child.state === "watchdog") child.state = "running";
      }
      parent.children.set(id, child);
      if (typeof p.reason === "string") child.reason = p.reason.slice(0,500);
      if (typeof p.delivery === "string") child.delivery = p.delivery;
      if (!this.isReadOnly() && e.kind === "subagent.spawned" && !["done", "failed", "stopped"].includes(child.state)) this.scheduleChildRead(id);
    } else {
      if (this.state.readOnly && id && typeof e.seq === "number") {
        if (this.isTerminal(parent.children.get(id)?.state ?? "unknown")) return;
        if (e.seq <= (parent.childStateSeq.get(id) ?? -1)) return;
        parent.childStateSeq.set(id, e.seq);
      }
      if (e.kind === "permission.resolved" && !id) {
        const card = this.state.controls.permissions.get(p.request_id); id = card?.childSessionId;
      }
      const child = id ? parent.children.get(id) : undefined;
      if (!child || ["done", "failed", "stopped"].includes(child.state)) return;
      if (e.kind === "permission.requested") child.state = "permission";
      else if (e.kind === "permission.resolved" || e.kind === "turn.start") { child.state = "running"; child.pauseToken = undefined; }
      else if (e.kind === "agent.yield" && ["budget", "watchdog"].includes(p.kind)) {
        child.state = p.kind === "budget" ? "budget" : "watchdog";
        const token = p.details?.pause_token;
        child.pauseToken = typeof token?.incarnation === "string" && typeof token?.hold_id === "string" ? token : undefined;
      } else if (e.kind === "command.result" && p.name === "watchdog" && p.data?.resolution &&
          JSON.stringify(p.data.pause_token) === JSON.stringify(child.pauseToken)) { child.state = "running"; child.pauseToken = undefined; }
      else if (e.kind === "session.end" || (e.kind === "turn.end" && ["done", "error"].includes(p.outcome))) { void this.reconcileChild(id!, parent); return; }
      else return;
    }
    if (id) {
      const child = parent.children.get(id);
      if (child) child.revision = (child.revision ?? 0) + 1;
    }
    this.emitChildren();
    this.panelOwner?.emitChildren();
  }

  private reconcileChild(id: string, parent: ConversationState): Promise<void> {
    const existing = this.childReconciliations.get(id);
    if (existing) { existing.trailing = true; return existing.done; }
    const generation = this.currentGeneration();
    const operation = { abort: new AbortController(), trailing: false, done: Promise.resolve() };
    operation.done = (async () => {
      // One active lookup and at most one trailing batch, each with bounded retries.
      for (let batch = 0; batch < 2; batch++) {
        for (let attempt = 0; attempt < 3; attempt++) {
          const timeout = setTimeout(() => operation.abort.abort(), 2000);
          try {
            const metadata = await this.request(`/v1/sessions/${encodeURIComponent(id)}`, { method: "GET", signal: operation.abort.signal });
            if (operation.abort.signal.aborted || generation !== this.currentGeneration()) return;
            this.seedChild(parent, id, metadata);
            const terminal = this.isTerminal(parent.children.get(id)!.state);
            if (this.state.readOnly) this.terminalChild = terminal;
            if (terminal) break;
          } catch { if (operation.abort.signal.aborted || generation !== this.currentGeneration()) return; }
          finally { clearTimeout(timeout); }
          if (attempt < 2) await new Promise<void>(resolve => {
            const timer = setTimeout(resolve, 100 * (attempt + 1));
            operation.abort.signal.addEventListener("abort", () => { clearTimeout(timer); resolve(); }, { once: true });
          });
          if (operation.abort.signal.aborted) return;
        }
        if (!operation.trailing || operation.abort.signal.aborted) break;
        operation.trailing = false;
      }
    })().finally(() => { if (this.childReconciliations.get(id) === operation) this.childReconciliations.delete(id); });
    this.childReconciliations.set(id, operation);
    return operation.done;
  }

  private mapEvent(envelope: WireEnvelope): HostToWebview | undefined {
    const payload = envelope.payload ?? {};
    this.observeChildren(envelope);
    // Refresh context even when these events also project a notice.
    if (envelope.kind === "context.compacted" || envelope.kind === "model.changed") void this.fetchContext();
    for (const update of this.state.controls.envelopeUpdates(envelope)) this.emit(update);
    if (envelope.kind === "message.received" && typeof payload.command_id === "string") {
      this.state.acceptedMessages.add(payload.command_id);
      const command = this.state.commands.get(payload.command_id);
      if (command?.name === "skill") {
        this.emit({ type: "sendResult", text: command.draft, accepted: true });
      }
    }
    if (envelope.kind === "session.end") {
      if (this.state.bypassSeq < 0) this.state.bypassEvents.push({ seq: envelope.seq, enabled: false });
      else if (envelope.seq > this.state.bypassSeq) { this.state.bypass = false; this.state.bypassSeq = envelope.seq; this.state.incarnation = null; this.publishBypass(); }
      if (this.state.compaction?.pending) { this.state.compaction = { ...this.state.compaction, pending: false, ok: false, text: "Compaction stopped with the conversation." }; this.publishCompaction(); }
      this.state.pendingModel = undefined;
      this.emit({ type: "modelPending" });
    }
    if (envelope.kind === "command.result") {
      this.settleCommand(payload, envelope.seq);
      const name = String(payload.name ?? "command");
      const text = outcomeText(name, payload.ok === true, payload.data, payload.error);
      const known = ["help", "model", "context", "variant", "agent", "stop", "new", "compact"].includes(name);
      if (text === null && (known || payload.data?.applies === "now")) return undefined;
      return { type: "notice", operationId: payload.operation_id, name, ok: payload.ok === true,
        text: `/${name} · ${text ?? "done"}`, ...(text === null ? { details: JSON.stringify(payload.data ?? null, null, 2) } : {}) };
    }
    if (envelope.kind === "session.agent_changed") return undefined; // Secondary notification, not effective identity.
    if (envelope.kind === "session.bypass" && typeof payload.enabled === "boolean") {
      if (this.state.bypassSeq < 0) this.state.bypassEvents.push({ seq: envelope.seq, enabled: payload.enabled });
      else if (envelope.seq > this.state.bypassSeq) { this.state.bypass = payload.enabled; this.state.bypassSeq = envelope.seq; this.publishBypass(); }
      return undefined;
    }
    if (envelope.kind === "context.compacted") {
      const text = `Context compacted: ~${payload.estimate_before ?? "?"} → ~${payload.estimate_after ?? "?"} tokens`;
      return { type: "notice", text };
    }
    if (envelope.kind === "model.changed") {
      if (typeof payload.operation_id === "string") this.state.settledModelOps.add(payload.operation_id);
      if (typeof payload.new?.model === "string") { this.applySelection(payload.new, envelope.seq); void this.refreshAssignments().catch(() => undefined); }
      if (payload.operation_id === this.state.pendingModel?.operationId) {
        this.state.pendingModel = undefined;
        this.emit({ type: "modelPending" });
      }
      this.emitModels();
      return { type: "notice", text: `Model: ${payload.new?.model ?? "unknown"}${payload.new?.variant ? ` · ${payload.new.variant}` : ""}` };
    }
    const projected = eventView(envelope, this.state.currentTurn, this.state.controls.calls);
    if (projected?.type === "pause" && envelope.seq <= this.state.previousRunThrough) {
      this.emit(projected);
      return { type: "pauseState", id: projected.id, state: "resolved", message: "Ended with the previous run" };
    }
    if (projected) return projected;
    switch (envelope.kind) {
      case "context.updated":
        this.state.contextEpoch++;
        this.acceptContext(payload);
        return undefined;
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
          this.state.turnTextDelta.add(`${id}:${String(payload.step_id ?? "")}`);
          return { type: "delta", id, stepId: String(payload.step_id ?? ""), text: String(delta.text ?? "") };
        }
        if (delta.delta_type === "thinking") {
          return { type: "reasoning", id, stepId: String(payload.step_id ?? ""), text: String(delta.text ?? "") };
        }
        return undefined; // tool_call deltas ride the tool events instead
      }
      case "llm.response": {
        // During a history replay no `llm.delta` arrives: a turn that got
        // no live text renders its final message as one delta. Live steps
        // (flagged above) must not be duplicated.
        const id = String(payload.turn_id ?? this.state.currentTurn ?? envelope.seq);
        const message = payload.message ?? {};
        const text = typeof message.text === "string" ? message.text : "";
        const thinking = typeof message.thinking === "string" ? message.thinking : "";
        if (this.state.turnTextDelta.has(`${id}:${String(payload.step_id ?? "")}`)) return undefined;
        if (!text && !thinking) return undefined;
        if (thinking) this.emit({ type: "reasoning", id, stepId: String(payload.step_id ?? ""), text: thinking });
        return text ? { type: "delta", id, stepId: String(payload.step_id ?? ""), text } : undefined;
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
        // the code so the Captain can judge (observed 2026-09-30: a
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
    if (message.type === "conversationOpened") this.state.title = message.title;
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
      message.type === "assignments" || message.type === "viewMode" ||
      message.type === "bypass" ||
      message.type === "compaction" ||
      message.type === "subagents" || message.type === "childView" ||
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
    if (message.type === "conversationOpened") this.publishBypass();
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
      if (scope !== "operation") this.emit({ type: "status", state: "error", message });
      throw new Error(`cognition server unreachable: ${message}`);
    }
    check();
    if (response.status === 401) {
      const message = "cognition server rejected the token (401)";
      if (scope !== "operation") this.emit({ type: "status", state: "error", message });
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
      if (scope !== "operation") this.emit({ type: "status", state: "error", message });
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
  const descendants = (id: string): number => {
    const found = new Set<string>([id]);
    let changed = true;
    while (changed) {
      changed = false;
      for (const child of sessions) {
        if (child && typeof child.id === "string" && found.has(child.parent_session_id) && !found.has(child.id)) {
          found.add(child.id); changed = true;
        }
      }
    }
    return found.size - 1;
  };
  return sessions
    .filter((s: any) => s && typeof s.id === "string" && !s.parent_session_id && s.promptable !== false)
    .map((s: any) => ({
      id: s.id,
      subagentCount: descendants(s.id),
      title: typeof s.title === "string" && s.title ? s.title : "Untitled conversation",
      updatedAt: typeof s.updated_at === "string" ? s.updated_at : "",
      messageCount: typeof s.message_count === "number" ? s.message_count : 0,
    }));
}

/** `GET /v1/models` body -> the picker's list; malformed entries are dropped. */
function parseModelList(body: any): ModelInfo[] {
  const models = Array.isArray(body?.models) ? body.models : [];
  return models
    .filter((m: any) => m && typeof m.name === "string" && typeof m.gateway === "string" && typeof m.upstream_id === "string" && typeof m.provider_group === "string")
    .map((m: any) => ({
      name: m.name,
      gateway: m.gateway, upstream_id: m.upstream_id, provider_group: m.provider_group, api: m.api, enabled: m.enabled, disabled_reason: m.disabled_reason,
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
import { outcomeText } from "./outcome";
