import { Crew } from "./Crew";
import { CaretDown, CaretRight, Folder, FolderPlus, Plus, ArrowsInLineVertical, ArrowsOutLineVertical, Trash } from "@phosphor-icons/react";
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { api } from "../api/client";
import { workspacesChanged, useSessions, useWorkspaces } from "../api/queries";
import type { SessionListEntry, WorkspaceEntry } from "../api/types";
import { sessionTitle } from "../lib/compose";
import { dayLabel, plural, shortPath } from "../lib/format";
import type { Route } from "../router";
import type { useWorkspaceExpansion } from "../lib/workspaceExpansion";
import { WorkspaceMenu, type RemoveRequest } from "./WorkspaceActions";
import { rootSessions, subagentsOf, type DeleteSessionRequest } from "./SessionActions";

interface Props {
  route: Route;
  navigate: (next: Partial<Route>) => void;
  expansion: ReturnType<typeof useWorkspaceExpansion>;
  onRemove?: (request: RemoveRequest) => void;
  /** Asks to delete a root session; the caller confirms first. */
  onDelete?: (request: DeleteSessionRequest) => void;
}

/** The listed workspaces, plus a row for one opened by path that the server does not list yet. */
export function sidebarWorkspaces(listed: WorkspaceEntry[], current: string | null): WorkspaceEntry[] {
  if (!current || listed.some((w) => w.path === current)) return listed;
  return [...listed, { path: current, name: current.split("/").filter(Boolean).pop() ?? current, launched: false, session_count: 0, updated_at: null, branch: null, dirty: null }];
}

export function Sidebar({ route, navigate, expansion, onRemove, onDelete }: Props) {
  const qc = useQueryClient();
  const workspaces = useWorkspaces();
  const [adding, setAdding] = useState(false);
  const [path, setPath] = useState("");
  const [opening, setOpening] = useState(false);
  const [openError, setOpenError] = useState<string | null>(null);
  const all = sidebarWorkspaces(workspaces.data ?? [], route.workspace);

  const openFolder = async (value: string) => {
    setOpening(true);
    setOpenError(null);
    try {
      // A folder removed from the list earlier comes back when it is opened again.
      await api.unhideWorkspace(value);
    } catch (e) {
      setOpenError(`This folder cannot be opened: ${e instanceof Error ? e.message : String(e)}`);
      setOpening(false);
      return;
    }
    setOpening(false);
    setAdding(false);
    setPath("");
    navigate({ workspace: value, session: null, subagent: null });
    void workspacesChanged(qc);
  };

  return (
    <nav className="sidebar" aria-label="Workspaces">
      <div className="side-head">
        WORKSPACES
        <div className="side-actions">
          <button type="button" className="icon-btn" aria-label="Collapse all workspaces" title="Collapse all workspaces" onClick={expansion.collapseAll}><ArrowsInLineVertical size={14} /></button>
          <button type="button" className="icon-btn" aria-label="Expand all workspaces" title="Expand all workspaces" onClick={() => expansion.expandAll(all.map((w) => w.path))}><ArrowsOutLineVertical size={14} /></button>
        <button type="button" className="icon-btn" aria-label="Open a folder" title="Open a folder" onClick={() => (setAdding(!adding), setOpenError(null))}>
          <FolderPlus size={14} />
        </button>
        </div>
      </div>
      {adding && (
        <form
          className="open-folder"
          onSubmit={(e) => {
            e.preventDefault();
            const value = path.trim();
            if (!value.startsWith("/")) {
              setOpenError("Type an absolute path, starting with /.");
              return;
            }
            void openFolder(value.replace(/(.)\/+$/, "$1"));
          }}
        >
          <input autoFocus className="mono" placeholder="/absolute/path" aria-label="Absolute path of a folder" aria-invalid={openError ? true : undefined} value={path} onChange={(e) => (setPath(e.target.value), setOpenError(null))} onKeyDown={(e) => e.key === "Escape" && (e.preventDefault(), setAdding(false))} />
          <button type="submit" className="btn" disabled={opening}>
            Open
          </button>
        </form>
      )}
      {adding && openError && (
        <div className="side-error" role="alert">
          {openError}
        </div>
      )}
      {workspaces.isError && <div className="side-empty">The workspace list is not available from this server.</div>}
      {workspaces.isSuccess && !all.length && <div className="side-empty">No workspace yet. Open a folder with the button above.</div>}
      {all.map((w) => (
        <WorkspaceRow key={w.path} ws={w} route={route} navigate={navigate} expanded={expansion.expanded.has(w.path)} setOpen={(open) => expansion.setOpen(w.path, open)} onRemove={onRemove} onDelete={onDelete} />
      ))}
    </nav>
  );
}

