import { CaretDown, CaretRight, Folder, FolderPlus, Plus, ArrowsInLineVertical, ArrowsOutLineVertical } from "@phosphor-icons/react";
import { useState } from "react";
import { useSessions, useWorkspaces } from "../api/queries";
import type { SessionListEntry, WorkspaceEntry } from "../api/types";
import { sessionTitle } from "../lib/compose";
import { dayLabel, plural, shortPath } from "../lib/format";
import type { Route } from "../router";
import type { useWorkspaceExpansion } from "../lib/workspaceExpansion";

interface Props {
  route: Route;
  navigate: (next: Partial<Route>) => void;
  expansion: ReturnType<typeof useWorkspaceExpansion>;
}

export function Sidebar({ route, navigate, expansion }: Props) {
  const workspaces = useWorkspaces();
  const [adding, setAdding] = useState(false);
  const [path, setPath] = useState("");
  const listed = workspaces.data ?? [];
  // A workspace opened by path (not yet known to the server) still gets a row.
  const all: WorkspaceEntry[] =
    route.workspace && !listed.some((w) => w.path === route.workspace)
      ? [...listed, { path: route.workspace, name: route.workspace.split("/").filter(Boolean).pop() ?? route.workspace, launched: false, session_count: 0, updated_at: null, branch: null, dirty: null }]
      : listed;

  return (
    <nav className="sidebar" aria-label="Workspaces">
      <div className="side-head">
        WORKSPACES
        <div className="side-actions">
          <button type="button" className="icon-btn" aria-label="Collapse all workspaces" title="Collapse all workspaces" onClick={expansion.collapseAll}><ArrowsInLineVertical size={14} /></button>
          <button type="button" className="icon-btn" aria-label="Expand all workspaces" title="Expand all workspaces" onClick={() => expansion.expandAll(all.map((w) => w.path))}><ArrowsOutLineVertical size={14} /></button>
        <button type="button" className="icon-btn" aria-label="Open a folder" title="Open a folder" onClick={() => setAdding(!adding)}>
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
            if (!value.startsWith("/")) return;
            setAdding(false);
            setPath("");
            navigate({ workspace: value.replace(/\/+$/, ""), session: null, subagent: null });
          }}
        >
          <input autoFocus className="mono" placeholder="/absolute/path" aria-label="Absolute path of a folder" value={path} onChange={(e) => setPath(e.target.value)} onKeyDown={(e) => e.key === "Escape" && setAdding(false)} />
          <button type="submit" className="btn">
            Open
          </button>
        </form>
      )}
      {workspaces.isError && <div className="side-empty">The workspace list is not available from this server.</div>}
      {workspaces.isSuccess && !all.length && <div className="side-empty">No workspace yet. Open a folder with the button above.</div>}
      {all.map((w) => (
        <WorkspaceRow key={w.path} ws={w} route={route} navigate={navigate} expanded={expansion.expanded.has(w.path)} setOpen={(open) => expansion.setOpen(w.path, open)} />
      ))}
    </nav>
  );
}

export function WorkspaceRow({ ws, route, navigate, expanded, setOpen }: { ws: WorkspaceEntry; route: Route; navigate: Props["navigate"]; expanded: boolean; setOpen: (open: boolean) => void }) {
  const current = route.workspace === ws.path;
  const sessions = useSessions(expanded ? ws.path : null);
  const rows = sessions.data ?? [];
  const roots = rows.filter((s) => !s.parent_session_id && s.promptable !== false);
  const children = (id: string) => rows.filter((s) => s.parent_session_id === id);
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
            <SessionRow key={s.id} s={s} kids={children(s.id)} ws={ws.path} route={route} navigate={navigate} />
          ))}
        </>
      )}
    </div>
  );
}

function SessionRow({ s, kids, ws, route, navigate }: { s: SessionListEntry; kids: SessionListEntry[]; ws: string; route: Route; navigate: Props["navigate"] }) {
  const selected = route.session === s.id && !route.subagent;
  return (
    <>
      <button
        type="button"
        className="sess"
        aria-current={selected ? "page" : undefined}
        onClick={() => navigate({ workspace: ws, session: s.id, subagent: null })}
        title={sessionTitle(s.title)}
      >
        <span className="title">{sessionTitle(s.title)}</span>
        <span className="meta">
          {[dayLabel(s.updated_at), plural(s.message_count, "msg")].filter(Boolean).join(" · ")}
        </span>
      </button>
      {kids.map((k) => (
        <button
          key={k.id}
          type="button"
          className="sess child"
          aria-current={route.session === s.id && route.subagent === k.id ? "page" : undefined}
          onClick={() => navigate({ workspace: ws, session: s.id, subagent: k.id })}
          title={k.title ?? undefined}
        >
          <span className="agent">└ {k.agent}</span>
          <span className="meta">{k.child_status ?? (k.live ? "running" : "")}</span>
        </button>
      ))}
    </>
  );
}
