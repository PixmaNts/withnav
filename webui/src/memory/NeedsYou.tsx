import { Check, HandWaving, X } from "@phosphor-icons/react";
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { api } from "../api/client";
import { registerChanged, useNeedsYou, useProjects } from "../api/queries";
import type { MembershipChange, ProjectsBody } from "../api/types";
import { ago, describeError, exactTime, shortId } from "./model";

/** What a waiting change would do, in a sentence that names the projects and groups. */
export function changeSentence(change: MembershipChange, body: ProjectsBody | undefined): string {
  const project = body?.projects.find((p) => p.id === change.project_id)?.name ?? shortId(change.project_id);
  if (change.relation === "project_group") {
    const group = body?.groups.find((g) => g.id === change.group_id)?.name ?? shortId(change.group_id ?? "");
    return change.change === "remove" ? `Remove ${project} from group ${group}` : `Add ${project} to group ${group}`;
  }
  const session = shortId(change.linked_session_id ?? "");
  return change.change === "remove" ? `Detach session ${session} from ${project}` : `Attach session ${session} to ${project}`;
}

export const NEEDS_YOU_CAP = 5;

/** The few changes that ask the Captain. Never a backlog: the register returns at most five. */
export function NeedsYou() {
  const qc = useQueryClient();
  const needs = useNeedsYou();
  const projects = useProjects();
  const [busy, setBusy] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const changes = needs.data ?? [];

  const decide = async (id: number, decision: "accept" | "reject") => {
    setBusy(id);
    setError(null);
    try {
      await api.decideChange(id, decision);
      await registerChanged(qc);
    } catch (e) {
      setError(describeError(e));
    } finally {
      setBusy(null);
    }
  };

  if (needs.isError)
    return (
      <div className="mm-alert" role="alert">
        <b>The waiting changes could not be read.</b> {describeError(needs.error)}{" "}
        <button type="button" className="btn" onClick={() => void needs.refetch()}>
          Try again
        </button>
      </div>
    );
  if (needs.isPending) return <div className="muted mm-pad">Loading…</div>;
  if (!changes.length)
    return (
      <div className="mm-empty">
        <HandWaving size={28} aria-hidden />
        <div className="mm-empty-title">Nothing needs you.</div>
        <div>When Nav proposes to share a project with a group, or to move a session, it will wait here for your answer.</div>
      </div>
    );
  return (
    <div className="mm-needs">
      {error && (
        <div className="mm-alert" role="alert">
          {error}
        </div>
      )}
      <p className="mm-hint">Changes to who shares what. Nothing happens until you answer.</p>
      {changes.map((c) => (
        <div key={c.id} className="mm-need">
          <div className="mm-need-main">
            <div className="mm-title">{changeSentence(c, projects.data)}</div>
            <div className="mm-hint">
              proposed by {c.actor === "agent" ? "Nav" : c.actor}
              {c.session_id && <> in session <span className="mono">{shortId(c.session_id)}</span></>}
              {" · "}
              <span title={exactTime(c.at)}>{ago(c.at)}</span>
            </div>
          </div>
          <button type="button" className="btn" disabled={busy === c.id} onClick={() => void decide(c.id, "accept")}>
            <Check size={12} aria-hidden /> Accept
          </button>
          <button type="button" className="btn danger" disabled={busy === c.id} onClick={() => void decide(c.id, "reject")}>
            <X size={12} aria-hidden /> Reject
          </button>
        </div>
      ))}
      {changes.length >= NEEDS_YOU_CAP && <p className="mm-hint">Showing up to {NEEDS_YOU_CAP} changes; more appear once you answer these.</p>}
    </div>
  );
}
