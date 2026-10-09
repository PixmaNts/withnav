import { DotsThree, EyeSlash, Trash } from "@phosphor-icons/react";
import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useId, useRef, useState } from "react";
import { api } from "../api/client";
import { HttpError } from "../api/http";
import { keys, workspacesChanged } from "../api/queries";
import type { WorkspaceEntry } from "../api/types";
import { plural } from "../lib/format";
import { ConfirmDialog } from "./ConfirmDialog";
import type { Route } from "../router";

export type RemoveKind = "hide" | "delete";
export interface RemoveRequest {
  ws: WorkspaceEntry;
  kind: RemoveKind;
  /** Root sessions of the folder, as the sidebar last counted them. */
  count: number;
}

/** Why a workspace cannot be removed, or undefined when it can. */
export function launchedReason(ws: WorkspaceEntry): string | undefined {
  return ws.launched ? "nav web was started with this folder, so it stays in the list while the server runs." : undefined;
}

/** Where to go once `removed` leaves the list: the next one, else the previous one, else nowhere. */
export function nextWorkspace(list: WorkspaceEntry[], removed: string): string | null {
  const index = list.findIndex((w) => w.path === removed);
  const rest = list.filter((w) => w.path !== removed);
  if (!rest.length) return null;
  return (rest[Math.max(0, Math.min(index, rest.length - 1))] ?? rest[0]!).path;
}

