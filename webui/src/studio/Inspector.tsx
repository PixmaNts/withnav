// The right column: the selected card's fields, per node type, plus the last run's raw answer.

import { Plus, Trash, WarningCircle, X } from "@phosphor-icons/react";
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { changeDecisionType, ID_PATTERN, isDecision, mapNode, nodeById, prunePorts, renameNode, renameOption, removeNode, TRIGGER_ID } from "./graph";
import { aliasFor, EMPTY_DECISION_WARNING, hasEmptyDecisionState, inputGroups, insertInputAlias, renderState, toggleInput } from "./inputs";
import { actionLook, fmt, lookOf, thresholdValue, TYPE_LOOK } from "./meta";
import type { Catalogue, Definition, Diagnostic, FallbackAction, FallbackKey, FlowNode, ParamValue, ReplayResult, RunRecord } from "./types";
import { FALLBACK_KEYS } from "./types";

type Edit = ((f: (def: Definition) => Definition) => void) | null;

interface Props {
  def: Definition;
  catalogue: Catalogue | undefined;
  selected: string;
  onSelect: (id: string) => void;
  edit: Edit;
  run: RunRecord | null;
  replay: ReplayResult | null;
  diagnostics: Diagnostic[];
  payload: Record<string, unknown> | undefined;
}

export function Inspector({ def, catalogue, selected, onSelect, edit, run, replay, diagnostics, payload }: Props) {
  const node = selected === TRIGGER_ID ? null : nodeById(def, selected) ?? null;
  const look = lookOf(node);
  const errors = diagnostics.filter((d) => (node ? d.node_id === node.id : d.node_id === null && (d.path.startsWith("flow") || d.path.startsWith("fallback"))));
  const setNode = (f: (n: FlowNode) => FlowNode) => edit?.((d) => mapNode(d, selected, f));

  return (
    <aside className="sf-inspector" aria-label="Inspector">
      <div className="sf-insp-head">
        <span style={{ color: look.color }}>{look.icon(16)}</span>
        <span className="sf-card-tag" style={{ color: look.color }}>{look.tag}</span>
        <span className="mono sf-insp-name">{node ? node.id : def.flow.on}</span>
      </div>
      <div className="sf-insp-body">
        {errors.length > 0 && (
          <ul className="sf-insp-errors" role="alert">
            {errors.map((e, i) => (
              <li key={i}>
                <WarningCircle size={13} aria-hidden /> <span>{e.message}</span>
              </li>
            ))}
          </ul>
        )}
        {!node ? (
          <TriggerFields def={def} catalogue={catalogue} edit={edit} payload={payload} diagnostics={diagnostics} />
        ) : (
          <>
            <IdFields node={node} def={def} edit={edit} onRenamed={onSelect} />
            {isDecision(node) && <DecisionFields node={node} def={def} setNode={setNode} run={run} disabled={!edit} />}
            {node.type === "expression" && <ExpressionFields node={node} setNode={setNode} disabled={!edit} />}
            {node.type === "action" && <ActionFields node={node} def={def} catalogue={catalogue} setNode={setNode} disabled={!edit} />}
            <InputsPicker node={node} def={def} catalogue={catalogue} setNode={setNode} disabled={!edit} />
            {isDecision(node) && (
              <Section label="State as sent">
                <pre className="sf-pre mono">{renderState(node, def, payload, run)}</pre>
                <div className="sf-note">{run ? "Values from the run shown on the canvas." : "Values from the sample payload; host context shows as absent until a run."}</div>
              </Section>
            )}
            <RunDetail node={node} run={run} replay={replay} />
            {edit && (
              <button type="button" className="sf-danger" onClick={() => { edit((d) => removeNode(d, node.id)); onSelect(TRIGGER_ID); }}>
                <Trash size={13} aria-hidden /> Delete node
              </button>
            )}
          </>
        )}
      </div>
    </aside>
  );
}

function Section({ label, aside, children }: { label: ReactNode; aside?: ReactNode; children: ReactNode }) {
  return (
    <section className="sf-sec">
      <div className="sf-sec-label">
        <span>{label}</span>
        {aside}
      </div>
      {children}
    </section>
  );
}

