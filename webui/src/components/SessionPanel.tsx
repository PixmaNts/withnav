import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { keys, useLedger, useSessionMemory } from "../api/queries";
import type { ClaimStatus, CriterionStatus, SessionLedger } from "../api/types";
import type { Chat } from "../session/useChat";

const CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/** The time a ULID carries in its first ten characters, in milliseconds; null for anything else. */
export function ulidTime(id: string): number | null {
  if (id.length !== 26) return null;
  let ms = 0;
  for (const char of id.slice(0, 10).toUpperCase()) {
    const digit = CROCKFORD.indexOf(char);
    if (digit < 0) return null;
    ms = ms * 32 + digit;
  }
  return ms;
}

/** `just now`, `2 min ago`, `3 h ago`, `4 d ago`. */
export function ago(then: number, now = Date.now()): string {
  const seconds = Math.max(0, Math.round((now - then) / 1000));
  if (seconds < 60) return "just now";
  if (seconds < 3600) return `${Math.floor(seconds / 60)} min ago`;
  if (seconds < 86_400) return `${Math.floor(seconds / 3600)} h ago`;
  return `${Math.floor(seconds / 86_400)} d ago`;
}

const CRITERION_LABEL: Record<CriterionStatus, string> = { supported: "supported", contradicted: "contradicted", not_checked: "not checked" };
const CLAIM_LABEL: Record<ClaimStatus, string> = { supported: "supported", contradicted: "contradicted", unverified: "unverified" };

/** Files listed under the ledger's count, most recently changed first. */
const LEDGER_FILES = 3;
/** Open questions listed under their count. */
const LEDGER_QUESTIONS = 3;
/** An objective longer than this can be expanded from its three-line form. */
const LONG_OBJECTIVE = 120;

/** Reads the ledger and the memory again whenever the stream says the session changed. */
function useRefreshOnChange(id: string | null, epoch: number) {
  const qc = useQueryClient();
  const seen = useRef(epoch);
  useEffect(() => {
    if (seen.current === epoch) return;
    seen.current = epoch;
    if (!id) return;
    void qc.invalidateQueries({ queryKey: keys.ledger(id) });
    void qc.invalidateQueries({ queryKey: keys.sessionMemory(id) });
  }, [qc, id, epoch]);
}

