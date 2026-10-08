// Contract between the extension host (which talks to the cognition over the
// local HTTP/SSE session server) and the chat webview (which only renders and
// never does networking). Both sides import these types.

/** View id of the chat panel, contributed in package.json. */
export const CHAT_VIEW_ID = "withnav.chat";

/** Who wrote a message shown in the chat. */
export type Author = "captain" | "nav" | "driver" | "cognition" | "navigator" | "subagent";

export type PermissionDecision = "allow_once" | "allow_folder" | "deny";
export interface PermissionCard {
  requestId: string;
  callId: string;
  tool: string;
  agent: string;
  childSessionId?: string;
  scope: { kind: string; value: string };
  reason: string;
  call: string;
  arguments: string;
  allowFolder: boolean;
  evidence?: PermissionEvidence;
}
export interface PermissionEvidence {
  policy_version: number;
  source: "jev" | "metacognition" | "captain" | "driver";
  reason?: string;
  verdicts?: unknown;
  overrides?: unknown;
  models?: unknown;
  latency_ms?: number;
  truncated?: boolean;
}
export interface PauseToken { incarnation: string; hold_id: string }

/** Host -> webview messages. */
export type HostToWebview = { generation?: number } & (
  | { type: "bypass"; enabled: boolean; pending: boolean; disabled: boolean }
  | { type: "viewMode"; readOnly: boolean; archiveReason?: string }
  | { type: "assignments"; agents: AgentInfo[]; layers: LayerInfo[]; current: string; model?: string; variant?: string | null; source?: string; variant_source?: string; models?: ModelInfo[]; locked?: boolean }
  | { type: "compaction"; operationId: string; pending: boolean; text: string; ok?: boolean }
  | { type: "subagents"; parentId: string; items: ChildInfo[] }
  | { type: "childView"; child?: ChildInfo; parentId?: string; parentTitle?: string }
  | { type: "status"; state: "connecting" | "ready" | "busy" | "error"; message?: string }
  /** A message already complete when shown (the Captain's own, or a Nav wake).
   *  `context` is the chip label of the editor context attached to a Captain message;
   *  `text` then holds the Captain's own text only, without the raw block. */
  | { type: "message"; id: string; author: Author; text: string; context?: string; source?: string; selectedSkill?: { name: string; origin: string } }
  | { type: "permission"; card: PermissionCard }
  | { type: "permissionCall"; requestId: string; call: string }
  | { type: "permissionState"; requestId: string; resolution: string; message?: string }
  | { type: "pause"; id: string; kind: "budget" | "watchdog"; text: string; token?: PauseToken }
  | { type: "pauseState"; id: string; state: "pending" | "resolved" | "error"; message?: string }
  | { type: "notice"; text: string; operationId?: string; ok?: boolean; name?: string }
  | { type: "commands"; items: CommandInfo[] }
  | { type: "commandError"; text: string }
  | { type: "commandPicker"; kind: "model" | "variant" | "skills" | "agent"; items: { name: string; description: string; gateway?: string; provider_group?: string }[]; disabledReason?: string }
  | { type: "agent"; name: string }
  | { type: "modelPending"; name?: string }
  | { type: "sendResult"; text: string; accepted: boolean; restoreDraft?: boolean }
  /** The cognition starts answering; deltas for this id follow. */
  | { type: "turnStart"; id: string }
  /** Streamed answer text for the current turn, to append. */
  | { type: "delta"; id: string; text: string }
  /** Streamed reasoning text for the current turn, to append (shown collapsed). */
  | { type: "reasoning"; id: string; text: string }
  /** A tool call inside the current turn. `summary` is a short human line (e.g. "read src/main.rs"). */
  | { type: "tool"; id: string; callId: string; phase: "start" | "end"; name: string; summary: string; ok?: boolean; evidence?: PermissionEvidence }
  /** The turn is over. `error` is set when it failed. */
  | { type: "turnEnd"; id: string; outcome: "done" | "stopped" | "error"; error?: string }
  /** The cognition asked the Captain a question and waits for the next message as the answer. */
  | { type: "question"; id: string; text: string }
  /** Live preview of the editor context the next message would carry (chip label). */
  | { type: "editorContext"; label: string }
  /** Past conversations of this workspace, newest first. `currentId` is the open one. */
  | { type: "conversations"; items: ConversationSummary[]; currentId?: string }
  /** A conversation was opened (resumed or started); a `history` message follows. */
  | { type: "conversationOpened"; id?: string; title: string }
  /** Replace the whole transcript (sent when the view is (re)opened). */
  | { type: "history"; items: HostToWebview[] }
  /** Chat models; current is effective selection. Locked only while opening/creating. */
  | { type: "models"; items: ModelInfo[]; current?: string; locked: boolean }
  /** The Captain's favorite model names (kept by the host, shared by every panel; names no longer in the catalogue are not listed by the picker). */
  | { type: "favorites"; items: string[] }
  /** Context of the last model call: `used` tokens out of `window` (absent when unknown). */
  | { type: "usage"; snapshot: import("./contextMeter").ContextSnapshot });