/** A text input that commits on blur or Enter, so ids are not renamed on every keystroke. */
function Commit({ value, onCommit, disabled, mono, placeholder, invalid }: { value: string; onCommit: (v: string) => boolean | void; disabled?: boolean; mono?: boolean; placeholder?: string; invalid?: (v: string) => string | null }) {
  const [draft, setDraft] = useState(value);
  useEffect(() => setDraft(value), [value]);
  const problem = invalid?.(draft) ?? null;
  const commit = () => {
    if (draft === value) return;
    if (problem || onCommit(draft) === false) setDraft(value);
  };
  return (
    <>
      <input
        className={`sf-input${mono ? " mono" : ""}`}
        value={draft}
        disabled={disabled}
        placeholder={placeholder}
        aria-invalid={!!problem}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === "Enter") commit();
          if (e.key === "Escape") setDraft(value);
        }}
      />
      {problem && draft !== value && <div className="sf-field-err">{problem}</div>}
    </>
  );
}

function IdFields({ node, def, edit, onRenamed }: { node: FlowNode; def: Definition; edit: Edit; onRenamed: (id: string) => void }) {
  const idProblem = (v: string) => (v === node.id ? null : !ID_PATTERN.test(v) ? "Letters, digits and _, starting with a letter or _." : v === TRIGGER_ID || nodeById(def, v) ? "Another node has this id." : null);
  return (
    <div className="sf-grid2">
      <label className="sf-field">
        <span className="sf-sec-label">ID · used by references</span>
        <Commit
          value={node.id}
          mono
          disabled={!edit}
          invalid={idProblem}
          onCommit={(v) => {
            const next = renameNode(def, node.id, v);
            if (!next || !edit) return false;
            edit(() => next);
            onRenamed(v);
          }}
        />
      </label>
      <label className="sf-field">
        <span className="sf-sec-label">Label · display only</span>
        <Commit value={node.label ?? ""} disabled={!edit} placeholder={node.id} onCommit={(v) => edit?.((d) => mapNode(d, node.id, (n) => ({ ...n, label: v.trim() ? v : null })))} />
      </label>
    </div>
  );
}

function DecisionFields({ node, def, setNode, run, disabled }: { node: Extract<FlowNode, { type: "noul" | "choice" | "score" }>; def: Definition; setNode: (f: (n: FlowNode) => FlowNode) => void; run: RunRecord | null; disabled: boolean }) {
  const r = run?.nodes.find((n) => n.id === node.id);
  const answer = r?.status === "completed" ? r.answer : null;
  return (
    <>
      <Section label="Decision type · System One">
        <div className="sf-types" role="radiogroup" aria-label="Decision type">
          {(["noul", "choice", "score"] as const).map((t) => (
            <button key={t} type="button" role="radio" aria-checked={node.type === t} disabled={disabled} className="sf-type" onClick={() => setNode((n) => changeDecisionType(n, t))}>
              <span className="mono">{t}</span>
              <span className="sf-type-sub">{TYPE_LOOK[t].sub}</span>
            </button>
          ))}
        </div>
      </Section>
      <Section label="Instructions · the question asked">
        <textarea className="sf-textarea" rows={3} value={node.instructions} disabled={disabled} onChange={(e) => setNode((n) => ({ ...n, instructions: e.target.value }) as FlowNode)} placeholder="Ask one plain question about the inputs." />
      </Section>
      {node.type === "noul" && <NoulThreshold node={node} def={def} setNode={setNode} answer={answer?.type === "noul" ? answer.noul : null} disabled={disabled} />}
      {node.type === "choice" && <ChoiceOptions node={node} setNode={setNode} probabilities={answer?.type === "choice" ? answer.probabilities : null} chosen={answer?.type === "choice" ? answer.choice : null} disabled={disabled} />}
      {node.type === "score" && <ScoreLevels node={node} setNode={setNode} probabilities={answer?.type === "score" ? answer.probabilities : null} disabled={disabled} />}
    </>
  );
}

