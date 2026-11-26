import { Bell, BellSlash } from "@phosphor-icons/react";
import { createContext, useContext, useEffect, useMemo, useRef, useState } from "react";
import type { WaitingEntry } from "../api/types";
import { sessionTitle } from "../lib/compose";
import { createNotifier, readMuted, setMuted, type NotificationLike } from "../lib/notify";
import type { Route } from "../router";

/** Every permission request the live sessions wait on, read once at the top of the app. */
export const WaitingContext = createContext<WaitingEntry[]>([]);

/** The root sessions that wait for the Captain. */
export function useWaitingSessions(): ReadonlySet<string> {
  const entries = useContext(WaitingContext);
  return useMemo(() => new Set(entries.map((entry) => entry.session_id)), [entries]);
}

/** The dot the sidebar shows beside a session that waits for the Captain. */
export function WaitingDot({ sessionId }: { sessionId: string }) {
  if (!useWaitingSessions().has(sessionId)) return null;
  return <span className="waiting-dot" role="img" aria-label="Waiting for you" title="Waiting for you" />;
}

const MUTE_EVENT = "withnav:notify-muted";

/** The sessions the Captain muted, shared by every component that asks, and across tabs. */
export function useMutedSessions(): [ReadonlySet<string>, (sessionId: string, muted: boolean) => void] {
  const [muted, setState] = useState(() => readMuted(localStorage));
  useEffect(() => {
    const refresh = () => setState(readMuted(localStorage));
    window.addEventListener(MUTE_EVENT, refresh);
    window.addEventListener("storage", refresh);
    return () => {
      window.removeEventListener(MUTE_EVENT, refresh);
      window.removeEventListener("storage", refresh);
    };
  }, []);
  const toggle = (sessionId: string, value: boolean) => {
    setMuted(localStorage, sessionId, value);
    window.dispatchEvent(new Event(MUTE_EVENT));
  };
  return [muted, toggle];
}

const browserNotifications = () => typeof Notification !== "undefined";

/** What the browser lets this page do: `unsupported` when there is no Notification API. */
function notificationPermission(): NotificationPermission | "unsupported" {
  return browserNotifications() ? Notification.permission : "unsupported";
}

const silent: NotificationLike = { close() {}, onclick: null };

/**
 * One browser notification per blocked turn, closed when the request is answered anywhere. A page
 * the Captain is looking at, on the waiting session, raises nothing; it does when he looks away.
 */
export function useWaitingNotifications(entries: WaitingEntry[], route: Pick<Route, "session">, open: (entry: WaitingEntry) => void) {
  const [muted] = useMutedSessions();
  const [away, setAway] = useState(0);
  const latest = useRef({ muted, route, open });
  latest.current = { muted, route, open };
  const notifier = useMemo(
    () =>
      createNotifier({
        create: (title, options) => (browserNotifications() && Notification.permission === "granted" ? new Notification(title, options) : silent),
        isMuted: (id) => latest.current.muted.has(id),
        quiet: (entry) => document.visibilityState === "visible" && document.hasFocus() && entry.session_id === latest.current.route.session,
        onOpen: (entry) => {
          window.focus();
          latest.current.open(entry);
        },
      }),
    [],
  );
  useEffect(() => {
    const bump = () => setAway((n) => n + 1);
    document.addEventListener("visibilitychange", bump);
    window.addEventListener("blur", bump);
    window.addEventListener("focus", bump);
    return () => {
      document.removeEventListener("visibilitychange", bump);
      window.removeEventListener("blur", bump);
      window.removeEventListener("focus", bump);
    };
  }, []);
  useEffect(() => notifier.sync(entries), [notifier, entries, muted, away, route.session]);
  useEffect(() => () => notifier.closeAll(), [notifier]);
}

/** The top bar's count of waiting sessions, and the list behind it with a mute switch per session. */
export function WaitingBadge({ open }: { open: (entry: WaitingEntry) => void }) {
  const entries = useContext(WaitingContext);
  const [muted, toggle] = useMutedSessions();
  const [shown, setShown] = useState(false);
  const [permission, setPermission] = useState(notificationPermission);
  if (entries.length === 0) return null;
  const label = `${entries.length} waiting for you`;
  return (
    <span className="waiting-badge">
      <button type="button" className="bypass-badge" aria-expanded={shown} aria-label={label} title={label} onClick={() => setShown((v) => !v)}>
        <Bell size={12} aria-hidden /> {entries.length} waiting
      </button>
      {shown && (
        <div className="waiting-list" role="group" aria-label="Sessions waiting for you">
          {entries.map((entry) => (
            <div key={`${entry.session_id}:${entry.turn_id}`} className="waiting-row">
              <button type="button" className="waiting-open" onClick={() => { setShown(false); open(entry); }}>
                <span className="title">{sessionTitle(entry.title)}</span>
                <span className="muted">{entry.summary}</span>
              </button>
              <button
                type="button"
                className="icon-btn"
                aria-pressed={muted.has(entry.session_id)}
                aria-label={muted.has(entry.session_id) ? "Notify me again for this session" : "Mute notifications for this session"}
                title={muted.has(entry.session_id) ? "Muted: click to notify again" : "Mute notifications for this session"}
                onClick={() => toggle(entry.session_id, !muted.has(entry.session_id))}
              >
                {muted.has(entry.session_id) ? <BellSlash size={14} aria-hidden /> : <Bell size={14} aria-hidden />}
              </button>
            </div>
          ))}
          {permission === "default" && (
            <button type="button" className="btn ghost" onClick={() => void Notification.requestPermission().then(setPermission)}>
              Notify me when I am away
            </button>
          )}
        </div>
      )}
    </span>
  );
}