/** One saved conversation, as listed in the panel. `updatedAt` is an RFC 3339 timestamp. */
export interface ConversationSummary {
  id: string;
  title: string;
  updatedAt: string;
  messageCount: number;
}

/** One canonical catalog identity; never reconstruct its name from upstream_id. */
export interface ModelInfo {
  name: string;
  gateway: string;
  provider_group: string;
  upstream_id: string;
  api?: string;
  enabled?: boolean;
  disabled_reason?: string;
  contextWindow?: number;
  variants?: string[];
  defaultVariant?: string;
}
export interface Selection { model: string; variant?: string | null; source: string; variant_source?: string }
export interface AgentInfo { id: string; description: string; mode: string; effective: Selection | null; assignment?: Selection | null; warning?: string | null }
export interface LayerInfo { layer: string; status: string; model?: string | null; variant?: string | null; api?: string; reason?: string | null }

export interface CommandInfo { name: string; description: string; args_hint: string }
export interface ChildInfo {
  id: string; agent: string; goal: string;
  state: "unknown" | "running" | "permission" | "budget" | "watchdog" | "done" | "failed" | "stopped";
  revision?: number; reason?: string; delivery?: string;
  pauseToken?: PauseToken;
  pending?: boolean;
  error?: string;
}
export interface ComposerOperation { owner: object; generation: number }

/** Webview -> host messages. */
export type WebviewToHost = { generation?: number } & (
  | { type: "toggleBypass" }
  | { type: "selectAgent"; id: string }
  | { type: "openChild"; parentId: string; childId: string }
  | { type: "controlChild"; parentId: string; childId: string; action: "stop" | "continue" }
  /** The webview finished loading and wants the history. */
  | { type: "ready" }
  /** The Captain sends a message to the cognition; the host appends the editor context when asked. */
  | { type: "send"; text: string; includeContext?: boolean }
  /** The Captain interrupts the current turn. */
  | { type: "stop" }
  | { type: "answerPermission"; sessionId: string; requestId: string; decision: PermissionDecision; message?: string }
  | { type: "controlPause"; sessionId: string; id: string; action: "continue" | "stop" }
  /** The Captain opens a file mentioned in the chat. */
  | { type: "openFile"; path: string; line?: number }
  /** The Captain asks for the conversation list (the host answers with `conversations`). */
  | { type: "listConversations" }
  /** The Captain reopens a past conversation. */
  | { type: "openConversation"; id: string }
  /** The Captain starts a new conversation; the current one stays saved. */
  | { type: "newConversation" }
  /** The Captain deletes a saved conversation. */
  | { type: "deleteConversation"; id: string }
  /** The Captain picks the model for the next new conversation. */
  | { type: "selectModel"; name: string }
  /** The Captain adds the model to the favorites, or removes it. The host answers with `favorites`. */
  | { type: "toggleFavorite"; name: string });

/** What the chat view needs from the host side; implemented by the cognition bridge. */
export interface ChatBackend {
  toggleBypassPermissions(): Promise<void>;
  publishBypass(): void;
  refreshAssignments(): Promise<void>;
  selectAgent(id: string): Promise<void>;
  publishCompaction(): void;
  dispose(): void;
  captureChildControl(parentId: string, childId: string, action: "stop" | "continue", generation?: number): object;
  publishSubagents(): void;
  openChild(parentId: string, childId: string, generation?: number): Promise<ChatBackend>;
  controlChild(parentId: string, childId: string, action: "stop" | "continue", generation?: number, descriptor?: object): Promise<void>;
  isReadOnly(): boolean;
  captureOperation(): ComposerOperation;
  ownsOperation(operation: ComposerOperation): boolean;
  currentContext(): import("./contextMeter").ContextSnapshot | undefined;
  currentGeneration(): number;
  refreshCommands(): Promise<void>;
  runComposerCommand(text: string, generation?: number, operation?: ComposerOperation): Promise<boolean>;
  resumePermissionReads(): void;
  cancelPermissionReads(): void;
  /** Current transcript, replayed into a freshly opened view. */
  history(): HostToWebview[];
  /** Subscribe to live messages; returns an unsubscribe function. */
  subscribe(listener: (message: HostToWebview) => void): () => void;
  /** `context` is an editor-context block appended after the Captain's text. */
  send(text: string, context?: string, operation?: ComposerOperation): Promise<void>;
  stop(): Promise<void>;
  hasOpenPermissions(): boolean;
  answerPermission(sessionId: string, requestId: string, decision: PermissionDecision, message?: string, generation?: number): Promise<void>;
  controlPause(sessionId: string, id: string, action: "continue" | "stop", generation?: number): Promise<void>;
  /** Saved conversations of the workspace, newest first. */
  listConversations(): Promise<ConversationSummary[]>;
  /** Resumes a saved conversation and replays it into the transcript. */
  openConversation(id: string): Promise<void>;
  /** Starts a new conversation on the next send. */
  newConversation(): Promise<void>;
  deleteConversation(id: string): Promise<void>;
  /** The open conversation's id, if one exists yet. */
  currentConversationId(): string | undefined;
  /** Model name for the next new conversation (ignored while one is open with messages). */
  selectModel(name: string): void;
}