function NoulThreshold({ node, def, setNode, answer, disabled }: { node: Extract<FlowNode, { type: "noul" }>; def: Definition; setNode: (f: (n: FlowNode) => FlowNode) => void; answer: number | null; disabled: boolean }) {
  const t = thresholdValue(node, def);
  const numericConstants = Object.entries(def.constants).filter(([, v]) => typeof v === "number");
  const fromConstant = typeof node.threshold === "string";
  return (
    <Section label="Yes-probability · threshold">
      <div className="sf-track" aria-hidden>
        {t !== null && <div className="sf-track-mark" style={{ left: `${t * 100}%` }} />}
        {answer !== null && <div className="sf-track-ans" style={{ left: `${answer * 100}%` }} title={`answer ${fmt(answer)}`} />}
      </div>
      <div className="sf-track-legend">
        <span>0 · port no</span>
        <span className="mono" style={{ color: "var(--or)" }}>≥ {t ?? "?"}</span>
        <span>1 · port yes</span>
      </div>
      <div className="sf-row">
        <input type="range" min={0} max={1} step={0.01} value={t ?? 0.5} disabled={disabled || fromConstant} onChange={(e) => setNode((n) => ({ ...n, threshold: Number(e.target.value) }) as FlowNode)} aria-label="Threshold" />
        <select className="sf-select" value={fromConstant ? node.threshold : ""} disabled={disabled} onChange={(e) => setNode((n) => ({ ...n, threshold: e.target.value || (t ?? 0.5) }) as FlowNode)} aria-label="Threshold source">
          <option value="">number</option>
          {numericConstants.map(([k, v]) => (
            <option key={k} value={`constant.${k}`}>
              {k} = {String(v)}
            </option>
          ))}
        </select>
      </div>
    </Section>
  );
}

function ChoiceOptions({ node, setNode, probabilities, chosen, disabled }: { node: Extract<FlowNode, { type: "choice" }>; setNode: (f: (n: FlowNode) => FlowNode) => void; probabilities: Record<string, number> | null; chosen: string | null; disabled: boolean }) {
  const entries = Object.entries(node.criteria);
  const setCriteria = (criteria: Record<string, string>) => setNode((n) => prunePorts({ ...n, criteria } as FlowNode));
  return (
    <Section label="Options · each one is a port">
      <div className="sf-opts">
        {entries.map(([k, v]) => {
          const p = probabilities?.[k];
          return (
            <div key={k} className={`sf-opt${chosen === k ? " chosen" : ""}`}>
              <div className="sf-row">
                <Commit
                  value={k}
                  mono
                  disabled={disabled}
                  invalid={(x) => (x === k ? null : !ID_PATTERN.test(x) ? "Letters, digits and _." : x in node.criteria ? "Already an option." : null)}
                  onCommit={(x) => {
                    const next = renameOption(node, k, x);
                    if (!next) return false;
                    setNode(() => next);
                  }}
                />
                {p !== undefined && <span className="mono sf-opt-pct">{Math.round(p * 100)} %</span>}
                {!disabled && entries.length > 2 && (
                  <button type="button" className="icon-btn" aria-label={`Remove option ${k}`} onClick={() => setCriteria(Object.fromEntries(entries.filter(([x]) => x !== k)))}>
                    <X size={12} />
                  </button>
                )}
              </div>
              <input className="sf-input" value={v} disabled={disabled} placeholder="When to pick this option" onChange={(e) => setCriteria({ ...node.criteria, [k]: e.target.value })} />
              {p !== undefined && (
                <div className="sf-bar">
                  <div style={{ width: `${p * 100}%` }} />
                </div>
              )}
            </div>
          );
        })}
      </div>
      {!disabled && entries.length < 16 && (
        <button type="button" className="sf-link" onClick={() => { let i = entries.length + 1; while (`option_${i}` in node.criteria) i++; setCriteria({ ...node.criteria, [`option_${i}`]: "" }); }}>
          <Plus size={12} aria-hidden /> Add option
        </button>
      )}
    </Section>
  );
}

