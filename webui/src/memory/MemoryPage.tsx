import { HandWaving, MagnifyingGlass, Notebook, Plugs } from "@phosphor-icons/react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { api } from "../api/client";
import { useMemoryItems, useNeedsYou, useProjects } from "../api/queries";
import type { MemoryFilter, MemoryItem, MemoryKind, MemoryScope, MemoryStatus } from "../api/types";
import { plural } from "../lib/format";
import { ranked } from "../lib/search";
import type { Route } from "../router";
import { ItemDetail } from "./ItemDetail";
import { ago, AUTHORITY_HINT, authority, describeError, exactTime, KINDS, scopeLabel, SCOPES, searchText, STATUSES, unreachable } from "./model";
import { NeedsYou } from "./NeedsYou";
import { PageShell } from "./PageShell";
import { UndoToast, type Toast } from "./UndoToast";

interface Props {
  route: Route;
  navigate: (next: Partial<Route>, replace?: boolean) => void;
}

function Chips<T extends string>({ label, values, value, onPick }: { label: string; values: readonly T[]; value: T | null; onPick: (next: T | null) => void }) {
  return (
    <div className="mm-chips" role="group" aria-label={label}>
      <span className="mm-chips-label" aria-hidden>
        {label.toUpperCase()}
      </span>
      {values.map((v) => (
        <button key={v} type="button" aria-pressed={value === v} onClick={() => onPick(value === v ? null : v)}>
          {v}
        </button>
      ))}
    </div>
  );
}

