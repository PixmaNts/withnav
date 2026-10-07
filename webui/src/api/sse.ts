// Server-sent events over `fetch`.
//
// EventSource cannot send an `Authorization` header, so the stream is read by
// hand: one `fetch` per connection, its body decoded frame by frame. The SSE
// `id:` of every frame is the envelope `seq`; a reconnect sends the highest seq
// delivered as `Last-Event-ID` so the server replays only what was missed. A
// `resync` control frame means the cursor was rejected: the consumer drops its
// projection and the next connection replays everything.

import type { Envelope } from "./types";
import { authHeaders, reportUnauthorized } from "./http";

export interface Frame {
  event: string;
  data: string;
  id?: string;
}

/**
 * Incremental decoder for `text/event-stream`. Feed it text chunks as they
 * arrive; it returns the frames completed by each chunk. Comments (`:`
 * keep-alives) and unknown fields are ignored, `\r\n` and `\r` line ends are
 * accepted, multi-line `data:` joins with `\n` (WHATWG SSE parsing rules).
 */
export class SseDecoder {
  private buffer = "";
  private event = "";
  private data: string[] = [];
  private id: string | undefined;

  push(chunk: string): Frame[] {
    this.buffer += chunk;
    const frames: Frame[] = [];
    for (;;) {
      const match = /\r\n|\r|\n/.exec(this.buffer);
      if (!match) break;
      // A lone `\r` at the very end may be the first half of `\r\n`.
      if (match[0] === "\r" && match.index === this.buffer.length - 1) break;
      const line = this.buffer.slice(0, match.index);
      this.buffer = this.buffer.slice(match.index + match[0].length);
      if (line === "") {
        if (this.data.length) frames.push({ event: this.event || "message", data: this.data.join("\n"), id: this.id });
        this.event = "";
        this.data = [];
        continue;
      }
      if (line.startsWith(":")) continue;
      const colon = line.indexOf(":");
      const field = colon === -1 ? line : line.slice(0, colon);
      let value = colon === -1 ? "" : line.slice(colon + 1);
      if (value.startsWith(" ")) value = value.slice(1);
      if (field === "data") this.data.push(value);
      else if (field === "event") this.event = value;
      else if (field === "id") this.id = value;
    }
    return frames;
  }
}

export type StreamSignal =
  | { type: "envelope"; envelope: Envelope }
  | { type: "connected" }
  | { type: "reconnecting"; attempt: number }
  | { type: "resync" }
  | { type: "ended"; reason: "session_end" | "closed" | "fatal"; message?: string };

const BACKOFF_START_MS = 250;
const BACKOFF_MAX_MS = 5000;

export function nextBackoff(current: number): number {
  return Math.min(current * 2, BACKOFF_MAX_MS);
}

/**
 * Follows `/v1/sessions/{id}/events` until `session.end`, a fatal answer, or
 * `abort`. `cursor` is the last seq already applied (null = full history).
 * A connection lost to the network is retried with backoff; one the server
 * closes cleanly is retried once, then reported `closed`. With `once` (a
 * conversation known to be stopped) the first clean close ends it.
 */
export async function followSession(
  sessionId: string,
  cursor: number | null,
  onSignal: (signal: StreamSignal) => void,
  abort: AbortSignal,
  options: { once?: boolean } = {},
): Promise<void> {
  let backoff = BACKOFF_START_MS;
  let attempt = 0;
  let last = cursor;
  while (!abort.aborted) {
    let delivered = false;
    let resynced = false;
    try {
      const headers = authHeaders({ accept: "text/event-stream" });
      if (last !== null) headers.set("last-event-id", String(last));
      const response = await fetch(`/v1/sessions/${encodeURIComponent(sessionId)}/events`, { headers, signal: abort });
      if (response.status === 401) {
        reportUnauthorized();
        onSignal({ type: "ended", reason: "fatal", message: "Session expired" });
        return;
      }
      if (!response.ok || !response.body) {
        const fatal = response.status >= 400 && response.status < 500 && response.status !== 408;
        if (fatal) {
          const text = await response.text().catch(() => "");
          onSignal({ type: "ended", reason: "fatal", message: errorText(text) || `HTTP ${response.status}` });
          return;
        }
        throw new Error(`HTTP ${response.status}`);
      }
      onSignal({ type: "connected" });
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      const sse = new SseDecoder();
      for (;;) {
        const part = await reader.read();
        if (part.done) break;
        for (const frame of sse.push(decoder.decode(part.value, { stream: true }))) {
          delivered = true;
          if (frame.event === "resync") {
            last = null;
            resynced = true;
            onSignal({ type: "resync" });
            continue;
          }
          if (frame.event !== "message") continue;
          let envelope: Envelope;
          try {
            envelope = JSON.parse(frame.data) as Envelope;
          } catch {
            continue;
          }
          if (typeof envelope.seq !== "number" || typeof envelope.kind !== "string") continue;
          last = last === null ? envelope.seq : Math.max(last, envelope.seq);
          onSignal({ type: "envelope", envelope });
          if (envelope.kind === "session.end") {
            void reader.cancel().catch(() => undefined);
            onSignal({ type: "ended", reason: "session_end" });
            return;
          }
        }
      }
      // The server closed the stream on its own. A stored (stopped)
      // conversation replays and closes, and a reconnect to it delivers
      // nothing: it is not live, and only a resume opens it again.
      if ((options.once || !delivered) && !resynced) {
        onSignal({ type: "ended", reason: "closed" });
        return;
      }
      // The cursor was rejected: replay everything now, no backoff.
      if (resynced) continue;
    } catch (error) {
      if (abort.aborted) return;
      void error;
    }
    if (delivered) backoff = BACKOFF_START_MS;
    attempt += 1;
    onSignal({ type: "reconnecting", attempt });
    await sleep(backoff, abort);
    backoff = nextBackoff(backoff);
  }
}

function errorText(body: string): string {
  try {
    const value = JSON.parse(body);
    return typeof value?.error === "string" ? value.error : "";
  } catch {
    return body.trim().slice(0, 300);
  }
}

function sleep(ms: number, abort: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, ms);
    abort.addEventListener("abort", () => {
      clearTimeout(timer);
      resolve();
    }, { once: true });
  });
}