function ScoreLevels({ node, setNode, probabilities, disabled }: { node: Extract<FlowNode, { type: "score" }>; setNode: (f: (n: FlowNode) => FlowNode) => void; probabilities: Record<string, number> | null; disabled: boolean }) {
  const idx = node.levels.indexOf(node.at_least);
  const setLevels = (levels: string[], at_least = node.at_least) => setNode((n) => ({ ...n, levels, at_least: levels.includes(at_least) ? at_least : levels[Math.min(idx, levels.length - 1)] ?? "" }) as FlowNode);
  return (
    <Section label="Levels · weakest first (2–10)">
      <div className="sf-levels">
        {node.levels.map((l, i) => (
          <div key={i} className={`sf-level${i >= idx && idx >= 0 ? " over" : ""}`}>
            <span className="mono muted">{i}</span>
            <input className="sf-input" value={l} disabled={disabled} onChange={(e) => { const levels = [...node.levels]; levels[i] = e.target.value; setLevels(levels, i === idx ? e.target.value : node.at_least); }} />
            {probabilities?.[String(i)] !== undefined && <span className="mono muted">{Math.round(probabilities[String(i)]! * 100)} %</span>}
            {!disabled && node.levels.length > 2 && (
              <button type="button" className="icon-btn" aria-label={`Remove level ${l}`} onClick={() => setLevels(node.levels.filter((_, j) => j !== i))}>
                <X size={12} />
              </button>
            )}
          </div>
        ))}
      </div>
      {!disabled && node.levels.length < 10 && (
        <button type="button" className="sf-link" onClick={() => setLevels([...node.levels, `level ${node.levels.length + 1}`])}>
          <Plus size={12} aria-hidden /> Add level
        </button>
      )}
      <div className="sf-row" style={{ marginTop: 8 }}>
        <span className="sf-note">At least</span>
        <select className="sf-select" value={node.at_least} disabled={disabled} onChange={(e) => setNode((n) => ({ ...n, at_least: e.target.value }) as FlowNode)} aria-label="At least level">
          {node.levels.map((l, i) => (
            <option key={i} value={l}>
              {i} · {l}
            </option>
          ))}
        </select>
        <span className="sf-note">goes to the at_least port.</span>
      </div>
    </Section>
  );
}

function ExpressionFields({ node, setNode, disabled }: { node: Extract<FlowNode, { type: "expression" }>; setNode: (f: (n: FlowNode) => FlowNode) => void; disabled: boolean }) {
  const assigns = Object.entries(node.assign);
  const setAssign = (assign: Record<string, string>) => setNode((n) => ({ ...n, assign }) as FlowNode);
  const names = [...Object.keys(node.inputs), ...Object.keys(node.assign)];
  return (
    <>
      <Section label="Condition · true or false">
        <textarea className="sf-textarea mono sf-expr" rows={2} value={node.condition} disabled={disabled} placeholder='asked_for < 0.5 && agent == "plan"' onChange={(e) => setNode((n) => ({ ...n, condition: e.target.value }) as FlowNode)} />
      </Section>
      <Section label="Variables · computed before the condition">
        {assigns.map(([k, v]) => (
          <div key={k} className="sf-row sf-assign">
            <Commit
              value={k}
              mono
              disabled={disabled}
              invalid={(x) => (x === k ? null : !ID_PATTERN.test(x) ? "Letters, digits and _." : x in node.assign || x in node.inputs ? "Name already used." : null)}
              onCommit={(x) => setAssign(Object.fromEntries(assigns.map(([a, b]) => [a === k ? x : a, b])))}
            />
            <span className="muted">=</span>
            <input className="sf-input mono sf-expr" value={v} disabled={disabled} onChange={(e) => setAssign({ ...node.assign, [k]: e.target.value })} />
            {!disabled && (
              <button type="button" className="icon-btn" aria-label={`Remove ${k}`} onClick={() => setAssign(Object.fromEntries(assigns.filter(([a]) => a !== k)))}>
                <X size={12} />
              </button>
            )}
          </div>
        ))}
        {!disabled && (
          <button type="button" className="sf-link" onClick={() => { let i = assigns.length + 1; while (`VAR_${i}` in node.assign) i++; setAssign({ ...node.assign, [`VAR_${i}`]: "" }); }}>
            <Plus size={12} aria-hidden /> Add variable
          </button>
        )}
      </Section>
      <Section label="Names in scope">
        <div className="sf-chips">{names.length ? names.map((n) => <span key={n} className="sf-chip mono">{n}</span>) : <span className="sf-note">Add inputs below to use them here.</span>}</div>
        <div className="sf-note">Only these names, numbers, strings, true/false, `is_set(x)` and `if(c, a, b)`. No instinct call. Ports: true, false.</div>
      </Section>
    </>
  );
}