export function SessionTab({ chat, workspace }: { chat: Chat; workspace: string | null }) {
  const id = chat.summary?.session_id ?? null;
  useRefreshOnChange(id, chat.view.sessionEpoch);
  const ledger = useLedger(id);
  const memory = useSessionMemory(id);
  if (!id) return <div className="panel-empty">The session panel fills in once the session starts.</div>;
  return (
    <div className="session-panel">
      {ledger.data ? (
        <>
          <GoalSection ledger={ledger.data} />
          <LedgerSection ledger={ledger.data} />
        </>
      ) : (
        <>
          <div className="section-label">GOAL · LEDGER</div>
          <div className="panel-empty">{ledger.isError ? "The ledger could not be read from this server." : "Reading the ledger…"}</div>
        </>
      )}
      <div className="section-label sp-memory-head">
        <span>MEMORY · applies to this session</span>
        <a className="link-btn" href={`#/memory${workspace ? `?ws=${encodeURIComponent(workspace)}` : ""}`}>Open Memory</a>
      </div>
      {memory.isError && <div className="panel-empty">The memory could not be read from this server.</div>}
      {memory.data && !memory.data.length && <div className="panel-empty">No memory item applies to this session yet. Add one with <span className="mono">/remember</span>.</div>}
      {!!memory.data?.length && (
        <div className="sp-list">
          {memory.data.map((item) => (
            <div key={item.id} className="sp-row" data-memory={item.id} title={item.current.why ?? undefined}>
              {item.current.title}
              <span className="muted"> · {item.scope}</span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function GoalSection({ ledger }: { ledger: SessionLedger }) {
  const [open, setOpen] = useState(false);
  const goal = ledger.goal;
  if (!goal)
    return (
      <>
        <div className="section-label">GOAL</div>
        <div className="panel-empty sp-empty">
          No goal for this session. Set one with <span className="mono">/goal</span> and Nav checks it against the criteria as the work moves.
        </div>
      </>
    );
  // A check of an older revision says nothing about the criteria as they stand now.
  const check = ledger.goal_check && ledger.goal_check.revision === goal.revision ? ledger.goal_check : null;
  const checkedAt = check ? ulidTime(check.turn_id) : null;
  const long = goal.objective.length > LONG_OBJECTIVE;
  return (
    <>
      <div className="section-label">GOAL</div>
      <div className={`sp-objective${open ? "" : " clamp3"}`}>{goal.objective}</div>
      {long && (
        <button type="button" className="link-btn" aria-expanded={open} onClick={() => setOpen(!open)}>
          {open ? "Show less" : "Show more"}
        </button>
      )}
      <div className="sp-meta">
        <span className="mono muted">revision {goal.revision}</span>
        <span className={`sp-status ${goal.status}`}>{goal.status}</span>
        <span className="muted sp-checked" title={checkedAt ? new Date(checkedAt).toLocaleString() : undefined}>
          {check ? `last checked${checkedAt ? ` ${ago(checkedAt)}` : ""}` : "not checked yet"}
        </span>
      </div>
      {!!goal.criteria.length && (
        <div className="sp-list">
          {goal.criteria.map((text, i) => {
            const n = i + 1;
            const found = check?.statuses.find((s) => s.index === n);
            const status: CriterionStatus = found?.status ?? "not_checked";
            return (
              <div key={n} className="sp-crit" data-criterion={n} data-status={status} title={found?.evidence || undefined}>
                <span className="sp-crit-text">
                  <span className="muted">Criterion {n}</span>
                  <br />
                  {text}
                </span>
                <span className="sp-state">{CRITERION_LABEL[status]}</span>
              </div>
            );
          })}
        </div>
      )}
      {!goal.criteria.length && <div className="muted sp-note">No success criteria yet.</div>}
      {!!goal.criteria.length && <div className="muted sp-note">Hover a criterion for its evidence.</div>}
      {!!check?.claims.length && (
        <>
          <div className="section-label sp-gap">CLAIMS · agent's last turn</div>
          <div className="sp-list">
            {check.claims.map((claim, i) => (
              <div key={i} className="sp-crit" data-claim={i + 1} data-status={claim.status} title={claim.evidence || undefined}>
                <span className="sp-crit-text">{claim.claim}</span>
                <span className="sp-state">{CLAIM_LABEL[claim.status]}</span>
              </div>
            ))}
          </div>
        </>
      )}
    </>
  );
}

function LedgerSection({ ledger }: { ledger: SessionLedger }) {
  const worst = ledger.repeated_failures[0];
  return (
    <>
      <div className="section-label sp-gap">LEDGER</div>
      <div className="sp-kv"><span className="muted">Turns</span><span className="mono" data-ledger="turns">{ledger.turns.count}</span></div>
      <div className="sp-kv"><span className="muted">Files changed</span><span className="mono" data-ledger="files">{ledger.files_touched.length}</span></div>
      {ledger.files_touched.slice(0, LEDGER_FILES).map((file) => (
        <div key={file.path} className="mono muted sp-file" data-file={file.path} title={file.path}>{file.path}</div>
      ))}
      <div className="sp-kv">
        <span className="muted">Repeated failures</span>
        <span className={`mono${ledger.repeated_failures.length ? " bad" : ""}`} data-ledger="failures">{ledger.repeated_failures.length}</span>
      </div>
      {worst && <div className="muted sp-note">Worst: <span className="mono sp-cmd">{worst.command}</span> failed {worst.count}× in this session.</div>}
      <div className="sp-kv"><span className="muted">Open questions</span><span className="mono" data-ledger="questions">{ledger.open_questions.length}</span></div>
      {ledger.open_questions.slice(0, LEDGER_QUESTIONS).map((q) => (
        <div key={q.id} className="muted sp-note">{q.title}</div>
      ))}
    </>
  );
}