export function WorkspaceRow({ ws, route, navigate, expanded, setOpen, onRemove, onDelete }: { ws: WorkspaceEntry; route: Route; navigate: Props["navigate"]; expanded: boolean; setOpen: (open: boolean) => void; onRemove?: Props["onRemove"]; onDelete?: Props["onDelete"] }) {
  const current = route.workspace === ws.path;
  const sessions = useSessions(expanded ? ws.path : null);
  const rows = sessions.data ?? [];
  const roots = rootSessions(rows);
  const children = (id: string) => subagentsOf(rows, id);
  const count = sessions.data ? roots.length : ws.session_count;
  return (
    <div className="ws">
      <div
        className="ws-row"
        role="group"
        tabIndex={0}
        aria-label={ws.name}
        aria-expanded={expanded}
        onKeyDown={(e) => {
          if (e.key === "ArrowLeft" || e.key === "ArrowRight") {
            e.preventDefault();
            setOpen(e.key === "ArrowRight");
          }
        }}
        title={ws.path}
      >
        <button type="button" className="icon-btn ws-toggle" aria-label={`${expanded ? "Collapse" : "Expand"} ${ws.name}`} aria-expanded={expanded} onClick={() => setOpen(!expanded)}>
          {expanded ? <CaretDown className="caret" /> : <CaretRight className="caret" />}
        </button>
        <button type="button" className="ws-select" aria-current={current ? "page" : undefined} onClick={() => navigate({ workspace: ws.path, session: null, subagent: null })}>
        <Folder size={14} style={{ color: current ? "var(--ac)" : "var(--mu)" }} aria-hidden />
        <span className="name">{ws.name}</span>
        <span className="count">{count}</span>
        </button>
        {onRemove && <WorkspaceMenu ws={ws} count={count} onRemove={onRemove} />}
      </div>
      {expanded && (
        <>
          <div className="ws-path mono" title={[ws.path, ws.branch, ws.dirty ? `${ws.dirty} changed` : ""].filter(Boolean).join(" · ")}>
            <span className="p">{shortPath(ws.path)}</span>
            {ws.branch && <span className="b"> · {ws.branch}</span>}
            {!!ws.dirty && <span className="b" style={{ color: "var(--or)" }}> ±{ws.dirty}</span>}
          </div>
          <button
            type="button"
            className="new-session"
            aria-current={current && !route.session ? "page" : undefined}
            onClick={() => navigate({ workspace: ws.path, session: null, subagent: null })}
          >
            <Plus size={12} aria-hidden /> New session
          </button>
          {sessions.isError && <div className="side-note">Sessions could not be loaded.</div>}
          {roots.map((s) => (
            <SessionRow key={s.id} s={s} kids={children(s.id)} ws={ws.path} route={route} navigate={navigate} onDelete={onDelete} />
          ))}
        </>
      )}
    </div>
  );
}

function SessionRow({ s, kids, ws, route, navigate, onDelete }: { s: SessionListEntry; kids: SessionListEntry[]; ws: string; route: Route; navigate: Props["navigate"]; onDelete?: Props["onDelete"] }) {
  const selected = route.session === s.id && !route.subagent;
  const title = sessionTitle(s.title);
  const remove = onDelete && (() => onDelete({ workspace: ws, id: s.id, title: s.title }));
  return (
    <>
      <div className="sess-item" data-selected={selected || undefined}>
        <button
          type="button"
          className="sess"
          aria-current={selected ? "page" : undefined}
          aria-keyshortcuts={remove ? "Delete" : undefined}
          onClick={() => navigate({ workspace: ws, session: s.id, subagent: null })}
          onKeyDown={(e) => {
            if (remove && e.key === "Delete" && !e.altKey && !e.ctrlKey && !e.metaKey) {
              e.preventDefault();
              remove();
            }
          }}
          title={title}
        >
          <span className="title">{title}</span>
          {s.bypass && <span className="bypass-badge">⚠ bypass</span>}
          <span className="meta">
            {[dayLabel(s.updated_at), plural(s.message_count, "msg")].filter(Boolean).join(" · ")}
          </span>
        </button>
        {remove && (
          <button type="button" className="icon-btn sess-del" aria-label={`Delete session ${title}`} title="Delete session (Del)" onClick={remove}>
            <Trash size={14} aria-hidden />
          </button>
        )}
      </div>
      {kids.map((k) => (
        <button
          key={k.id}
          type="button"
          className="sess child"
          aria-current={route.session === s.id && route.subagent === k.id ? "page" : undefined}
          onClick={() => navigate({ workspace: ws, session: s.id, subagent: k.id })}
          title={k.title ?? undefined}
        >
          <span className="agent">└ <Crew id={k.agent} /></span>
          {k.bypass && <span className="bypass-badge">⚠ bypass</span>}
          <span className="meta">{k.child_status ?? (k.live ? "running" : "")}</span>
        </button>
      ))}
    </>
  );
}
