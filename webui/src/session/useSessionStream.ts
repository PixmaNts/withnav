import { useCallback, useEffect, useRef, useState } from "react";
import { followSession, type StreamSignal } from "../api/sse";
import type { Envelope } from "../api/types";
import { emptyView, reduceAll, type SessionView } from "./reducer";

export type StreamStatus = "idle" | "connecting" | "live" | "reconnecting" | "ended" | "error";

export interface SessionStream {
  view: SessionView;
  status: StreamStatus;
  error: string | null;
  /** Opens the stream again from the last applied seq (after a resume). */
  reconnect: () => void;
}

/**
 * Follows one session's events into a {@link SessionView}. Envelopes are
 * batched per animation frame: a fast stream of `llm.delta` would otherwise
 * render once per token. `once` is for a conversation known to be stopped:
 * its replay ends and is not retried.
 */
export function useSessionStream(
  sessionId: string | null,
  options: { once?: boolean; ready?: boolean; onEnvelope?: (envelope: Envelope) => void } = {},
): SessionStream {
  const [view, setView] = useState<SessionView>(emptyView);
  const [status, setStatus] = useState<StreamStatus>("idle");
  const [error, setError] = useState<string | null>(null);
  const [generation, setGeneration] = useState(0);
  const viewRef = useRef(view);
  viewRef.current = view;
  const pending = useRef<Envelope[]>([]);
  const frame = useRef<number | null>(null);
  const onEnvelope = useRef(options.onEnvelope);
  onEnvelope.current = options.onEnvelope;
  const sessionRef = useRef(sessionId);

  // A different session starts from nothing.
  if (sessionRef.current !== sessionId) {
    sessionRef.current = sessionId;
    pending.current = [];
  }
  useEffect(() => {
    setView(emptyView());
    viewRef.current = emptyView();
    setStatus(sessionId ? "connecting" : "idle");
    setError(null);
  }, [sessionId]);

  const flush = useCallback(() => {
    frame.current = null;
    const batch = pending.current;
    pending.current = [];
    if (batch.length) setView((current) => reduceAll(current, batch));
  }, []);

  const ready = options.ready ?? true;
  const once = options.once ?? false;
  useEffect(() => {
    if (!sessionId || !ready) return;
    const abort = new AbortController();
    const cursor = viewRef.current.lastSeq;
    setStatus("connecting");
    const onSignal = (signal: StreamSignal) => {
      if (abort.signal.aborted) return;
      switch (signal.type) {
        case "envelope":
          pending.current.push(signal.envelope);
          onEnvelope.current?.(signal.envelope);
          if (frame.current === null) frame.current = requestAnimationFrame(flush);
          break;
        case "connected":
          setStatus("live");
          setError(null);
          break;
        case "reconnecting":
          setStatus("reconnecting");
          break;
        case "resync":
          // The server no longer holds our cursor: drop the projection, the
          // replay that follows rebuilds it.
          pending.current = [];
          setView(emptyView());
          viewRef.current = emptyView();
          break;
        case "ended":
          flush();
          setStatus(signal.reason === "fatal" ? "error" : "ended");
          if (signal.message) setError(signal.message);
          break;
      }
    };
    void followSession(sessionId, cursor, onSignal, abort.signal, { once });
    return () => {
      abort.abort();
      if (frame.current !== null) cancelAnimationFrame(frame.current);
      frame.current = null;
      flush();
    };
  }, [sessionId, ready, once, generation, flush]);

  const reconnect = useCallback(() => setGeneration((g) => g + 1), []);
  return { view, status, error, reconnect };
}
