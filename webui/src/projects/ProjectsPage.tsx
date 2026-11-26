import { ClockCounterClockwise, GitFork, Plugs, X } from "@phosphor-icons/react";
import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useMemo, useState } from "react";
import { api } from "../api/client";
import { registerChanged, useProjects, useSessions } from "../api/queries";
import type { MembershipChange, MemoryGroup, MemoryProject, ProjectsBody } from "../api/types";
import { plural } from "../lib/format";
import { ago, describeError, exactTime, shortId, unreachable, widening } from "../memory/model";
import { PageShell } from "../memory/PageShell";
import { WideningDialog } from "../memory/WideningDialog";
import type { Route } from "../router";

interface Props {
  route: Route;
  navigate: (next: Partial<Route>, replace?: boolean) => void;
}

/** "all", "none" (projects without a group) or a group id. */
type Selection = string;

export function ProjectsPage({ route, navigate }: Props) {
  const qc = useQueryClient();
  const register = useProjects();
  const sessions = useSessions(route.workspace);
  const [selection, setSelection] = useState<Selection>("all");
  const [error, setError] = useState<string | null>(null);
  const body = register.data;

  useEffect(() => {
    document.title = "Projects · WithNav";
  }, []);

  const titles = useMemo(() => new Map((sessions.data ?? []).map((s) => [s.id, s.title?.trim() || shortId(s.id)])), [sessions.data]);
  const projects = useMemo(() => {
    const all = body?.projects ?? [];
    return selection === "all" ? all : selection === "none" ? all.filter((p) => !p.groups.length) : all.filter((p) => p.groups.includes(selection));
  }, [body, selection]);
  const open = body?.projects.find((p) => p.id === route.item) ?? null;
  const group = body?.groups.find((g) => g.id === selection) ?? null;

  /** Runs a register write and reads everything again; a refusal is shown, never swallowed. */
  const write = async (work: () => Promise<unknown>): Promise<boolean> => {
    setError(null);
    try {
      await work();
      await registerChanged(qc);
      return true;
    } catch (e) {
      setError(describeError(e));
      return false;
    }
  };

  return (
    <PageShell route={route} navigate={navigate} title="Projects">
      <div className="mm-page">
        <header className="mm-head">
          <h1>Projects</h1>
          <span className="mm-hint">{body ? `${plural(body.projects.length, "project")} · ${plural(body.groups.length, "group")}` : ""}</span>
        </header>
        {register.isError ? (
          <div className="mm-alert" role="alert">
            <Plugs size={18} aria-hidden />
            <div>
              <b>{unreachable(register.error) ? "The WithNav server is unreachable." : describeError(register.error)}</b>
              <div className="mm-hint">Projects and groups are stored on the server. Check that it is running, then try again.</div>
              <button type="button" className="btn" onClick={() => void register.refetch()}>
                Try again
              </button>
            </div>
          </div>
        ) : !body ? (
          <div className="muted mm-pad">Loading…</div>
        ) : (
          <div className="pj-grid">
            <GroupsColumn body={body} selection={selection} setSelection={setSelection} group={group} write={write} />
            <section className="pj-list" aria-label="Projects">
              {!projects.length && (
                <div className="mm-empty">
                  <GitFork size={26} aria-hidden />
                  {body.projects.length ? <div>No project in this group.</div> : <div className="mm-empty-title">No project yet.</div>}
                  {!body.projects.length && <div>A project appears here when a session starts in a git repository. Worktrees of one repository share it.</div>}
                </div>
              )}
              {projects.map((p) => (
                <button key={p.id} type="button" className="pj-project" aria-current={p.id === route.item ? "true" : undefined} onClick={() => navigate({ item: p.id }, false)}>
                  <span className="mm-title">{p.name}</span>
                  <span className="mm-hint mono">{p.aliases[0] ?? shortId(p.id)}</span>
                  <span className="mm-hint">{p.groups.length ? p.groups.map((g) => body.groups.find((x) => x.id === g)?.name ?? shortId(g)).join(", ") : "no group"}</span>
                </button>
              ))}
            </section>
            <aside className="pj-detail" aria-label="Project">
              {open ? (
                <ProjectDetail key={open.id} project={open} body={body} titles={titles} workspaceSessions={(sessions.data ?? []).filter((s) => !s.parent_session_id)} write={write} error={error} dismissError={() => setError(null)} />
              ) : (
                <div className="mm-empty muted">Pick a project to see its groups and sessions.</div>
              )}
            </aside>
          </div>
        )}
      </div>
    </PageShell>
  );
}

