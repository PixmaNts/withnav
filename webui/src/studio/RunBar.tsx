// Bottom bar: run the flow on a sample payload (real instinct call), or replay a recorded run (no
// model call), then show the path taken, calls, tokens, cost and the budget.

import { ClockCounterClockwise, Play, X } from "@phosphor-icons/react";
import { useEffect, useState } from "react";
import { actionLook, money, tokens } from "./meta";
import { TRIGGER_ID } from "./graph";
import { applicationLabel } from "./diagnostics";
import type { Definition, ReplayResult, RunRecord, RunSummary } from "./types";

interface Props {
  def: Definition;
  payload: string;
  setPayload: (text: string) => void;
  payloadError: string | null;
  run: RunRecord | null;
  replay: ReplayResult | null;
  busy: "sample" | "replay" | "view" | null;
  error: string | null;
  runs: RunSummary[];
  blockedReason: string | null;
  onSample: () => void;
  onReplay: (runId: string) => void;
  onViewRun: (runId: string) => void;
  onClear: () => void;
}

export function RunBar({ def, payload, setPayload, payloadError, run, replay, busy, error, runs, blockedReason, onSample, onReplay, onViewRun, onClear }: Props) {
  const [editing, setEditing] = useState(false);
  const [replayId, setReplayId] = useState("");
  useEffect(() => {
    if (!runs.some((r) => r.run_id === replayId)) setReplayId(runs.find((r) => r.mode === "live")?.run_id ?? runs[0]?.run_id ?? "");
  }, [runs, replayId]);
  const oneLine = (() => {
    try {
      return JSON.stringify(JSON.parse(payload));
    } catch {
      return payload.replace(/\s+/g, " ");
    }
  })();
  const shown = run ?? null;

  return (
    <div className="sf-runbar">
      <div className="sf-run-row">
        <span className="muted">Sample</span>
        <button type="button" className={`sf-payload mono${payloadError ? " bad" : ""}`} onClick={() => setEditing((e) => !e)} aria-expanded={editing} title="Edit the sample payload">
          {def.flow.on} · {oneLine}
        </button>
        <button type="button" className="sf-btn accent" onClick={onSample} disabled={!!busy || !!payloadError || !!blockedReason} title={blockedReason ?? "Runs the flow once with this payload. Calls the instinct; applies nothing."}>
          <Play size={12} aria-hidden /> {busy === "sample" ? "Running…" : "Run sample"}
        </button>
        <span className="sf-sep" />
        <select className="sf-select" value={replayId} onChange={(e) => setReplayId(e.target.value)} disabled={!runs.length} aria-label="Recorded run to replay">
          {!runs.length && <option value="">No recorded run</option>}
          {runs.map((r) => (
            <option key={r.run_id} value={r.run_id}>
              {r.mode} · {new Date(r.started_at).toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" })} · {r.status} · {r.duration_ms} ms
            </option>
          ))}
        </select>
        <button type="button" className="sf-btn ghost" onClick={() => replayId && onViewRun(replayId)} disabled={!!busy || !replayId} title="Read the recorded run, including what the host applied. Runs nothing.">
          {busy === "view" ? "Loading…" : "View run"}
        </button>
        <button type="button" className="sf-btn" onClick={() => replayId && onReplay(replayId)} disabled={!!busy || !replayId || !!blockedReason} title={blockedReason ?? "Runs the recorded answers through this version of the flow. No model call."}>
          <ClockCounterClockwise size={12} aria-hidden /> {busy === "replay" ? "Replaying…" : "Replay"}
        </button>
        {(shown || replay) && (
          <button type="button" className="sf-btn ghost" onClick={onClear}>
            <X size={12} aria-hidden /> Clear run
          </button>
        )}
        {shown && <RunStats run={shown} def={def} replay={!!replay} />}
      </div>
      {editing && (
        <div className="sf-payload-edit">
          <textarea className="sf-textarea mono" rows={6} value={payload} onChange={(e) => setPayload(e.target.value)} spellCheck={false} aria-label="Sample payload (JSON)" />
          <div className="sf-note">{payloadError ?? "The event payload, as JSON. Host context (session, workspace…) is collected by the server."}</div>
        </div>
      )}
      {error && <div className="sf-run-error" role="alert">{error}</div>}
      {blockedReason && !error && <div className="sf-note">{blockedReason}</div>}
      {replay && !replay.comparable && (
        <div className="sf-run-error" role="status">
          Replay not comparable: {replay.nodes.filter((n) => n.status === "question_changed" || n.status === "missing_capture").map((n) => `${n.id} (${n.status === "question_changed" ? "question changed" : "no recorded answer"})`).join(", ")}. No answer is invented, so there is no outcome.
        </div>
      )}
      {shown && <PathLine run={shown} def={def} />}
    </div>
  );
}

function RunStats({ run, def, replay }: { run: RunRecord; def: Definition; replay: boolean }) {
  const calls = run.batches.length;
  const usage = run.usage;
  const budget = run.budget_ms ?? (def.flow.blocking ? def.flow.budget_ms : null);
  const pct = budget ? Math.min(100, Math.round((run.duration_ms / budget) * 100)) : 100;
  const over = budget !== null && run.duration_ms > budget;
  return (
    <div className="sf-stats">
      <span className="muted">
        {replay ? "replay · no model call" : `${calls} instinct call${calls === 1 ? "" : "s"}`} · {money(run.cost_usd)}
        {usage ? ` · ${tokens(usage.input_tokens)} tok in · ${tokens(usage.output_tokens)} out` : replay ? "" : " · tokens unknown"}
      </span>
      <div className={`sf-budget${over ? " over" : ""}`} title={budget ? `${run.duration_ms} of ${budget} ms` : "asynchronous: no budget"}>
        <div style={{ width: `${pct}%` }} />
      </div>
      <span className="mono">{budget ? `${run.duration_ms} / ${budget} ms` : `${run.duration_ms} ms · async`}</span>
    </div>
  );
}

function PathLine({ run, def }: { run: RunRecord; def: Definition }) {
  const byId = new Map(def.node.map((n) => [n.id, n]));
  const chips: { text: string; color: string }[] = [{ text: def.flow.on, color: "var(--at)" }];
  if (run.status === "skipped") chips.push({ text: "skipped: when is false", color: "var(--mu)" });
  for (const r of run.nodes) {
    if (r.status === "not_reached") continue;
    const n = byId.get(r.id);
    if (!n || r.id === TRIGGER_ID) continue;
    if (r.status === "failed") chips.push({ text: `${r.id} failed`, color: "var(--rd)" });
    else if (n.type === "action") chips.push({ text: n.action === n.id ? n.id : `${n.id} (${n.action})`, color: run.status === "failure" ? "var(--mu)" : actionLook(n.action).color });
    else chips.push({ text: `${r.id} = ${r.port ?? "?"}`, color: "var(--tx)" });
  }
  const outcome =
    run.status === "failure"
      ? `failed (${run.failure?.kind}): ${run.failure?.message}. Fallback: ${run.fallback_actions.map((a) => a.action).join(" › ") || "none"}`
      : run.actions.length
        ? `proposes ${run.actions.map((a) => a.action).join(", ")}`
        : run.status === "skipped"
          ? "nothing to do"
          : "proposes nothing";
  return (
    <div className="sf-path">
      <span className="muted">Path</span>
      {chips.map((c, i) => (
        <span key={i} className="mono" style={{ color: c.color }}>
          {i > 0 && <span className="muted"> › </span>}
          {c.text}
        </span>
      ))}
      <span className={`sf-outcome${run.status === "failure" ? " bad" : ""}`}>{outcome}</span>
      <span className={`sf-application${run.application.status === "failed" ? " bad" : ""}`}>
        application · {applicationLabel(run.application.status)}
        {run.application.actions.some((action) => action.status !== run.application.status) && (
          <small className="muted"> · {run.application.actions.map((action) => `${action.action}: ${action.status.replaceAll("_", " ")}`).join(", ")}</small>
        )}
      </span>
    </div>
  );
}
