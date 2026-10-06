import { CaretDown, CaretRight, Folder, FolderPlus, Plus } from "@phosphor-icons/react";
import { useState } from "react";
import { useSessions, useWorkspaces } from "../api/queries";
import type { SessionListEntry, WorkspaceEntry } from "../api/types";
import { dayLabel, plural, shortPath } from "../lib/format";
import type { Route } from "../router";

interface Props {
  route: Route;
  navigate: (next: Partial<Route>) => void;
}

export function Sidebar({ route, navigate }: Props) {
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
        <button type="button" className="icon-btn" aria-label="Open a folder" title="Open a folder" onClick={() => setAdding(!adding)}>
          <FolderPlus size={14} />
        </button>
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
        <WorkspaceRow key={w.path} ws={w} route={route} navigate={navigate} />
      ))}
    </nav>
  );
}

function WorkspaceRow({ ws, route, navigate }: { ws: WorkspaceEntry; route: Route; navigate: Props["navigate"] }) {
  const current = route.workspace === ws.path;
  const [open, setOpen] = useState(current);
  const expanded = open || current;
  const sessions = useSessions(expanded ? ws.path : null);
  const rows = sessions.data ?? [];
  const roots = rows.filter((s) => !s.parent_session_id && s.promptable !== false);
  const children = (id: string) => rows.filter((s) => s.parent_session_id === id);
  const count = sessions.data ? roots.length : ws.session_count;
  return (
    <div className="ws">
      <button
        type="button"
        className="ws-row"
        aria-expanded={expanded}
        onClick={() => {
          if (current) setOpen(!expanded);
          else {
            setOpen(true);
            navigate({ workspace: ws.path, session: null, subagent: null });
          }
        }}
        title={ws.path}
      >
        {expanded ? <CaretDown className="caret" /> : <CaretRight className="caret" />}
        <Folder size={14} style={{ color: current ? "var(--ac)" : "var(--mu)" }} aria-hidden />
        <span className="name">{ws.name}</span>
        <span className="count">{count}</span>
      </button>
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
        title={s.title ?? undefined}
      >
        <span className="title">{s.title || "Untitled session"}</span>
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
