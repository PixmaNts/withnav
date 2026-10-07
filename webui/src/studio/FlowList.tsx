// Left column: flows grouped by trigger, then the constants of the open flow.

import { ArrowsSplit, Hand, Plus, WarningCircle, X } from "@phosphor-icons/react";
import { useState } from "react";
import { ID_PATTERN } from "./graph";
import { healthWarnings } from "./diagnostics";
import type { Definition, FlowSummary, HealthRow, Location, Scalar } from "./types";

export const LOCATION_LABEL: Record<Location, string> = { workspace: "workspace", global: "global", builtin: "built-in" };

interface Props {
  flows: FlowSummary[];
  health: HealthRow[];
  current: { name: string; location: Location } | null;
  onOpen: (f: FlowSummary) => void;
  onToggle: (f: FlowSummary) => void;
  toggling: string | null;
  onNew: () => void;
  def: Definition | null;
  editConstants: ((f: (c: Definition["constants"]) => Definition["constants"]) => void) | null;
}

export function FlowList({ flows, health, current, onOpen, onToggle, toggling, onNew, def, editConstants }: Props) {
  const groups = new Map<string, FlowSummary[]>();
  for (const f of flows) {
    const k = f.trigger ?? "unreadable";
    groups.set(k, [...(groups.get(k) ?? []), f]);
  }
  // Same name in several locations: only the first one in workspace > global > builtin runs.
  const rank: Record<Location, number> = { workspace: 0, global: 1, builtin: 2 };
  const winner = new Map<string, Location>();
  for (const f of flows) {
    const w = winner.get(f.name);
    if (!w || rank[f.location] < rank[w]) winner.set(f.name, f.location);
  }
  return (
    <nav className="sf-list" aria-label="Flows">
      <button type="button" className="sf-new" onClick={onNew}>
        <Plus size={13} aria-hidden /> New flow
      </button>
      {!flows.length && <div className="sf-note" style={{ padding: "4px 8px" }}>No flow yet in this workspace, your configuration or Navai's built-ins.</div>}
      {[...groups].map(([trigger, list]) => (
        <div key={trigger} className="sf-group">
          <div className="sf-group-label mono">{trigger}</div>
          {list.map((f) => {
            const open = current?.name === f.name && current.location === f.location;
            const h = health.find((x) => x.name === f.name);
            const shadowed = winner.get(f.name) !== f.location;
            const key = `${f.location}:${f.name}`;
            return (
              <div key={key} className="sf-flow" aria-current={open ? "page" : undefined}>
                <button type="button" className="sf-flow-main" onClick={() => onOpen(f)}>
                  <span className="sf-flow-top">
                    <span className="mono sf-flow-name">{f.name}</span>
                    {f.blocking === null ? null : f.blocking ? (
                      <Hand size={12} className="muted" aria-label="blocking" />
                    ) : (
                      <ArrowsSplit size={12} className="muted" aria-label="asynchronous" />
                    )}
                    {f.errors.length > 0 && (
                      <span className="sf-flow-err" title={f.errors.map((e) => e.message).join("\n")}>
                        <WarningCircle size={12} weight="fill" aria-hidden /> {f.errors.length}
                      </span>
                    )}
                  </span>
                  <span className="sf-flow-sub">
                    <span className={`sf-loc ${f.location}`}>{LOCATION_LABEL[f.location]}</span>
                    {shadowed && <span className="muted" title={`A ${winner.get(f.name)} flow with the same name replaces this one.`}>replaced</span>}
                    {f.saved_since_reload && <span className="sf-unloaded" title="Saved since the last reload: what runs is still the previous version.">saved · not reloaded</span>}
                    {h && h.runs > 0 && (
                      <span className="muted" title={`Last 24 h: ${h.runs} runs, ${h.failures} failures, ${h.fallbacks_applied} fallbacks applied`}>
                        {h.runs} runs{h.fallbacks_applied ? <span className="sf-fb-count"> · {h.fallbacks_applied} fb</span> : null}
                      </span>
                    )}
                  </span>
                  {h && healthWarnings(h).map((message) => (
                    <span className="sf-health-warning" key={message}><WarningCircle size={12} aria-hidden /> {message}</span>
                  ))}
                </button>
                <button
                  type="button"
                  className="sf-toggle"
                  role="switch"
                  aria-checked={!!f.enabled}
                  aria-label={`${f.enabled ? "Disable" : "Enable"} ${f.name}`}
                  disabled={f.location === "builtin" || f.enabled === null || toggling === key}
                  title={f.location === "builtin" ? "Built-in flows cannot be edited. Create a flow with the same name to replace it." : "Saved to the file; takes effect at the next reload."}
                  onClick={() => onToggle(f)}
                >
                  <span />
                </button>
              </div>
            );
          })}
        </div>
      ))}
      {def && <Constants def={def} edit={editConstants} />}
    </nav>
  );
}

function parseScalar(text: string): Scalar {
  if (text === "true" || text === "false") return text === "true";
  const n = Number(text);
  return text.trim() !== "" && Number.isFinite(n) ? n : text;
}

function Constants({ def, edit }: { def: Definition; edit: Props["editConstants"] }) {
  const [name, setName] = useState("");
  const entries = Object.entries(def.constants);
  const nameProblem = name && (!ID_PATTERN.test(name) ? "Letters, digits and _." : name in def.constants ? "Already defined." : null);
  return (
    <div className="sf-group">
      <div className="sf-group-label">Constants · {def.flow.name}</div>
      {!entries.length && <div className="sf-note" style={{ padding: "2px 8px" }}>None. Thresholds and inputs can read a constant.</div>}
      {entries.map(([k, v]) => (
        <div key={k} className="sf-const">
          <span className="mono">{k}</span>
          <input
            className="sf-input mono sf-const-val"
            defaultValue={String(v)}
            key={String(v)}
            disabled={!edit}
            aria-label={`Value of ${k}`}
            onBlur={(e) => {
              const next = parseScalar(e.target.value);
              if (next !== v) edit?.((c) => ({ ...c, [k]: next }));
            }}
          />
          {edit && (
            <button type="button" className="icon-btn" aria-label={`Remove ${k}`} onClick={() => edit((c) => Object.fromEntries(Object.entries(c).filter(([x]) => x !== k)))}>
              <X size={11} />
            </button>
          )}
        </div>
      ))}
      {edit && (
        <form
          className="sf-const"
          onSubmit={(e) => {
            e.preventDefault();
            if (!name || nameProblem) return;
            edit((c) => ({ ...c, [name]: 0.5 }));
            setName("");
          }}
        >
          <input className="sf-input mono" value={name} onChange={(e) => setName(e.target.value)} placeholder="NEW_CONSTANT" aria-label="New constant name" aria-invalid={!!nameProblem} title={nameProblem || undefined} />
          <button type="submit" className="icon-btn" aria-label="Add constant" disabled={!name || !!nameProblem}>
            <Plus size={12} />
          </button>
        </form>
      )}
    </div>
  );
}