function ParamInput({ value, onChange, disabled, aliases, multiline }: { value: ParamValue | undefined; onChange: (v: ParamValue) => void; disabled: boolean; aliases: string[]; multiline: boolean }) {
  const isRef = typeof value === "object" && value !== null;
  return (
    <div className="sf-param">
      {isRef ? (
        <select className="sf-select mono" value={value.ref} disabled={disabled} onChange={(e) => onChange({ ref: e.target.value })}>
          {aliases.map((a) => (
            <option key={a} value={a}>
              {a}
            </option>
          ))}
        </select>
      ) : multiline ? (
        <textarea className="sf-textarea" rows={2} value={String(value ?? "")} disabled={disabled} onChange={(e) => onChange(e.target.value)} />
      ) : (
        <input className="sf-input" value={String(value ?? "")} disabled={disabled} onChange={(e) => onChange(e.target.value)} />
      )}
      {aliases.length > 0 && !disabled && (
        <button type="button" className="sf-mini" title={isRef ? "Use literal text" : "Use one input as the value"} onClick={() => onChange(isRef ? "" : { ref: aliases[0]! })}>
          {isRef ? "text" : "input"}
        </button>
      )}
    </div>
  );
}

function ActionFields({ node, def, catalogue, setNode, disabled }: { node: Extract<FlowNode, { type: "action" }>; def: Definition; catalogue: Catalogue | undefined; setNode: (f: (n: FlowNode) => FlowNode) => void; disabled: boolean }) {
  const allowed = actionsFor(def, catalogue);
  const spec = catalogue?.actions.find((a) => a.name === node.action);
  const look = actionLook(node.action);
  const aliases = Object.keys(node.inputs);
  const hasThen = (node.next.then ?? []).length > 0;
  return (
    <>
      <Section label="Action">
        <select
          className="sf-select"
          value={node.action}
          disabled={disabled}
          onChange={(e) => {
            const next = catalogue?.actions.find((a) => a.name === e.target.value);
            setNode((n) => ({ ...n, action: e.target.value, params: Object.fromEntries((next?.params ?? []).filter((p) => p.required || (n as typeof node).params[p.name] !== undefined).map((p) => [p.name, (n as typeof node).params[p.name] ?? (p.value_type === "boolean" ? false : "")])) }) as FlowNode);
          }}
        >
          {allowed.map((a) => (
            <option key={a.name} value={a.name} disabled={!!a.why}>
              {actionLook(a.name).label} ({a.name}){a.why ? ` · ${a.why}` : ""}
            </option>
          ))}
          {!allowed.some((a) => a.name === node.action) && <option value={node.action}>{node.action} · not in the catalogue</option>}
        </select>
        <div className="sf-note">
          {look.describe} {spec?.kind === "verdict" ? "Verdict: at most one per run." : "Effect."}
        </div>
      </Section>
      {(spec?.params ?? []).length > 0 && (
        <Section label="Parameters">
          {spec!.params.map((p) => (
            <label key={p.name} className="sf-field">
              <span className="sf-note mono">{p.name}</span>
              {p.value_type === "boolean" ? <input type="checkbox" checked={node.params[p.name] === true} disabled={disabled} onChange={(e) => setNode((n) => ({ ...n, params: { ...(n as typeof node).params, [p.name]: e.target.checked } }) as FlowNode)} /> : <ParamInput value={node.params[p.name]} aliases={aliases} disabled={disabled} multiline={p.value_type === "string"} onChange={(v) => setNode((n) => ({ ...n, params: { ...(n as typeof node).params, [p.name]: v } }) as FlowNode)} />}
            </label>
          ))}
          <div className="sf-note">Text may use {"{alias}"} for a declared input; an absent one shows as —.</div>
        </Section>
      )}
      <div className="sf-note">{hasThen ? "The then port runs another action after this one." : "End of chain. Wire the then port to run another action after it."}</div>
    </>
  );
}

