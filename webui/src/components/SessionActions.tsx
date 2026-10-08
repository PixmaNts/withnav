import { ArrowElbowDownRight, Trash } from "@phosphor-icons/react";
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { api } from "../api/client";
import { HttpError } from "../api/http";
import { sessionsChanged, useSessions } from "../api/queries";
import type { SessionListEntry } from "../api/types";
import { sessionTitle } from "../lib/compose";
import { plural } from "../lib/format";
import type { Route } from "../router";
import { ConfirmDialog } from "./ConfirmDialog";
import { Crew } from "./Crew";

export interface DeleteSessionRequest {
  workspace: string;
  id: string;
  /** The title as the row or the open page showed it, used until the list answers. */
  title: string | null;
}

/** The sessions the sidebar lists as rows of their own: roots a driver can prompt. */
export const rootSessions = (rows: SessionListEntry[]) => rows.filter((s) => !s.parent_session_id && s.promptable !== false);

/** The subagent sessions attached to `id`; the server deletes them with it. */
export const subagentsOf = (rows: SessionListEntry[], id: string) => rows.filter((s) => s.parent_session_id === id);

/** Where to go once `deleted` leaves the list: the next root session, else the previous one, else none. */
export function nextSession(rows: SessionListEntry[], deleted: string): string | null {
  const roots = rootSessions(rows);
  const index = roots.findIndex((s) => s.id === deleted);
  const rest = roots.filter((s) => s.id !== deleted);
  if (!rest.length) return null;
  return rest[Math.max(0, Math.min(index, rest.length - 1))]!.id;
}

/** A sentence of the confirmation, also used by the tests. */
export function deleteQuestion(subagents: number): string {
  return subagents
    ? `Delete this session and its ${plural(subagents, "subagent session")}? This cannot be undone.`
    : "Delete this session? This cannot be undone.";
}

const childState = (k: SessionListEntry) => (k.live && (!k.child_status || k.child_status === "running") ? "running · will be stopped" : k.child_status ?? "stopped");

/** Confirmation of a session deletion; it calls the server, refreshes the lists and leaves a deleted open session. */
export function DeleteSessionDialog({
  request,
  route,
  navigate,
  onClose,
}: {
  request: DeleteSessionRequest;
  route: Route;
  navigate: (next: Partial<Route>, replace?: boolean) => void;
  onClose: () => void;
}) {
  const qc = useQueryClient();
  // The list the confirmation counts from is the same one the sidebar shows, refreshed as it opens.
  const sessions = useSessions(request.workspace);
  const rows = sessions.data ?? [];
  const row = rows.find((s) => s.id === request.id);
  const kids = subagentsOf(rows, request.id);
  const title = sessionTitle(row?.title ?? request.title);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const run = async () => {
    setBusy(true);
    setError(null);
    const gone = new Set([request.id, ...kids.map((k) => k.id)]);
    try {
      await api.remove(request.id);
    } catch (e) {
      // Already gone (another tab, the TUI): the outcome the driver asked for, so only the lists refresh.
      if (!(e instanceof HttpError && e.status === 404)) {
        setError(e instanceof HttpError && e.status !== 0 ? `The server refused: ${e.message}` : e instanceof Error ? e.message : String(e));
        setBusy(false);
        void sessionsChanged(qc, []);
        return;
      }
    }
    // The open session, or the parent of the open subagent view, is gone: show the next one.
    if (route.session && gone.has(route.session)) {
      const next = nextSession(rows, request.id);
      navigate({ workspace: request.workspace, session: next, subagent: null }, true);
    }
    onClose();
    await sessionsChanged(qc, [...gone]);
  };

  return (
    <ConfirmDialog
      title={`Delete session “${title}”`}
      icon={<Trash size={16} aria-hidden />}
      danger
      error={error}
      busy={busy}
      confirmLabel={busy ? "Deleting…" : "Delete session"}
      onConfirm={() => void run()}
      onDismiss={onClose}
    >
      <p className="confirm-lead">{deleteQuestion(kids.length)}</p>
      <p>
        {row?.live ? "It is still running, so it is stopped first. " : ""}
        The conversation and its events are deleted; files in the workspace are not touched.
      </p>
      {!!kids.length && (
        <div className="confirm-subs" aria-label="Subagent sessions deleted with it">
          <div className="confirm-subs-head">Subagents deleted with it</div>
          {kids.map((k) => (
            <div key={k.id} className="confirm-sub">
              <ArrowElbowDownRight size={13} aria-hidden />
              <Crew id={k.agent} />
              <span className="state" data-running={childState(k).startsWith("running") || undefined}>
                {childState(k)}
              </span>
            </div>
          ))}
        </div>
      )}
    </ConfirmDialog>
  );
}
