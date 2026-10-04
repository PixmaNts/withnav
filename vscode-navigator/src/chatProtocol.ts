// Contract between the extension host (which talks to the cognition over the
// local HTTP/SSE session server) and the chat webview (which only renders and
// never does networking). Both sides import these types.

/** View id of the chat panel, contributed in package.json. */
export const CHAT_VIEW_ID = "navai.chat";

/** Who wrote a message shown in the chat. */
export type Author = "driver" | "cognition" | "navigator" | "subagent";

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
  source: "jev" | "metacognition" | "driver";
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
  | { type: "status"; state: "connecting" | "ready" | "busy" | "error"; message?: string }
  /** A message already complete when shown (the driver's own, or a navigator wake).
   *  `context` is the chip label of the editor context attached to a driver message;
   *  `text` then holds the driver's own text only, without the raw block. */
  | { type: "message"; id: string; author: Author; text: string; context?: string; source?: string; selectedSkill?: { name: string; origin: string } }
  | { type: "permission"; card: PermissionCard }
  | { type: "permissionCall"; requestId: string; call: string }
  | { type: "permissionState"; requestId: string; resolution: string; message?: string }
  | { type: "pause"; id: string; kind: "budget" | "watchdog"; text: string; token?: PauseToken }
  | { type: "pauseState"; id: string; state: "pending" | "resolved" | "error"; message?: string }
  | { type: "notice"; text: string }
  | { type: "sendResult"; text: string; accepted: boolean }
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
  /** The cognition asked the driver a question and waits for the next message as the answer. */
  | { type: "question"; id: string; text: string }
  /** Live preview of the editor context the next message would carry (chip label). */
  | { type: "editorContext"; label: string }
  /** Past conversations of this workspace, newest first. `currentId` is the open one. */
  | { type: "conversations"; items: ConversationSummary[]; currentId?: string }
  /** A conversation was opened (resumed or started); a `history` message follows. */
  | { type: "conversationOpened"; id?: string; title: string }
  /** Replace the whole transcript (sent when the view is (re)opened). */
  | { type: "history"; items: HostToWebview[] }
  /** Models the cognition can use; `current` is the open conversation's (or the next new one's).
   *  `locked`: the conversation exists, its model can no longer change. */
  | { type: "models"; items: ModelInfo[]; current?: string; locked: boolean }
  /** Context of the last model call: `used` tokens out of `window` (absent when unknown). */
  | { type: "usage"; used: number; window?: number });

/** One saved conversation, as listed in the panel. `updatedAt` is an RFC 3339 timestamp. */
export interface ConversationSummary {
  id: string;
  title: string;
  updatedAt: string;
  messageCount: number;
}

/** One model alias from `GET /v1/models`. `contextWindow` is absent when models.json omits it. */
export interface ModelInfo {
  alias: string;
  model: string;
  contextWindow?: number;
}

/** Webview -> host messages. */
export type WebviewToHost = { generation?: number } & (
  /** The webview finished loading and wants the history. */
  | { type: "ready" }
  /** The driver sends a message to the cognition; the host appends the editor context when asked. */
  | { type: "send"; text: string; includeContext?: boolean }
  /** The driver interrupts the current turn. */
  | { type: "stop" }
  | { type: "answerPermission"; sessionId: string; requestId: string; decision: PermissionDecision; message?: string }
  | { type: "controlPause"; sessionId: string; id: string; action: "continue" | "stop" }
  /** The driver opens a file mentioned in the chat. */
  | { type: "openFile"; path: string; line?: number }
  /** The driver asks for the conversation list (the host answers with `conversations`). */
  | { type: "listConversations" }
  /** The driver reopens a past conversation. */
  | { type: "openConversation"; id: string }
  /** The driver starts a new conversation; the current one stays saved. */
  | { type: "newConversation" }
  /** The driver deletes a saved conversation. */
  | { type: "deleteConversation"; id: string }
  /** The driver picks the model for the next new conversation. */
  | { type: "selectModel"; alias: string });

/** What the chat view needs from the host side; implemented by the cognition bridge. */
export interface ChatBackend {
  currentGeneration(): number;
  resumePermissionReads(): void;
  cancelPermissionReads(): void;
  /** Current transcript, replayed into a freshly opened view. */
  history(): HostToWebview[];
  /** Subscribe to live messages; returns an unsubscribe function. */
  subscribe(listener: (message: HostToWebview) => void): () => void;
  /** `context` is an editor-context block appended after the driver's text. */
  send(text: string, context?: string): Promise<void>;
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
  /** Model alias for the next new conversation (ignored while one is open with messages). */
  selectModel(alias: string): void;
}