/** Catalogue actions with why each one cannot be used on this flow, if so. */
export function actionsFor(def: Definition, catalogue: Catalogue | undefined) {
  return (catalogue?.actions ?? []).map((a) => ({
    name: a.name,
    kind: a.kind,
    why: !a.allowed_triggers.includes(def.flow.on) ? `not on ${def.flow.on}` : a.kind === "verdict" && !def.flow.blocking ? "blocking flows only" : null,
  }));
}

function InputsPicker({ node, def, catalogue, setNode, disabled }: { node: FlowNode; def: Definition; catalogue: Catalogue | undefined; setNode: (f: (n: FlowNode) => FlowNode) => void; disabled: boolean }) {
  const [open, setOpen] = useState(false);
  const groups = useMemo(() => inputGroups(def, catalogue, node.id), [def, catalogue, node.id]);
  const chosen = new Set(Object.values(node.inputs));
  const entries = Object.entries(node.inputs);
  return (
    <Section
      label={isDecision(node) ? "State · inputs sent with the question" : "Inputs"}
      aside={
        !disabled && (
          <button type="button" className="sf-link" onClick={() => setOpen((o) => !o)} aria-expanded={open}>
            {open ? "Done" : "+ Add input"}
          </button>
        )
      }
    >
      {!open ? (
        <div className="sf-chips">
          {entries.length ? (
            entries.map(([alias, ref]) => (
              <span key={alias} className="sf-chip mono" title={ref}>
                {alias}
                {alias !== ref.split(".").pop() && <span className="muted"> ← {ref}</span>}
              </span>
            ))
          ) : (
            <span className="sf-note">Reads nothing yet.</span>
          )}
        </div>
      ) : (
        <div className="sf-catalog" role="group" aria-label="Input catalogue">
          {groups.map((g) => (
            <div key={g.label}>
              <div className="sf-cat-group">{g.label}</div>
              {g.options.map((o) => {
                const on = chosen.has(o.reference);
                return (
                  <label key={o.reference} className={`sf-cat-item${o.disabled && !on ? " off" : ""}`} title={o.disabled ?? undefined}>
                    <input type="checkbox" checked={on} disabled={!!o.disabled && !on} onChange={() => setNode((n) => toggleInput(n, o.reference))} />
                    <span className="mono">{o.reference}</span>
                    <span className="sf-cat-note">
                      {o.mayBeAbsent && <span className="sf-absent" title="The run may not pass through this node: the value can be absent. Test it with is_set().">may be absent</span>}
                      {o.disabled ? "not upstream" : o.note}
                    </span>
                  </label>
                );
              })}
            </div>
          ))}
        </div>
      )}
      {hasEmptyDecisionState(node) && <div className="sf-field-err" role="status"><WarningCircle size={13} aria-hidden /> {EMPTY_DECISION_WARNING}</div>}
    </Section>
  );
}

function RunDetail({ node, run, replay }: { node: FlowNode; run: RunRecord | null; replay: ReplayResult | null }) {
  const r = run?.nodes.find((n) => n.id === node.id);
  const replayed = replay?.nodes.find((n) => n.id === node.id);
  if (!r && !replayed) return null;
  const batch = r?.batch_id ? run?.batches.find((b) => b.id === r.batch_id) : undefined;
  const raw = r?.answer
    ? JSON.stringify(r.answer, null, 2)
    : r?.status === "completed" && node.type === "expression"
      ? JSON.stringify({ result: r.result, variables: r.variables }, null, 2)
      : r?.status === "completed" && node.type === "action"
        ? JSON.stringify(run?.actions.find((a) => a.node_id === node.id) ?? run?.fallback_actions[0] ?? null, null, 2)
        : r?.error ?? "not reached";
  return (
    <Section
      label={replay ? "Replayed run" : run?.mode === "sample" ? "Last sample run" : "Recorded run"}
      aside={<span className="mono sf-note">{r?.duration_ms !== null && r?.duration_ms !== undefined ? `${r.duration_ms} ms` : ""}{batch?.usage ? ` · ${batch.usage.input_tokens} in / ${batch.usage.output_tokens} out${batch.node_ids.length > 1 ? ` (shared by ${batch.node_ids.length})` : ""}` : ""}</span>}
    >
      {replayed && <div className={`sf-replay-tag ${replayed.status}`}>{REPLAY_LABEL[replayed.status]}</div>}
      <pre className="sf-pre mono">{raw}</pre>
    </Section>
  );
}