/** The `…` button of a workspace row and its menu. */
export function WorkspaceMenu({ ws, count, onRemove }: { ws: WorkspaceEntry; count: number; onRemove: (request: RemoveRequest) => void }) {
  const [open, setOpen] = useState(false);
  const trigger = useRef<HTMLButtonElement>(null);
  const menu = useRef<HTMLDivElement>(null);
  const id = useId();
  const blocked = launchedReason(ws);
  const items: { kind: RemoveKind; label: string; disabled?: string }[] = [
    { kind: "hide", label: "Remove from list", disabled: blocked },
    { kind: "delete", label: "Delete workspace sessions…", disabled: blocked ?? (count ? undefined : "This folder has no session to delete.") },
  ];

  const focusItem = (index: number) => {
    const all = menu.current?.querySelectorAll<HTMLButtonElement>('[role="menuitem"]');
    if (!all?.length) return;
    all[(index + all.length) % all.length]!.focus();
  };
  const close = (returnFocus: boolean) => {
    setOpen(false);
    if (returnFocus) trigger.current?.focus();
  };

  useEffect(() => {
    if (!open) return;
    focusItem(0);
    const onDown = (e: MouseEvent) => {
      if (!menu.current?.contains(e.target as Node) && !trigger.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [open]);

  const choose = (item: (typeof items)[number]) => {
    if (item.disabled) return;
    close(false);
    onRemove({ ws, kind: item.kind, count });
  };

  return (
    <div className="ws-more-wrap">
      <button
        ref={trigger}
        type="button"
        className="icon-btn ws-more"
        aria-label={`Actions for ${ws.name}`}
        title="Workspace actions"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={open ? id : undefined}
        data-open={open || undefined}
        onClick={() => setOpen(!open)}
        onKeyDown={(e) => {
          if (e.key === "ArrowDown" || e.key === "ArrowUp") {
            e.preventDefault();
            e.stopPropagation();
            setOpen(true);
          }
        }}
      >
        <DotsThree size={15} weight="bold" />
      </button>
      {open && (
        <div
          ref={menu}
          id={id}
          className="ws-menu"
          role="menu"
          aria-label={`Actions for ${ws.name}`}
          onKeyDown={(e) => {
            // The row toggles on left/right arrows; keys pressed in the menu stay in it.
            e.stopPropagation();
            const all = [...(menu.current?.querySelectorAll<HTMLButtonElement>('[role="menuitem"]') ?? [])];
            const at = all.indexOf(document.activeElement as HTMLButtonElement);
            if (e.key === "ArrowDown") (e.preventDefault(), focusItem(at + 1));
            else if (e.key === "ArrowUp") (e.preventDefault(), focusItem(at - 1));
            else if (e.key === "Home") (e.preventDefault(), focusItem(0));
            else if (e.key === "End") (e.preventDefault(), focusItem(-1));
            else if (e.key === "Escape") (e.preventDefault(), close(true));
            else if (e.key === "Tab") close(false);
          }}
        >
          {items.map((item) => (
            <button
              key={item.kind}
              type="button"
              role="menuitem"
              tabIndex={-1}
              className={`ws-menu-item${item.kind === "delete" ? " danger" : ""}`}
              aria-disabled={item.disabled ? true : undefined}
              aria-describedby={item.disabled && blocked ? `${id}-why` : undefined}
              title={item.disabled}
              onClick={() => choose(item)}
            >
              {item.kind === "hide" ? <EyeSlash size={14} aria-hidden /> : <Trash size={14} aria-hidden />}
              {item.label}
            </button>
          ))}
          {blocked && (
            <div className="ws-menu-note" id={`${id}-why`}>
              {blocked}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

/** Words for an error the server gave back; the server's own message when there is no better one. */
function removalError(error: unknown): string {
  if (error instanceof HttpError) {
    if (error.reason === "launched") return "nav web was started with this folder, so it cannot leave the list while the server runs.";
    if (error.status === 0) return error.message;
    return `The server refused: ${error.message}`;
  }
  return error instanceof Error ? error.message : String(error);
}

/** Confirmation for both removals; it calls the server, refreshes the lists and leaves a removed open workspace. */
export function RemoveWorkspaceDialog({
  request,
  workspaces,
  route,
  navigate,
  onClose,
}: {
  request: RemoveRequest;
  workspaces: WorkspaceEntry[];
  route: Route;
  navigate: (next: Partial<Route>) => void;
  onClose: () => void;
}) {
  const qc = useQueryClient();
  const { kind } = request;
  // The server counts root sessions the way it deletes them; its latest answer wins over the row's.
  const ws = workspaces.find((w) => w.path === request.ws.path) ?? request.ws;
  const count = workspaces.some((w) => w.path === ws.path) ? ws.session_count : request.count;
  useEffect(() => {
    void qc.invalidateQueries({ queryKey: keys.workspaces });
  }, [qc]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const destructive = kind === "delete";

  const run = async () => {
    setBusy(true);
    setError(null);
    const leaving = route.workspace === ws.path;
    const next = nextWorkspace(workspaces, ws.path);
    try {
      if (destructive) await api.deleteWorkspaceSessions(ws.path);
      else await api.hideWorkspace(ws.path);
    } catch (e) {
      setError(removalError(e));
      setBusy(false);
      // A deletion that failed half way has still removed some sessions.
      if (destructive) void workspacesChanged(qc);
      return;
    }
    if (leaving) navigate({ workspace: next, session: null, subagent: null });
    onClose();
    await workspacesChanged(qc);
  };

  const sessions = plural(count, "session");
  return (
    <ConfirmDialog
      title={destructive ? `Delete the sessions of ${ws.name}` : `Remove ${ws.name} from the list`}
      icon={destructive ? <Trash size={16} aria-hidden /> : <EyeSlash size={16} aria-hidden />}
      danger={destructive}
      error={error}
      busy={busy}
      confirmLabel={busy ? (destructive ? "Deleting…" : "Removing…") : destructive ? `Delete ${sessions}` : "Remove from list"}
      onConfirm={() => void run()}
      onDismiss={onClose}
    >
      {destructive ? (
        <>
          <p className="confirm-lead">Delete {sessions} and their subagents? This cannot be undone.</p>
          <p>A session still running is stopped first. The folder and its files are not touched, and {ws.name} leaves the list.</p>
        </>
      ) : (
        <>
          <p className="confirm-lead">Remove {ws.name} from the list?</p>
          <p>{count === 1 ? "Its session is kept." : count ? `Its ${sessions} are kept.` : "Nothing is deleted."} Open the folder again to bring it back.</p>
        </>
      )}
      <p className="confirm-path mono" title={ws.path}>
        {ws.path}
      </p>
    </ConfirmDialog>
  );
}
