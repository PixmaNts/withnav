import { ArrowSquareOut, ArrowCounterClockwise, Copy, PencilSimple, ProhibitInset, Quotes, ShieldCheck, ShareNetwork, X } from "@phosphor-icons/react";
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { api } from "../api/client";
import { keys, registerChanged, useMemoryItem, useProjects } from "../api/queries";
import type { MemoryGroup, MemoryItem, MemoryItemDetail, MemoryProject, MemoryScope } from "../api/types";
import { ago, AUTHORITY_HINT, authority, authorLine, describeError, exactTime, scopeLabel, SCOPES, shortId, versionConflict, widening } from "./model";
import type { Toast } from "./UndoToast";
import { VersionTimeline } from "./VersionTimeline";
import { WideningDialog } from "./WideningDialog";

type Draft =
  | { kind: "edit"; title: string; body: string; why: string; reason: string; origin: { title: string; body: string; why: string } }
  | { kind: "invalidate"; reason: string }
  | { kind: "scope"; scope: MemoryScope; ref: string; reason: string };

interface Props {
  id: string;
  onToast: (toast: Omit<Toast, "id">) => void;
  onClose: () => void;
  onOpenSession: (sessionId: string) => void;
}

const UNDO_REASON = "Undone from the toast";

export function ItemDetail({ id, onToast, onClose, onOpenSession }: Props) {
  const qc = useQueryClient();
  const detail = useMemoryItem(id);
  const register = useProjects();
  const groups = register.data?.groups ?? [];
  const projects = register.data?.projects ?? [];

  const [draft, setDraft] = useState<Draft | null>(null);
  /** The version the draft is written against; a stale one is refused by the register. */
  const [base, setBase] = useState(0);
  const [conflict, setConflict] = useState<{ current: number } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [affected, setAffected] = useState<MemoryProject[] | null>(null);

  if (detail.isError)
    return (
      <div className="mm-alert" role="alert">
        <b>This item could not be read.</b> {describeError(detail.error)}{" "}
        <button type="button" className="btn" onClick={() => void detail.refetch()}>
          Try again
        </button>
      </div>
    );
  if (!detail.data) return <div className="muted mm-pad">Loading…</div>;
  const it = detail.data;
  const v = it.current;
  const auth = authority(v);
  const label = scopeLabel(it.scope, it.scope_ref, groups, projects);

  const open = (next: Draft) => {
    setDraft(next);
    setBase(it.current_version);
    setConflict(null);
    setError(null);
  };
  const close = () => {
    setDraft(null);
    setConflict(null);
    setError(null);
    setAffected(null);
  };
  const edit = () => open({ kind: "edit", title: v.title, body: v.body, why: v.why ?? "", reason: "", origin: { title: v.title, body: v.body, why: v.why ?? "" } });

  /** Runs a write and tells the Captain how to take it back; a refusal is kept apart from a failure. */
  const settled = async (done: (next: MemoryItem) => Toast["undo"] | undefined, message: (next: MemoryItem) => string, write: () => Promise<MemoryItem>) => {
    setBusy(true);
    setError(null);
    try {
      const next = await write();
      await registerChanged(qc);
      onToast({ message: message(next), undo: done(next) });
      close();
    } catch (e) {
      const stale = versionConflict(e);
      const wide = widening(e);
      if (stale) {
        setConflict(stale);
        void qc.invalidateQueries({ queryKey: keys.memoryItem(id) });
      } else if (wide) setAffected(wide);
      else setError(describeError(e));
    } finally {
      setBusy(false);
    }
  };

  const undoWith = (write: () => Promise<MemoryItem>) => async () => {
    try {
      await write();
      await registerChanged(qc);
      onToast({ message: "Back as it was" });
    } catch (e) {
      onToast({ message: `Could not undo: ${describeError(e)}`, tone: "error" });
    }
  };

  const submit = (confirm = false, against?: number) => {
    if (!draft) return;
    const expected = against ?? base;
    if (draft.kind === "edit") {
      const body: { expected_version: number; title?: string; text?: string; why?: string; reason?: string } = { expected_version: expected };
      if (draft.title !== draft.origin.title) body.title = draft.title;
      if (draft.body !== draft.origin.body) body.text = draft.body;
      if (draft.why !== draft.origin.why) body.why = draft.why;
      if (draft.reason.trim()) body.reason = draft.reason.trim();
      if (body.title === undefined && body.text === undefined && body.why === undefined) {
        setError("Nothing changed: edit the title, the decision or the reason it was made.");
        return;
      }
      void settled(
        (next) => undoWith(() => api.restoreMemory(id, { expected_version: next.current_version, from_version: expected, reason: UNDO_REASON })),
        (next) => `Saved as version ${next.current_version}`,
        () => api.reviseMemory(id, body),
      );
    } else if (draft.kind === "invalidate") {
      void settled(
        (next) => undoWith(() => api.restoreMemory(id, { expected_version: next.current_version, reason: UNDO_REASON })),
        () => "Invalidated; the history is kept",
        () => api.invalidateMemory(id, draft.reason.trim() ? { expected_version: expected, reason: draft.reason.trim() } : { expected_version: expected }),
      );
    } else {
      const was = { scope: it.scope, ref: it.scope_ref };
      const body: { expected_version: number; scope: string; scope_ref?: string; confirm?: boolean; reason?: string } = { expected_version: expected, scope: draft.scope };
      if (draft.scope !== "global") body.scope_ref = draft.ref;
      if (confirm) body.confirm = true;
      if (draft.reason.trim()) body.reason = draft.reason.trim();
      void settled(
        (next) => undoWith(() => api.rescopeMemory(id, { expected_version: next.current_version, scope: was.scope, ...(was.scope === "global" ? {} : { scope_ref: was.ref }), reason: UNDO_REASON })),
        () => `Moved to ${scopeLabel(draft.scope, draft.ref, groups, projects)}`,
        () => api.rescopeMemory(id, body),
      );
    }
  };

  const restore = (from?: number) =>
    void settled(
      (next) =>
        from === undefined
          ? undoWith(() => api.invalidateMemory(id, { expected_version: next.current_version, reason: UNDO_REASON }))
          : undoWith(() => api.restoreMemory(id, { expected_version: next.current_version, from_version: it.current_version, reason: UNDO_REASON })),
      (next) => (from === undefined ? `Restored as version ${next.current_version}` : `The wording of version ${from} is current again, as version ${next.current_version}`),
      () => api.restoreMemory(id, from === undefined ? { expected_version: it.current_version } : { expected_version: it.current_version, from_version: from }),
    );

  const newer = conflict && detail.data.current_version === conflict.current ? detail.data : null;
  const canWrite = it.status === "active" || it.status === "invalidated";

  return (
    <article className="mm-detail-body" aria-label={v.title}>
      <header className="mm-detail-head">
        <div className="mm-tags">
          <span className="mm-tag" data-scope={it.scope}>{label}</span>
          <span className="mm-tag" data-auth={auth} title={AUTHORITY_HINT[auth]}>{auth}</span>
          <span className="mm-tag" data-status={it.status}>{it.status}</span>
          <span className="mono mm-hint">v{it.current_version}</span>
        </div>
        <button type="button" className="icon-btn mm-close" aria-label="Close the item" title="Close" onClick={onClose}>
          <X size={16} aria-hidden />
        </button>
      </header>
      <h2 className="mm-detail-title">{v.title}</h2>
      <div className="mono mm-hint">
        {it.id} · {it.kind}
      </div>

      <div className="mm-label">DECISION</div>
      <p className="mm-text">{v.body}</p>
      <div className="mm-label">WHY</div>
      <p className="mm-text" data-empty={!v.why || undefined}>{v.why ?? "not stated"}</p>

      {v.quote && (
        <blockquote className="mm-quote">
          <div className="mm-label">
            <Quotes size={12} aria-hidden /> EXACT QUOTE
          </div>
          “{v.quote}”
        </blockquote>
      )}
      {v.source_session_id && (
        <button type="button" className="mm-link" onClick={() => onOpenSession(v.source_session_id!)}>
          <ArrowSquareOut size={12} aria-hidden /> Open session <span className="mono">{shortId(v.source_session_id)}</span>
        </button>
      )}
      <div className="mm-hint" title={exactTime(it.updated_at)}>
        Updated {ago(it.updated_at)} · created {ago(it.created_at)}
      </div>

      <p className="mm-note">
        <ShieldCheck size={13} aria-hidden /> Memory informs Nav. It never grants permission: an item worded as an instruction is still only a note.
      </p>

      <div className="mm-actions">
        {it.status === "active" && (
          <>
            <button type="button" className="btn" onClick={edit}>
              <PencilSimple size={12} aria-hidden /> Edit
            </button>
            <button type="button" className="btn" onClick={() => open({ kind: "invalidate", reason: "" })}>
              <ProhibitInset size={12} aria-hidden /> Invalidate
            </button>
            <button type="button" className="btn" onClick={() => open({ kind: "scope", scope: it.scope, ref: it.scope_ref, reason: "" })}>
              <ShareNetwork size={12} aria-hidden /> Change scope
            </button>
          </>
        )}
        {it.status === "invalidated" && (
          <button type="button" className="btn" disabled={busy} onClick={() => restore()}>
            <ArrowCounterClockwise size={12} aria-hidden /> Restore
          </button>
        )}
        <button type="button" className="btn ghost" onClick={() => void navigator.clipboard?.writeText(it.id)}>
          <Copy size={12} aria-hidden /> Copy id
        </button>
      </div>
      {!canWrite && <p className="mm-hint">This item is {it.status}; its wording cannot be changed from here.</p>}

      {draft && (
        <form
          className="mm-form"
          onSubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
          <div className="mm-form-title">{draft.kind === "edit" ? "Edit: saved as a new version" : draft.kind === "invalidate" ? "Invalidate this item" : "Change who reads this item"}</div>
          {draft.kind === "edit" && (
            <>
              <input aria-label="Title" value={draft.title} onChange={(e) => setDraft({ ...draft, title: e.target.value })} />
              <textarea aria-label="Decision" rows={4} value={draft.body} onChange={(e) => setDraft({ ...draft, body: e.target.value })} />
              <input aria-label="Why" placeholder="Why (optional)" value={draft.why} onChange={(e) => setDraft({ ...draft, why: e.target.value })} />
            </>
          )}
          {draft.kind === "scope" && <ScopeFields draft={draft} setDraft={setDraft} groups={groups} projects={projects} />}
          <input
            aria-label="Reason"
            placeholder={draft.kind === "invalidate" ? "Why is it no longer true? (kept in the history)" : "Reason for this change (kept in the history)"}
            value={draft.reason}
            onChange={(e) => setDraft({ ...draft, reason: e.target.value })}
          />
          {error && (
            <div className="mm-alert" role="alert">
              {error}
            </div>
          )}
          {conflict && (
            <div className="mm-conflict" role="alert">
              <b>The item moved on: version {conflict.current} was saved while you were editing.</b> Your draft is kept below.
              {newer ? (
                <ConflictVersion item={newer} />
              ) : (
                <div className="mm-hint">Loading the newer version…</div>
              )}
              <div className="mm-form-actions">
                <button
                  type="button"
                  className="btn"
                  disabled={busy}
                  onClick={() => {
                    // The draft was written against the old version; it is now written on top of the newer one.
                    setBase(conflict.current);
                    setConflict(null);
                    submit(false, conflict.current);
                  }}
                >
                  Apply my edit on top of version {conflict.current}
                </button>
                <button type="button" className="btn ghost" onClick={close}>
                  Discard my draft
                </button>
              </div>
            </div>
          )}
          {!conflict && (
            <div className="mm-form-actions">
              <button type="submit" className="btn" disabled={busy || (draft.kind === "scope" && !scopeReady(draft, it))}>
                {draft.kind === "edit" ? "Save as a new version" : draft.kind === "invalidate" ? "Invalidate item" : "Move item"}
              </button>
              <button type="button" className="btn ghost" onClick={close}>
                Cancel
              </button>
            </div>
          )}
        </form>
      )}

      <VersionTimeline
        versions={it.versions}
        currentVersion={it.current_version}
        groups={groups}
        projects={projects}
        onUse={it.status === "active" ? (n) => restore(n) : undefined}
      />

      {affected && draft?.kind === "scope" && (
        <WideningDialog
          title="Share this item with more projects?"
          lead={<>Moving it to {scopeLabel(draft.scope, draft.ref, groups, projects)} lets more projects read it. Nav reads it as a note, never as a permission.</>}
          projects={affected}
          busy={busy}
          error={error}
          onConfirm={() => submit(true)}
          onDismiss={() => setAffected(null)}
        />
      )}
    </article>
  );
}

