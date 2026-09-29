// Contract between the extension host (which talks to the cognition over the
// local HTTP/SSE session server) and the chat webview (which only renders and
// never does networking). Both sides import these types.

/** View id of the chat panel, contributed in package.json. */
export const CHAT_VIEW_ID = "navigator.chat";

/** Who wrote a message shown in the chat. */
export type Author = "driver" | "cognition" | "navigator";

/** Host -> webview messages. */
export type HostToWebview =
  | { type: "status"; state: "connecting" | "ready" | "busy" | "error"; message?: string }
  /** A message already complete when shown (the driver's own, or a navigator wake). */
  | { type: "message"; id: string; author: Author; text: string }
  /** The cognition starts answering; deltas for this id follow. */
  | { type: "turnStart"; id: string }
  /** Streamed answer text for the current turn, to append. */
  | { type: "delta"; id: string; text: string }
  /** Streamed reasoning text for the current turn, to append (shown collapsed). */
  | { type: "reasoning"; id: string; text: string }
  /** A tool call inside the current turn. `summary` is a short human line (e.g. "read src/main.rs"). */
  | { type: "tool"; id: string; callId: string; phase: "start" | "end"; name: string; summary: string; ok?: boolean }
  /** The turn is over. `error` is set when it failed. */
  | { type: "turnEnd"; id: string; outcome: "done" | "stopped" | "error"; error?: string }
  /** The cognition asked the driver a question and waits for the next message as the answer. */
  | { type: "question"; id: string; text: string }
  /** Replace the whole transcript (sent when the view is (re)opened). */
  | { type: "history"; items: HostToWebview[] };

/** Webview -> host messages. */
export type WebviewToHost =
  /** The webview finished loading and wants the history. */
  | { type: "ready" }
  /** The driver sends a message to the cognition. */
  | { type: "send"; text: string }
  /** The driver interrupts the current turn. */
  | { type: "stop" }
  /** The driver opens a file mentioned in the chat. */
  | { type: "openFile"; path: string; line?: number };

/** What the chat view needs from the host side; implemented by the cognition bridge. */
export interface ChatBackend {
  /** Current transcript, replayed into a freshly opened view. */
  history(): HostToWebview[];
  /** Subscribe to live messages; returns an unsubscribe function. */
  subscribe(listener: (message: HostToWebview) => void): () => void;
  send(text: string): Promise<void>;
  stop(): Promise<void>;
}