export function MemoryPage({ route, navigate }: Props) {
  const [scope, setScope] = useState<MemoryScope | null>(null);
  const [kind, setKind] = useState<MemoryKind | null>(null);
  // Active by default: a list of what Nav may read. Another status is a deliberate look at the past.
  const [status, setStatus] = useState<MemoryStatus | null>("active");
  const [text, setText] = useState("");
  const [toast, setToast] = useState<Toast | null>(null);
  const filter = useMemo<MemoryFilter>(() => ({ ...(status ? { status } : {}), ...(scope ? { scope } : {}), ...(kind ? { kind } : {}) }), [scope, kind, status]);
  const list = useMemoryItems(filter);
  const needs = useNeedsYou();
  const register = useProjects();
  const waiting = needs.data?.length ?? 0;
  const needsView = route.view === "needs";
  const groups = register.data?.groups ?? [];
  const projects = register.data?.projects ?? [];

  useEffect(() => {
    document.title = "Memory · WithNav";
  }, []);

  const shown = useMemo(() => ranked<MemoryItem>(list.data ?? [], text, (i) => i.current.title, searchText), [list.data, text]);
  const filtered = !!(scope || kind || text.trim() || status !== "active");
  const dismiss = useCallback(() => setToast(null), []);
  const say = useCallback((t: Omit<Toast, "id">) => setToast({ ...t, id: Date.now() }), []);
  const select = (item: string | null) => navigate({ item }, false);
  const clear = () => {
    setScope(null);
    setKind(null);
    setStatus("active");
    setText("");
  };
  const openSession = useCallback(
    (id: string) => {
      api
        .session(id)
        .then((s) => navigate({ page: "chat", workspace: s.workspace, session: id, subagent: null, item: null, view: null }))
        .catch((e) => say({ message: `The session cannot be opened: ${describeError(e)}`, tone: "error" }));
    },
    [navigate, say],
  );

  return (
    <PageShell route={route} navigate={navigate} title="Memory">
      <div className="mm-page">
        <header className="mm-head">
          <h1>Memory</h1>
          <span className="mm-hint">{list.data ? plural(shown.length, "item") : ""}</span>
          <div className="mm-head-right">
            {needsView ? (
              <button type="button" className="btn ghost" onClick={() => navigate({ view: null, item: null }, false)}>
                Back to memory
              </button>
            ) : (
              <button type="button" className="mm-needs-btn" data-waiting={waiting > 0 || undefined} onClick={() => navigate({ view: "needs", item: null }, false)}>
                <HandWaving size={14} aria-hidden /> Needs you
                {waiting > 0 && <span className="mm-badge">{waiting}</span>}
              </button>
            )}
          </div>
        </header>

        {needsView ? (
          <div className="mm-scroll">
            <NeedsYou />
          </div>
        ) : (
          <>
            <div className="mm-filters">
              <label className="mm-search">
                <MagnifyingGlass size={13} aria-hidden />
                <input type="search" aria-label="Search memory" placeholder="Search title, wording and quote" value={text} onChange={(e) => setText(e.target.value)} />
              </label>
              <Chips label="Scope" values={SCOPES} value={scope} onPick={setScope} />
              <Chips label="Kind" values={KINDS} value={kind} onPick={setKind} />
              <Chips label="Status" values={STATUSES} value={status} onPick={setStatus} />
            </div>
            <div className="mm-split" data-open={route.item ? true : undefined}>
              <div className="mm-list" aria-label="Memory items">
                {list.isError ? (
                  <div className="mm-alert" role="alert">
                    <Plugs size={18} aria-hidden />
                    <div>
                      <b>{unreachable(list.error) ? "The WithNav server is unreachable." : describeError(list.error)}</b>
                      <div className="mm-hint">Nothing is lost: the register is stored on the server. Check that it is running, then try again.</div>
                      <button type="button" className="btn" onClick={() => void list.refetch()}>
                        Try again
                      </button>
                    </div>
                  </div>
                ) : list.isPending ? (
                  <div className="muted mm-pad">Loading…</div>
                ) : !list.data.length && !filtered ? (
                  <div className="mm-empty">
                    <Notebook size={28} aria-hidden />
                    <div className="mm-empty-title">Nothing is remembered yet.</div>
                    <div>
                      Write <span className="mono">/remember &lt;text&gt;</span> in a session to keep a decision, a convention or a pitfall for later. Items you write are active at once.
                    </div>
                  </div>
                ) : !shown.length ? (
                  <div className="mm-empty">
                    <div>No item matches these filters.</div>
                    <button type="button" className="btn" onClick={clear}>
                      Clear filters
                    </button>
                  </div>
                ) : (
                  shown.map((i) => <Row key={i.id} item={i} selected={route.item === i.id} onOpen={() => select(i.id)} label={scopeLabel(i.scope, i.scope_ref, groups, projects)} />)
                )}
              </div>
              <aside className="mm-detail" aria-label="Item">
                {route.item ? (
                  <ItemDetail key={route.item} id={route.item} onToast={say} onClose={() => select(null)} onOpenSession={openSession} />
                ) : (
                  <div className="mm-empty muted">Pick an item to read its wording, its quote and every version.</div>
                )}
              </aside>
            </div>
          </>
        )}
        {toast && <UndoToast toast={toast} onDismiss={dismiss} />}
      </div>
    </PageShell>
  );
}

function Row({ item, selected, onOpen, label }: { item: MemoryItem; selected: boolean; onOpen: () => void; label: string }) {
  const auth = authority(item.current);
  return (
    <div
      className="mm-row"
      role="button"
      tabIndex={0}
      aria-pressed={selected}
      data-status={item.status}
      onClick={onOpen}
      onKeyDown={(e) => (e.key === "Enter" || e.key === " ") && (e.preventDefault(), onOpen())}
    >
      <div className="mm-title">{item.current.title}</div>
      <div className="mm-meta">
        <span className="mm-tag" data-scope={item.scope}>{label}</span>
        <span className="mm-tag" data-auth={auth} title={AUTHORITY_HINT[auth]}>{auth}</span>
        {item.status !== "active" && <span className="mm-tag" data-status={item.status}>{item.status}</span>}
        <span className="mm-hint">{item.kind}</span>
        <span className="mono mm-hint">v{item.current_version}</span>
        <span className="mm-hint" title={exactTime(item.updated_at)}>{ago(item.updated_at)}</span>
      </div>
    </div>
  );
}
