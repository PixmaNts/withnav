import { Crew } from "./Crew";
import { ArrowLeft } from "@phosphor-icons/react";
import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useMemo, useState } from "react";
import { api } from "../api/client";
import { keys, useSessionSummary } from "../api/queries";
import { elapsed } from "../lib/format";
import type { Child, SessionView, ToolCall, Turn } from "../session/reducer";
import { useSessionStream } from "../session/useSessionStream";
import { Markdown } from "./Markdown";
import { ToolList } from "./ToolList";

const DELIVERY: Record<string, string> = { next_turn: "next-turn", steer: "steer", interrupt: "interrupt" };

/**
 * A subagent's own session, read-only: its task, how its result returns, the
 * calls it made and what it has written so far. Children are not promptable
 * (CONTEXT.md, Subagent), so there is no steer box: guidance comes from the
 * parent agent. The Captain can stop a child, or continue one that is paused.
 */
export function SubagentView({ childId, parentId, parent, parentTitle, onBack }: { childId: string; parentId: string; parent: SessionView; parentTitle: string; onBack: () => void }) {
  const qc = useQueryClient();
  const summary = useSessionSummary(childId);
  const stopped = summary.data?.actor_state === "stopped";
  const stream = useSessionStream(childId, { ready: !!summary.data, once: stopped, bypass: summary.data?.bypass, bypassSeq: summary.data?.bypass_seq });
  const child: Child | undefined = parent.children[childId];
  const spawnCall: ToolCall | undefined = child ? parent.tools[child.toolCallId] : undefined;
  const delivery = child?.delivery ?? (typeof spawnCall?.args?.delivery === "string" ? spawnCall.args.delivery : null);
  const view = stream.view;
  useEffect(() => {
    if (!summary.data || view.bypassSeq < 0) return;
    qc.setQueryData(keys.sessions(summary.data.workspace), (rows: import("../api/types").SessionListEntry[] | undefined) => rows?.map(row => row.id === childId && (row.bypass_seq ?? -1) <= view.bypassSeq ? { ...row, bypass: view.bypass, bypass_seq: view.bypassSeq } : row));
  }, [view.bypass, view.bypassSeq, childId, summary.data, qc]);
  const [now, setNow] = useState(Date.now());
  const [error, setError] = useState<string | null>(null);
  const [acting, setActing] = useState(false);

  const status = child?.status ?? (stopped ? "stopped" : summary.data ? "running" : "…");
  const running = status === "running" || status === "doubt";
  useEffect(() => {
    if (!running) return;
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [running]);
  const started = child?.spawnedAt ?? view.startedAt;
  const ended = child?.endedAt ?? view.endedAt;
  const time = started ? elapsed((ended ? Date.parse(ended) : now) - Date.parse(started)) : null;

  const calls = useMemo(() => Object.values(view.tools), [view.tools]);
  const toolNames = [...new Set(calls.map((c) => c.name))];
  const turns = view.items.filter((i): i is Turn => i.kind === "turn");
  const findings = turns
    .flatMap((t) => t.parts)
    .filter((p): p is Extract<Turn["parts"][number], { type: "text" }> => p.type === "text" && !!p.text.trim())
    .map((p) => p.text);
  const latest = findings[findings.length - 1];
  const goal = child?.goal || (view.items.find((i) => i.kind === "captain" || i.kind === "subagent-result") as { text?: string } | undefined)?.text || "";
  const paused = view.pause;

  const act = async (work: () => Promise<unknown>) => {
    setActing(true);
    setError(null);
    try {
      await work();
      await qc.invalidateQueries({ queryKey: keys.session(childId) });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
    setActing(false);
  };

  return (
    <div className="subview">
      {view.items.filter(item => item.kind === "notice" && item.text.startsWith("Bypass ")).map(item => item.kind === "notice" && <div className="sub-note" key={item.key}>{item.text}</div>)}
      <div className="sub-crumb">
        <a
          href="#"
          onClick={(e) => {
            e.preventDefault();
            onBack();
          }}
        >
          <ArrowLeft size={13} aria-hidden /> {parentTitle}
        </a>
        <span aria-hidden>›</span>
        <span style={{ color: "var(--pu)" }}><Crew id={child?.agent ?? summary.data?.agent ?? "subagent"} /></span>
        <span className={`pill ${status === "done" ? "done" : status === "failed" ? "failed" : status === "stopped" ? "stopped" : ""}`}>
          {status === "doubt" ? "paused · watchdog" : status}
          {time ? ` · ${time}` : ""}
        </span>
      </div>
      <div className="task-card">
        <div className="msg-captain" style={{ minWidth: 0 }}>
          <div className="byline">{child ? `Delegation · from ${child.spawnedBy}` : "Captain · Delegation"}</div>
          <div className="label">TASK</div>
          <div className="goal">{goal || <span className="muted">Not known yet.</span>}</div>
        </div>
        <div className="facts">
          <div>
            <span>spawned by</span>
            <span>{child?.spawnedBy ?? "—"}</span>
          </div>
          <div>
            <span>tools used</span>
            <span>{toolNames.length ? toolNames.join(" · ") : "none yet"}</span>
          </div>
          {delivery && (
            <div>
              <span>delivery</span>
              <span>{DELIVERY[delivery] ?? delivery}</span>
            </div>
          )}
          <div>
            <span>model</span>
            <span className="mono" style={{ fontSize: 11 }}>
              {view.model ?? summary.data?.model ?? "—"}
              {(view.model ? view.variant : summary.data?.variant) ? ` · ${view.model ? view.variant : summary.data?.variant}` : ""}
            </span>
          </div>
        </div>
      </div>
      {calls.length > 0 ? (
        <div className="sub-calls">
          <ToolList calls={calls} />
        </div>
      ) : (
        <div className="muted">{stream.status === "connecting" ? "Loading its transcript…" : "No tool call yet."}</div>
      )}
      <div>
        <div className="label" style={{ marginBottom: 4 }}>{status === "done" ? "RESULT" : "FINDINGS SO FAR"}</div>
        {latest ? <Markdown text={latest} /> : <div className="muted">Nothing written yet.</div>}
      </div>
      {child?.reason && <div className="notice">{child.reason}</div>}
      <div className="sub-note">
        When the subagent finishes it emits <code>subagent.done</code>; the result joins the parent's next turn. The parent keeps working meanwhile.
      </div>
      {(running || paused) && (
        <div className="actions">
          {paused && (
            <button
              type="button"
              className="btn"
              disabled={acting}
              onClick={() => act(() => api.continueChild(parentId, childId, paused.token))}
            >
              Continue
            </button>
          )}
          <button type="button" className="btn ghost" disabled={acting} onClick={() => act(() => api.stop(childId))}>
            Stop
          </button>
          <span className="muted" style={{ fontSize: 12 }}>
            Subagents take no messages; guide them through the parent session.
          </span>
        </div>
      )}
      {error && <div className="composer-error">{error}</div>}
    </div>
  );
}