const REPLAY_LABEL = {
  comparable: "comparable · recorded answer reused",
  question_changed: "question changed · cannot reuse the answer",
  missing_capture: "no recorded answer for this decision",
  not_reached: "not reached in the replay",
} as const;

// ---------- trigger card: event, filter, fallback ----------

function TriggerFields({ def, catalogue, edit, payload, diagnostics }: { def: Definition; catalogue: Catalogue | undefined; edit: Edit; payload: Record<string, unknown> | undefined; diagnostics: Diagnostic[] }) {
  const expression = useRef<HTMLInputElement>(null);
  const flowEdit = (f: (flow: Definition["flow"]) => Definition["flow"]) => edit?.((d) => ({ ...d, flow: f(d.flow) }));
  const disabled = !edit;
  const whenGroups = useMemo(() => inputGroups(def, catalogue, null), [def, catalogue]);
  const whenChosen = new Set(Object.values(def.flow.inputs));
  const firstField = catalogue?.triggers.find((t) => t.name === def.flow.on)?.payload_fields[0];
  const example = firstField ? `${Object.entries(def.flow.inputs).find(([, ref]) => ref === `event.${firstField.name}`)?.[0] ?? aliasFor(`event.${firstField.name}`, def.flow.inputs)} == ${firstField.value_type === "string" ? '"test"' : firstField.value_type === "boolean" ? "true" : firstField.value_type === "number" || firstField.value_type === "integer" ? "1" : "null"}` : "Always run";
  const whenErrors = diagnostics.filter((d) => d.path.startsWith("flow.when") || d.path.startsWith("flow.inputs"));
  return (
    <>
      <Section label="Event">
        <div className="sf-events">
          {(catalogue?.triggers ?? []).map((t) => (
            <button
              key={t.name}
              type="button"
              className="sf-event"
              aria-pressed={t.name === def.flow.on}
              disabled={disabled}
              onClick={() => flowEdit((f) => ({ ...f, on: t.name, blocking: f.blocking && t.can_block, budget_ms: f.blocking && t.can_block ? f.budget_ms : null }))}
            >
              <span className="mono">{t.name}</span>
              <span className={t.can_block ? "sf-can-block" : "muted"}>{t.can_block ? "can block" : "async only"}</span>
            </button>
          ))}
        </div>
        <div className="sf-note">Changing the event keeps the nodes; inputs and actions that do not fit it show as errors.</div>
      </Section>
      <Section label="Filter · when">
        <input ref={expression} aria-label="Filter expression" className="sf-input mono sf-expr" value={def.flow.when ?? ""} disabled={disabled} placeholder={example} onChange={(e) => flowEdit((f) => ({ ...f, when: e.target.value.trim() ? e.target.value : null }))} />
        <div className="sf-note">Click a chip to declare its input and insert its name. Empty means every event runs.</div>
        <div className="sf-note mono">{'== != < > && || ! · strings in "double quotes" · is_set(x)'}</div>
        {whenErrors.map((d, i) => <div key={i} className="sf-field-err" role="alert">{d.message}{/unknown.*name/i.test(d.message) && " — click its chip to declare it"}</div>)}
        <div className="sf-chips" style={{ marginTop: 6 }}>
          {whenGroups.flatMap((g) => g.options).filter((o) => o.reference.startsWith("event.")).map((o) => (
            <button key={o.reference} type="button" className="sf-chip mono" aria-pressed={whenChosen.has(o.reference)} disabled={disabled} onMouseDown={(e) => e.preventDefault()} onClick={() => {
              const field = expression.current;
              const text = def.flow.when ?? "";
              const next = insertInputAlias(def.flow.inputs, text, o.reference, field?.selectionStart ?? text.length, field?.selectionEnd ?? text.length);
              flowEdit((f) => ({ ...f, inputs: next.inputs, when: next.expression }));
              requestAnimationFrame(() => { field?.focus(); field?.setSelectionRange(next.cursor, next.cursor); });
            }}>
              {o.reference.slice(6)}
            </button>
          ))}
        </div>
      </Section>
      <FallbackFields def={def} catalogue={catalogue} edit={edit} />
      <Section label="Sample event">
        <pre className="sf-pre mono">{JSON.stringify(payload ?? {}, null, 2)}</pre>
        <div className="sf-note">Edit it in the run bar below.</div>
      </Section>
    </>
  );
}