function scopeReady(draft: Extract<Draft, { kind: "scope" }>, it: MemoryItemDetail): boolean {
  if (draft.scope === "global") return it.scope !== "global";
  return draft.ref.trim() !== "" && (draft.scope !== it.scope || draft.ref !== it.scope_ref);
}

/** The newer version that the register holds after a refused write. */
function ConflictVersion({ item }: { item: MemoryItemDetail }) {
  return (
    <div className="mm-conflict-newer">
      <div className="mm-hint">
        Version {item.current_version} by {authorLine(item.current)} · <span title={exactTime(item.current.at)}>{ago(item.current.at)}</span>
      </div>
      <div className="mm-title">{item.current.title}</div>
      <div>{item.current.body}</div>
      {item.current.why && <div className="mm-hint">Why: {item.current.why}</div>}
    </div>
  );
}

function ScopeFields({
  draft,
  setDraft,
  groups,
  projects,
}: {
  draft: Extract<Draft, { kind: "scope" }>;
  setDraft: (draft: Draft) => void;
  groups: MemoryGroup[];
  projects: MemoryProject[];
}) {
  return (
    <>
      <div className="mm-chips" role="group" aria-label="New scope">
        {SCOPES.map((s) => (
          <button key={s} type="button" aria-pressed={draft.scope === s} onClick={() => setDraft({ ...draft, scope: s, ref: s === draft.scope ? draft.ref : "" })}>
            {s}
          </button>
        ))}
      </div>
      {draft.scope === "group" && (
        <select aria-label="Group" value={draft.ref} onChange={(e) => setDraft({ ...draft, ref: e.target.value })}>
          <option value="">Choose a group…</option>
          {groups.map((g) => (
            <option key={g.id} value={g.id}>
              {g.name}
            </option>
          ))}
        </select>
      )}
      {draft.scope === "project" && (
        <select aria-label="Project" value={draft.ref} onChange={(e) => setDraft({ ...draft, ref: e.target.value })}>
          <option value="">Choose a project…</option>
          {projects.map((p) => (
            <option key={p.id} value={p.id}>
              {p.name}
            </option>
          ))}
        </select>
      )}
      {draft.scope === "session" && <input className="mono" aria-label="Session id" placeholder="Session id" value={draft.ref} onChange={(e) => setDraft({ ...draft, ref: e.target.value })} />}
    </>
  );
}