function GroupsColumn({
  body,
  selection,
  setSelection,
  group,
  write,
}: {
  body: ProjectsBody;
  selection: Selection;
  setSelection: (s: Selection) => void;
  group: MemoryGroup | null;
  write: (work: () => Promise<unknown>) => Promise<boolean>;
}) {
  const [name, setName] = useState("");
  const [renaming, setRenaming] = useState<string | null>(null);
  const none = body.projects.filter((p) => !p.groups.length).length;
  const row = (id: Selection, label: string, count: number, extra?: string) => (
    <button key={id} type="button" className="pj-group" aria-current={selection === id ? "true" : undefined} onClick={() => (setSelection(id), setRenaming(null))}>
      <span className="mm-title">{label}</span>
      <span className="mm-hint">{count}</span>
      {extra && <span className="mm-hint pj-extra">{extra}</span>}
    </button>
  );

  return (
    <nav className="pj-groups" aria-label="Groups">
      <div className="mm-label">GROUPS</div>
      {row("all", "All projects", body.projects.length)}
      {body.groups.map((g) => row(g.id, g.name, g.members.length, plural(g.items, "item")))}
      {row("none", "No group", none)}
      {group &&
        (renaming === null ? (
          <button type="button" className="btn ghost" onClick={() => setRenaming(group.name)}>
            Rename
          </button>
        ) : (
          <form
            className="pj-inline"
            onSubmit={async (e) => {
              e.preventDefault();
              const next = renaming.trim();
              if (next && next !== group.name && (await write(() => api.renameGroup(group.id, next)))) setRenaming(null);
              else if (next === group.name) setRenaming(null);
            }}
          >
            <input aria-label="Group name" value={renaming} onChange={(e) => setRenaming(e.target.value)} />
            <button type="submit" className="btn">
              Save name
            </button>
            <button type="button" className="btn ghost" onClick={() => setRenaming(null)}>
              Cancel
            </button>
          </form>
        ))}
      <form
        className="pj-inline"
        onSubmit={async (e) => {
          e.preventDefault();
          const next = name.trim();
          if (next && (await write(() => api.createGroup(next)))) setName("");
        }}
      >
        <input aria-label="New group name" placeholder="New group" value={name} onChange={(e) => setName(e.target.value)} />
        <button type="submit" className="btn" disabled={!name.trim()}>
          Create group
        </button>
      </form>
    </nav>
  );
}