const FALLBACK_LABEL: Record<FallbackKey, string> = {
  on_failure: "On failure (any)",
  on_timeout: "On timeout",
  on_provider_error: "On provider error",
  on_invalid_answer: "On invalid answer",
};

function FallbackFields({ def, catalogue, edit }: { def: Definition; catalogue: Catalogue | undefined; edit: Edit }) {
  const fb = def.fallback;
  const actions = actionsFor(def, catalogue);
  const trigger = catalogue?.triggers.find((t) => t.name === def.flow.on);
  const set = (key: FallbackKey, value: FallbackAction | null) =>
    edit?.((d) => {
      const base = d.fallback ?? { inputs: {}, on_failure: null, on_timeout: null, on_provider_error: null, on_invalid_answer: null };
      const next = { ...base, [key]: value };
      const emptyFb = FALLBACK_KEYS.every((k) => !next[k]) && !Object.keys(next.inputs).length;
      return { ...d, fallback: emptyFb ? null : next };
    });
  const defaultText = def.flow.blocking
    ? trigger?.default_fallback
      ? `Without one: ${actionLook(trigger.default_fallback.action).label.toLowerCase()}.`
      : "Without one: nothing; the event goes on after the budget."
    : "Without one: nothing (asynchronous flow).";
  return (
    <Section label="Fallback · when the run fails">
      <div className="sf-note" style={{ marginBottom: 6 }}>All actions of a failed run are discarded and the fallback applies instead. {defaultText}</div>
      {FALLBACK_KEYS.map((k) => {
        const a = fb?.[k] ?? null;
        const spec = catalogue?.actions.find((x) => x.name === a?.action);
        return (
          <div key={k} className="sf-fallback">
            <div className="sf-row">
              <span className="sf-fb-key">{FALLBACK_LABEL[k]}</span>
              <select
                className="sf-select"
                value={a?.action ?? ""}
                disabled={!edit}
                onChange={(e) => {
                  const name = e.target.value;
                  const s = catalogue?.actions.find((x) => x.name === name);
                   set(k, name ? { action: name, params: Object.fromEntries((s?.params ?? []).filter((p) => p.required || a?.params[p.name] !== undefined).map((p) => [p.name, a?.params[p.name] ?? (p.value_type === "boolean" ? false : "")])), then: null } : null);
                }}
              >
                <option value="">{k === "on_failure" ? "default" : "same as on failure"}</option>
                {actions.map((x) => (
                  <option key={x.name} value={x.name} disabled={!!x.why}>
                    {x.name}
                    {x.why ? ` · ${x.why}` : ""}
                  </option>
                ))}
              </select>
            </div>
            {a &&
              spec?.params.map((p) => (
                 p.value_type === "boolean" ? <label key={p.name}><input type="checkbox" checked={a.params[p.name] === true} disabled={!edit} onChange={(e) => set(k, { ...a, params: { ...a.params, [p.name]: e.target.checked } })} />{p.name}</label> : <input key={p.name} className="sf-input" value={String(a.params[p.name] ?? "")} disabled={!edit} placeholder={`${p.name} · may use {failure_message}`} onChange={(e) => set(k, { ...a, params: { ...a.params, [p.name]: e.target.value } })} />
              ))}
          </div>
        );
      })}
    </Section>
  );
}