function ProjectDetail({
  project,
  body,
  titles,
  workspaceSessions,
  write,
  error,
  dismissError,
}: {
  project: MemoryProject;
  body: ProjectsBody;
  titles: Map<string, string>;
  workspaceSessions: { id: string; title: string | null }[];
  write: (work: () => Promise<unknown>) => Promise<boolean>;
  error: string | null;
  dismissError: () => void;
}) {
  const [toAdd, setToAdd] = useState("");
  const [toAttach, setToAttach] = useState("");
  const [pending, setPending] = useState<{ group: MemoryGroup; affected: MemoryProject[]; error: string | null; busy: boolean } | null>(null);
  const groupName = (id: string) => body.groups.find((g) => g.id === id)?.name ?? shortId(id);
  const sessionName = (id: string) => titles.get(id) ?? shortId(id);
  const active = project.sessions.filter((s) => s.active);
  const detached = project.sessions.filter((s) => !s.active);
  const candidates = workspaceSessions.filter((s) => !active.some((a) => a.session_id === s.id));
  const joinable = body.groups.filter((g) => !project.groups.includes(g.id));
  const history = body.history.filter((c) => c.project_id === project.id);

  const add = async () => {
    const group = body.groups.find((g) => g.id === toAdd);
    if (!group) return;
    dismissError();
    try {
      await api.addProjectToGroup(project.id, group.id, false);
      setToAdd("");
      await write(async () => undefined);
    } catch (e) {
      const affected = widening(e);
      if (affected) setPending({ group, affected, error: null, busy: false });
      else await write(() => Promise.reject(e));
    }
  };
  const confirm = async () => {
    if (!pending) return;
    setPending({ ...pending, busy: true, error: null });
    try {
      await api.addProjectToGroup(project.id, pending.group.id, true);
      setPending(null);
      setToAdd("");
      await write(async () => undefined);
    } catch (e) {
      setPending({ ...pending, busy: false, error: describeError(e) });
    }
  };

  return (
    <div className="pj-detail-body">
      <h2 className="mm-detail-title">{project.name}</h2>
      <div className="mm-hint mono">{project.id}</div>
      {error && (
        <div className="mm-alert" role="alert">
          {error}{" "}
          <button type="button" className="icon-btn" aria-label="Dismiss" onClick={dismissError}>
            <X size={12} aria-hidden />
          </button>
        </div>
      )}

      <div className="mm-label">PATHS</div>
      {project.aliases.length ? (
        <ul className="pj-paths mono">
          {project.aliases.map((a) => (
            <li key={a}>{a}</li>
          ))}
        </ul>
      ) : (
        <p className="mm-hint">No path is known for this project.</p>
      )}

      <div className="mm-label">GROUPS</div>
      <div className="pj-chips">
        {project.groups.map((g) => (
          <span key={g} className="mm-tag" data-scope="group">
            {groupName(g)}
            <button type="button" className="icon-btn" aria-label={`Remove ${project.name} from ${groupName(g)}`} title="Remove from the group" onClick={() => void write(() => api.removeProjectFromGroup(project.id, g))}>
              <X size={11} aria-hidden />
            </button>
          </span>
        ))}
        {!project.groups.length && <span className="mm-hint">No group{project.group_answered ? "" : " yet: the question is still open"}.</span>}
      </div>
      <div className="pj-inline">
        <select aria-label="Add to a group" value={toAdd} onChange={(e) => setToAdd(e.target.value)} disabled={!joinable.length}>
          <option value="">{joinable.length ? "Choose a group…" : "No other group"}</option>
          {joinable.map((g) => (
            <option key={g.id} value={g.id}>
              {g.name}
            </option>
          ))}
        </select>
        <button type="button" className="btn" disabled={!toAdd} onClick={() => void add()}>
          Add to group
        </button>
      </div>

      <div className="mm-label">SESSIONS</div>
      {!active.length && <p className="mm-hint">No session is attached.</p>}
      {active.map((s) => (
        <div key={s.session_id} className="pj-session" data-active>
          <span className="mm-title">{sessionName(s.session_id)}</span>
          <span className="mono mm-hint">{shortId(s.session_id)}</span>
          <button type="button" className="btn" onClick={() => void write(() => api.sessionProject(s.session_id, null))}>
            Detach
          </button>
        </div>
      ))}
      <p className="mm-hint">Detaching keeps the link in the history; it does not delete the session.</p>
      {detached.map((s) => (
        <div key={s.session_id} className="pj-session" data-detached>
          <span className="mm-title">{sessionName(s.session_id)}</span>
          <span className="mono mm-hint">{shortId(s.session_id)}</span>
          <span className="mm-hint">detached</span>
        </div>
      ))}
      <div className="pj-inline">
        <select aria-label="Attach a session" value={toAttach} onChange={(e) => setToAttach(e.target.value)} disabled={!candidates.length}>
          <option value="">{candidates.length ? "Choose a session…" : "No session of this workspace to attach"}</option>
          {candidates.map((s) => (
            <option key={s.id} value={s.id}>
              {s.title?.trim() || shortId(s.id)}
            </option>
          ))}
        </select>
        <button
          type="button"
          className="btn"
          disabled={!toAttach}
          onClick={async () => {
            if (await write(() => api.sessionProject(toAttach, project.id))) setToAttach("");
          }}
        >
          Attach
        </button>
      </div>

      <details className="pj-history">
        <summary>
          <ClockCounterClockwise size={13} aria-hidden /> History ({history.length})
        </summary>
        {history.length ? (
          <ul>
            {history.map((c) => (
              <li key={c.id}>
                {historySentence(c, project, groupName, sessionName)} <span className="mm-hint" title={exactTime(c.at)}>· {c.actor} · {ago(c.at)}</span>
              </li>
            ))}
          </ul>
        ) : (
          <p className="mm-hint">Nothing changed yet.</p>
        )}
      </details>

      {pending && (
        <WideningDialog
          title="Share the group's items?"
          lead={
            <>
              Adding <b>{project.name}</b> to group <b>{pending.group.name}</b> lets it read the {plural(pending.group.items, "item")} scoped to the group, and lets the group's other projects read the items scoped to {project.name}'s group.
            </>
          }
          projects={pending.affected}
          busy={pending.busy}
          error={pending.error}
          onConfirm={() => void confirm()}
          onDismiss={() => setPending(null)}
        />
      )}
    </div>
  );
}

function historySentence(c: MembershipChange, project: MemoryProject, group: (id: string) => string, session: (id: string) => string): string {
  const remove = c.change === "remove";
  const what =
    c.relation === "project_group"
      ? `${remove ? "Removed" : "Added"} ${project.name} ${remove ? "from" : "to"} group ${group(c.group_id ?? "")}`
      : `${remove ? "Detached" : "Attached"} session ${session(c.linked_session_id ?? "")}`;
  if (c.status === "proposed") return `Proposed: ${what.charAt(0).toLowerCase()}${what.slice(1)}`;
  if (c.status === "rejected") return `Rejected: ${what.charAt(0).toLowerCase()}${what.slice(1)}`;
  return what;
}
