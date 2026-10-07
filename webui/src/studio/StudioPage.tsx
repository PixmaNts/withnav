// The InstinctFlow Studio (#/studio?ws=…&flow=…&loc=…). Three columns under a top bar: flows ·
// canvas or TOML · inspector, with the run bar under the canvas. Imports nothing from the chat
// pages; only the shared HTTP helper, theme and router.

import "@xyflow/react/dist/style.css";
import "./studio.css";

import { ArrowClockwise, ArrowLeft, CaretDown, FloppyDisk, FlowArrow, Lightning, ShieldWarning, WarningCircle, X } from "@phosphor-icons/react";
import { ReactFlowProvider } from "@xyflow/react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type { Route } from "../router";
import { envelopeOf, errorMessage, studioApi, type StudioApi } from "./api";
import { Canvas } from "./Canvas";
import { FlowList, LOCATION_LABEL } from "./FlowList";
import { addNode, completeLayout, FLOW_NAME_PATTERN, newNode, TRIGGER_ID } from "./graph";
import { actionsFor, Inspector } from "./Inspector";
import { actionLook, TYPE_LOOK } from "./meta";
import { RunBar } from "./RunBar";
import type { Definition, Diagnostic, FlowDocument, FlowSummary, Location, NodeType, ReplayResult, RunRecord } from "./types";
import { setEnabledInToml, useFlowEditor } from "./useFlowEditor";

interface Props {
  route: Route;
  navigate: (next: Partial<Route>, replace?: boolean) => void;
}

const isLocation = (v: string | null | undefined): v is Location => v === "workspace" || v === "global" || v === "builtin";

export function StudioPage({ route, navigate }: Props) {
  const workspace = route.workspace;
  useEffect(() => {
    document.title = route.flow ? `${route.flow} · Studio · Navai` : "Studio · Navai";
  }, [route.flow]);

  const apiQ = useQuery({ queryKey: ["studio-api", workspace, !!route.mock], queryFn: () => studioApi(workspace!, !!route.mock), enabled: !!workspace, staleTime: Infinity, retry: false });

  return (
    <div className="sf-shell">
      <header className="topbar sf-top">
        <button type="button" className="sf-back" onClick={() => navigate({ page: "chat", flow: null, loc: null, mock: false })} title="Back to the sessions">
          <ArrowLeft size={13} aria-hidden />
        </button>
        <span className="brand">
          <FlowArrow size={16} aria-hidden />
          Studio
        </span>
        <span className="crumb" title={workspace ?? undefined}>
          {workspace ? workspace.split("/").filter(Boolean).pop() : "Navai"} / InstinctFlow
        </span>
        <div className="topbar-right">
          {apiQ.data?.mocked && (
            <span className="sf-mock" title="The server has no flows routes yet: the Studio uses an in-memory stand-in. Nothing is written to disk and sample answers are invented.">
              mock API · nothing is saved to disk
            </span>
          )}
          <span className="muted sf-top-instinct">
            <Lightning size={12} aria-hidden /> instinct · System One API
          </span>
        </div>
      </header>
      {!workspace ? (
        <Center title="Choose a workspace first">Flows are read per workspace. Open the Studio from a workspace in the chat page.</Center>
      ) : apiQ.isError ? (
        <Center title="The flows API is not available">{errorMessage(apiQ.error)}</Center>
      ) : !apiQ.data ? (
        <Center title="">Loading the flows…</Center>
      ) : (
        <ReactFlowProvider>
          <Studio api={apiQ.data} route={route} navigate={navigate} />
        </ReactFlowProvider>
      )}
    </div>
  );
}

function Center({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div className="sf-center">
      {title && <b>{title}</b>}
      <div className="muted">{children}</div>
    </div>
  );
}

function Studio({ api, route, navigate }: { api: StudioApi; route: Route } & Pick<Props, "navigate">) {
  const qc = useQueryClient();
  const ws = route.workspace!;
  const k = (...parts: unknown[]) => ["studio", ws, api.mocked, ...parts];

  const list = useQuery({ queryKey: k("list"), queryFn: () => api.list().then((b) => b.flows) });
  const catalogue = useQuery({ queryKey: k("catalogue"), queryFn: () => api.catalogue(), staleTime: Infinity });
  const health = useQuery({ queryKey: k("health"), queryFn: () => api.health(), refetchInterval: 60_000 });
  const trust = useQuery({ queryKey: k("trust"), queryFn: () => api.trust() });

  // The open flow: the route's, else the first one listed.
  const flows = list.data ?? [];
  const pick = flows.find((f) => f.name === route.flow && (!isLocation(route.loc) || f.location === route.loc)) ?? (route.flow ? undefined : flows[0]);
  const current = pick ? { name: pick.name, location: pick.location } : null;
  useEffect(() => {
    if (pick && (pick.name !== route.flow || pick.location !== route.loc)) navigate({ flow: pick.name, loc: pick.location }, true);
  }, [pick, route.flow, route.loc, navigate]);

  const docQ = useQuery({ queryKey: k("doc", current?.location, current?.name), queryFn: () => api.get(current!.name, current!.location), enabled: !!current });
  const editor = useFlowEditor(api, docQ.data);
  const { state } = editor;
  const doc = state.doc;
  const def = state.def;
  const runsQ = useQuery({ queryKey: k("runs", current?.name), queryFn: () => api.runs(current!.name).then((b) => b.runs), enabled: !!current });

  const [selected, setSelected] = useState(TRIGGER_ID);
  const [view, setView] = useState<"canvas" | "toml">("canvas");
  const [notice, setNotice] = useState<string | null>(null);
  const [run, setRun] = useState<RunRecord | null>(null);
  const [replay, setReplay] = useState<ReplayResult | null>(null);
  const [busy, setBusy] = useState<"sample" | "replay" | null>(null);
  const [runError, setRunError] = useState<string | null>(null);
  const [reload, setReload] = useState<{ busy: boolean; ok: boolean | null; message: string; errors: Diagnostic[] }>({ busy: false, ok: null, message: "", errors: [] });
  const [creating, setCreating] = useState(false);
  const [toggling, setToggling] = useState<string | null>(null);

  // A flow switch clears the selection and the run.
  const openKey = current ? `${current.location}:${current.name}` : "";
  useEffect(() => {
    setSelected(TRIGGER_ID);
    setRun(null);
    setReplay(null);
    setRunError(null);
  }, [openKey]);
  useEffect(() => {
    if (def && selected !== TRIGGER_ID && !def.node.some((n) => n.id === selected)) setSelected(TRIGGER_ID);
  }, [def, selected]);

  // Sample payload: the trigger's sample until the driver edits it.
  const trigger = catalogue.data?.triggers.find((t) => t.name === def?.flow.on);
  const [payloadText, setPayloadText] = useState("");
  const payloadFor = useRef<string>("");
  useEffect(() => {
    if (!trigger || payloadFor.current === `${openKey}:${trigger.name}`) return;
    payloadFor.current = `${openKey}:${trigger.name}`;
    setPayloadText(JSON.stringify(trigger.sample_payload, null, 2));
  }, [trigger, openKey]);
  const payload = useMemo(() => {
    try {
      const v = JSON.parse(payloadText) as unknown;
      return typeof v === "object" && v && !Array.isArray(v) ? { value: v as Record<string, unknown>, error: null } : { value: undefined, error: "The payload must be a JSON object." };
    } catch (e) {
      return { value: undefined, error: `Not valid JSON: ${errorMessage(e)}` };
    }
  }, [payloadText]);

  const readOnly = !!doc?.read_only;
  const edit = readOnly || state.tomlPending ? null : editor.editGraph;
  const dirty = state.origin !== null;
  const untrusted = trust.data && trust.data.has_workspace_flows && !trust.data.trusted;
  const blockedReason = doc?.location === "workspace" && trust.data && !trust.data.trusted ? "Trust this workspace's flows to run a sample or a replay." : null;

  const refreshList = useCallback(() => qc.invalidateQueries({ queryKey: ["studio", ws, api.mocked] }), [qc, ws, api.mocked]);

  const onSave = async () => {
    const saved = await editor.save();
    if (saved) {
      qc.setQueryData(k("doc", saved.location, saved.name), saved);
      void qc.invalidateQueries({ queryKey: k("list") });
      setNotice(state.origin === "graph" ? "Saved. The file was rewritten from the canvas (comments are not kept). Reload to make it run." : "Saved. Reload to make it run.");
    }
  };

  const onReopen = async () => {
    if (!current) return;
    const fresh = await api.get(current.name, current.location);
    qc.setQueryData(k("doc", current.location, current.name), fresh);
    editor.adopt(fresh);
  };

  const onReload = async () => {
    setReload({ busy: true, ok: null, message: "", errors: [] });
    try {
      const r = await api.reload();
      setReload({ busy: false, ok: true, message: `Reloaded ${r.flows.length} flow${r.flows.length === 1 ? "" : "s"}. Saved flows run from now on.`, errors: [] });
    } catch (e) {
      const env = envelopeOf(e);
      setReload({ busy: false, ok: false, message: env?.error ?? errorMessage(e), errors: env?.errors ?? [] });
    }
    void refreshList();
  };

  const onToggle = async (f: FlowSummary) => {
    const key = `${f.location}:${f.name}`;
    setToggling(key);
    try {
      const d = await api.get(f.name, f.location);
      const saved = await api.save(f.name, { location: f.location, revision: d.revision, content: { format: "toml", toml: setEnabledInToml(d.toml, !f.enabled), templates: d.templates } });
      qc.setQueryData(k("doc", f.location, f.name), saved);
      if (current?.name === f.name && current.location === f.location && !dirty) editor.adopt(saved);
      setNotice(`${f.name} ${f.enabled ? "disabled" : "enabled"} in its file. Reload to apply.`);
    } catch (e) {
      setNotice(envelopeOf(e)?.error ?? errorMessage(e));
    } finally {
      setToggling(null);
      void qc.invalidateQueries({ queryKey: k("list") });
    }
  };

  const onSample = async () => {
    const source = editor.source();
    if (!source || !payload.value) return;
    setBusy("sample");
    setRunError(null);
    setReplay(null);
    try {
      setRun(await api.sample({ source, payload: payload.value, session_id: null }));
      void qc.invalidateQueries({ queryKey: k("runs", current?.name) });
    } catch (e) {
      const env = envelopeOf(e);
      setRunError(env ? `${env.error}${env.errors.length ? ` ${env.errors.map((d) => d.message).join(" ")}` : ""}` : errorMessage(e));
    } finally {
      setBusy(null);
    }
  };

  const onReplay = async (runId: string) => {
    const source = editor.source();
    if (!source) return;
    setBusy("replay");
    setRunError(null);
    try {
      const r = await api.replay({ source, run_id: runId });
      setReplay(r);
      setRun(r.outcome ?? (await api.run(current!.name, runId)));
    } catch (e) {
      setRunError(envelopeOf(e)?.error ?? errorMessage(e));
    } finally {
      setBusy(null);
    }
  };

  const onAdd = (type: NodeType, action?: string) => {
    if (!def || !edit) return;
    const node = newNode(def, type, catalogue.data, action);
    const layout = completeLayout(def);
    const rightmost = Math.max(...Object.values(layout).map((p) => p.x), 0);
    edit((d) => addNode(d, node, { x: rightmost + 240, y: 40 + (d.node.length % 5) * 60 }));
    setSelected(node.id);
  };

  const onCreated = (created: FlowDocument) => {
    setCreating(false);
    void refreshList();
    navigate({ flow: created.name, loc: created.location });
    setNotice(`Created ${created.path}. Reload to make it run.`);
  };

  return (
    <>
      {untrusted && (
        <div className="sf-trust" role="status">
          <ShieldWarning size={15} aria-hidden />
          <span>
            This workspace has its own flows. They are not loaded, sampled or replayed until you trust them. Trust lets them read the context they declare; it grants no tool permission.
          </span>
          <button type="button" className="sf-btn accent" onClick={async () => { await api.setTrust(true); void refreshList(); }}>
            Trust this workspace's flows
          </button>
        </div>
      )}
      <div className="sf-grid">
        <FlowList
          flows={flows}
          health={health.data?.flows ?? []}
          current={current}
          onOpen={(f) => {
            if (dirty && !confirm("Leave this flow? Unsaved changes are lost.")) return;
            navigate({ flow: f.name, loc: f.location });
          }}
          onToggle={onToggle}
          toggling={toggling}
          onNew={() => setCreating(true)}
          def={def}
          editConstants={edit ? (f) => edit((d) => ({ ...d, constants: f(d.constants) })) : null}
        />

        <main className="sf-main">
          {!doc ? (
            <Center title={list.isPending || docQ.isPending ? "" : flows.length ? "Flow not found" : "No flow yet"}>
              {list.isPending || docQ.isPending ? "Loading…" : flows.length ? `No ${route.loc ?? ""} flow called ${route.flow}.` : "Create a flow with New flow on the left."}
            </Center>
          ) : (
            <>
              <FlowHeader doc={doc} def={def} edit={edit} dirty={dirty} state={state} canBlock={!!trigger?.can_block} cap={catalogue.data?.limits.blocking_budget_ms ?? 2000} health={health.data?.flows.find((h) => h.name === doc.name)} onSave={onSave} onDiscard={editor.discard} onReload={onReload} reloading={reload.busy} />
              {state.saveError && (
                <Banner tone="bad" onClose={undefined}>
                  {state.saveError.message}
                  {state.saveError.reason === "stale" && (
                    <button type="button" className="sf-link" onClick={onReopen}>
                      Reopen the file (your changes are lost)
                    </button>
                  )}
                  <DiagList errors={state.saveError.errors} />
                </Banner>
              )}
              {(reload.ok !== null || notice) && (
                <Banner tone={reload.ok === false ? "bad" : "ok"} onClose={() => { setReload((r) => ({ ...r, ok: null })); setNotice(null); }}>
                  {reload.ok !== null ? reload.message : notice}
                  {reload.ok === false && <DiagList errors={reload.errors} />}
                </Banner>
              )}
              <Toolbar view={view} setView={setView} onAdd={onAdd} disabled={!edit} def={def} catalogue={catalogue.data} tomlPending={state.tomlPending} readOnly={readOnly} />
              <div className="sf-stage">
                {view === "canvas" ? (
                  def ? (
                    <>
                      {state.tomlPending && (
                        <div className="sf-locked">
                          <WarningCircle size={13} aria-hidden /> The TOML text has changes {state.validating ? "being checked" : "that do not parse"}. The canvas shows the last valid version and is locked until the text parses.
                          <button type="button" className="sf-link" onClick={() => setView("toml")}>Open TOML</button>
                        </div>
                      )}
                      <Canvas key={openKey} def={def} run={run} diagnostics={state.diagnostics} selected={selected} onSelect={setSelected} edit={edit} onRefused={setNotice} />
                    </>
                  ) : (
                    <Center title="This file does not parse">Open the TOML view to fix it.</Center>
                  )
                ) : (
                  <TomlView toml={state.toml} errors={state.tomlErrors} readOnly={readOnly} onChange={editor.editToml} />
                )}
              </div>
              {def && (
                <RunBar
                  def={def}
                  payload={payloadText}
                  setPayload={setPayloadText}
                  payloadError={payload.error}
                  run={run}
                  replay={replay}
                  busy={busy}
                  error={runError}
                  runs={runsQ.data ?? []}
                  blockedReason={blockedReason ?? (state.tomlPending ? "Fix the TOML text first." : null)}
                  onSample={onSample}
                  onReplay={onReplay}
                  onClear={() => { setRun(null); setReplay(null); setRunError(null); }}
                />
              )}
            </>
          )}
        </main>

        {def ? (
          <Inspector def={def} catalogue={catalogue.data} selected={selected} onSelect={setSelected} edit={edit} run={run} replay={replay} diagnostics={state.diagnostics} payload={payload.value} />
        ) : (
          <aside className="sf-inspector" />
        )}
      </div>
      {creating && <NewFlowDialog api={api} flows={flows} catalogue={catalogue.data} onClose={() => setCreating(false)} onCreated={onCreated} />}
    </>
  );
}

function Banner({ tone, onClose, children }: { tone: "ok" | "bad"; onClose?: () => void; children: ReactNode }) {
  return (
    <div className={`sf-banner ${tone}`} role={tone === "bad" ? "alert" : "status"}>
      <div className="sf-banner-body">{children}</div>
      {onClose && (
        <button type="button" className="icon-btn" aria-label="Dismiss" onClick={onClose}>
          <X size={12} />
        </button>
      )}
    </div>
  );
}

function DiagList({ errors }: { errors: Diagnostic[] }) {
  if (!errors.length) return null;
  return (
    <ul className="sf-diags">
      {errors.map((e, i) => (
        <li key={i}>
          <span className="mono sf-diag-code">{e.code}</span> {e.message}
          {e.path && <span className="mono muted"> · {e.path}{e.line ? `:${e.line}` : ""}</span>}
        </li>
      ))}
    </ul>
  );
}

function FlowHeader({ doc, def, edit, dirty, state, canBlock, cap, health, onSave, onDiscard, onReload, reloading }: {
  doc: FlowDocument;
  def: Definition | null;
  edit: ((f: (d: Definition) => Definition) => void) | null;
  dirty: boolean;
  state: ReturnType<typeof useFlowEditor>["state"];
  canBlock: boolean;
  cap: number;
  health: { runs: number; failures: number; fallbacks_applied: number; cost_usd: number | null } | undefined;
  onSave: () => void;
  onDiscard: () => void;
  onReload: () => void;
  reloading: boolean;
}) {
  const f = def?.flow;
  const blocking = !!f?.blocking;
  const errors = state.origin === "toml" ? state.tomlErrors : state.diagnostics;
  const graphInvalid = state.origin === "graph" && errors.length > 0;
  const setFlow = (g: (x: Definition["flow"]) => Definition["flow"]) => edit?.((d) => ({ ...d, flow: g(d.flow) }));
  return (
    <div className="sf-head">
      <div className="sf-head-id">
        <div className="sf-head-title">
          <span className="mono sf-head-name">{doc.name}</span>
          {f && <span className="sf-pill">on {f.on}</span>}
          <span className={`sf-loc ${doc.location}`}>{LOCATION_LABEL[doc.location]}</span>
          {doc.read_only && <span className="sf-pill">read-only</span>}
          {doc.saved_since_reload && !dirty && <span className="sf-unloaded" title="What runs is the version of the last reload.">saved · not reloaded</span>}
          {dirty && <span className="sf-dirty">unsaved</span>}
        </div>
        <div className="sf-head-sub">
          <span className="mono sf-head-path" title={doc.path}>{doc.path}</span>
          {health && (
            <span className="sf-health" title="Last 24 hours, live runs only. Fallbacks applied is the main sign of a badly tuned flow.">
              <span><b>{health.runs}</b> runs</span>
              <span className={health.failures ? "warn" : ""}><b>{health.failures}</b> failed</span>
              <span className={health.fallbacks_applied ? "warn" : ""}><b>{health.fallbacks_applied}</b> fallbacks applied</span>
              <span>{health.cost_usd === null ? "cost unknown" : `$${health.cost_usd.toFixed(4)}`}</span>
              <span className="muted">· 24 h</span>
            </span>
          )}
        </div>
      </div>
      {f && (
        <div className="sf-head-ctl">
          <label className={`sf-switch-label${!canBlock ? " off" : ""}`} title={canBlock ? "A blocking flow holds the event until it answers, within its budget." : `${f.on} cannot block.`}>
            <button
              type="button"
              role="switch"
              aria-checked={blocking}
              aria-label="Blocking"
              className="sf-toggle"
              disabled={!edit || (!canBlock && !blocking)}
              onClick={() => setFlow((x) => ({ ...x, blocking: !x.blocking, budget_ms: !x.blocking ? x.budget_ms ?? 800 : null }))}
            >
              <span />
            </button>
            Blocking
          </label>
          <label className="sf-budget-field" style={{ opacity: blocking ? 1 : 0.4 }}>
            <span className="muted">budget</span>
            <input
              className="sf-input mono"
              type="number"
              min={1}
              max={cap}
              step={50}
              value={f.budget_ms ?? ""}
              disabled={!edit || !blocking}
              onChange={(e) => setFlow((x) => ({ ...x, budget_ms: e.target.value === "" ? null : Math.round(Number(e.target.value)) }))}
              aria-label="Budget in milliseconds"
            />
            <span className="muted">ms</span>
          </label>
        </div>
      )}
      <div className="sf-head-actions">
        {state.validating && <span className="muted sf-checking">checking…</span>}
        {!state.validating && errors.length > 0 && (
          <span className="sf-err-count" title={errors.map((e) => e.message).join("\n")}>
            <WarningCircle size={13} aria-hidden /> {errors.length} error{errors.length === 1 ? "" : "s"}
          </span>
        )}
        {dirty && (
          <button type="button" className="sf-btn ghost" onClick={onDiscard}>
            Discard
          </button>
        )}
        <button
          type="button"
          className="sf-btn accent"
          onClick={onSave}
          disabled={!dirty || state.saving || doc.read_only || graphInvalid}
          title={doc.read_only ? "Built-in flows cannot be edited." : graphInvalid ? "An invalid canvas cannot be saved. Fix the errors, or edit the TOML text (invalid text can be saved)." : "Writes the file. Changes nothing that runs until a reload."}
        >
          <FloppyDisk size={13} aria-hidden /> {state.saving ? "Saving…" : "Save"}
        </button>
        <button type="button" className="sf-btn" onClick={onReload} disabled={reloading} title="Reads every flow file again. If any file is invalid, nothing changes and the errors are listed.">
          <ArrowClockwise size={13} aria-hidden className={reloading ? "spin" : undefined} /> {reloading ? "Reloading…" : "Reload"}
        </button>
      </div>
    </div>
  );
}

function Toolbar({ view, setView, onAdd, disabled, def, catalogue, tomlPending, readOnly }: {
  view: "canvas" | "toml";
  setView: (v: "canvas" | "toml") => void;
  onAdd: (type: NodeType, action?: string) => void;
  disabled: boolean;
  def: Definition | null;
  catalogue: import("./types").Catalogue | undefined;
  tomlPending: boolean;
  readOnly: boolean;
}) {
  const [menu, setMenu] = useState<null | "decision" | "action">(null);
  useEffect(() => {
    if (!menu) return;
    const close = (e: MouseEvent) => !(e.target as HTMLElement).closest(".sf-menu-wrap") && setMenu(null);
    window.addEventListener("mousedown", close);
    return () => window.removeEventListener("mousedown", close);
  }, [menu]);
  const actions = def ? actionsFor(def, catalogue) : [];
  const off = disabled || view !== "canvas";
  const why = readOnly ? "Built-in flows are read-only." : tomlPending ? "Fix the TOML text first." : view !== "canvas" ? "Switch to the canvas to add nodes." : undefined;
  return (
    <div className="sf-toolbar">
      <div className="sf-menu-wrap">
        <button type="button" className="sf-tool" aria-expanded={menu === "decision"} disabled={off} title={why} onClick={() => setMenu(menu === "decision" ? null : "decision")}>
          + Decision <CaretDown size={10} aria-hidden />
        </button>
        {menu === "decision" && (
          <div className="sf-menu" role="menu">
            {(["noul", "choice", "score"] as const).map((t) => (
              <button key={t} type="button" role="menuitem" className="sf-menu-item" onClick={() => { onAdd(t); setMenu(null); }}>
                <span style={{ color: TYPE_LOOK[t].color }}>{TYPE_LOOK[t].icon(14)}</span>
                <span>
                  <span className="mono">{t}</span>
                  <span className="sf-menu-desc">{TYPE_LOOK[t].describe}</span>
                </span>
              </button>
            ))}
          </div>
        )}
      </div>
      <button type="button" className="sf-tool" disabled={off} title={why ?? TYPE_LOOK.expression.describe} onClick={() => onAdd("expression")}>
        + Expression
      </button>
      <div className="sf-menu-wrap">
        <button type="button" className="sf-tool" aria-expanded={menu === "action"} disabled={off} title={why} onClick={() => setMenu(menu === "action" ? null : "action")}>
          + Action <CaretDown size={10} aria-hidden />
        </button>
        {menu === "action" && (
          <div className="sf-menu" role="menu">
            {actions.map((a) => {
              const look = actionLook(a.name);
              return (
                <button key={a.name} type="button" role="menuitem" className="sf-menu-item" disabled={!!a.why} title={a.why ?? undefined} onClick={() => { onAdd("action", a.name); setMenu(null); }}>
                  <span style={{ color: look.color }}>{look.icon(14)}</span>
                  <span>
                    <span>{look.label} <span className="mono muted">{a.name}</span></span>
                    <span className="sf-menu-desc">{a.why ? `Not available: ${a.why}.` : look.describe}</span>
                  </span>
                </button>
              );
            })}
          </div>
        )}
      </div>
      <div className="sf-views" role="tablist" aria-label="View">
        {(["canvas", "toml"] as const).map((v) => (
          <button key={v} type="button" role="tab" aria-selected={view === v} className="sf-view" onClick={() => setView(v)}>
            {v === "canvas" ? "Canvas" : "TOML"}
          </button>
        ))}
      </div>
    </div>
  );
}

function TomlView({ toml, errors, readOnly, onChange }: { toml: string; errors: Diagnostic[]; readOnly: boolean; onChange: (t: string) => void }) {
  const lines = toml.split("\n").length;
  const bad = new Set(errors.map((e) => e.line).filter((l): l is number => !!l));
  const gutter = useRef<HTMLDivElement>(null);
  return (
    <div className="sf-toml">
      <div className="sf-toml-edit">
        <div className="sf-gutter mono" ref={gutter} aria-hidden>
          {Array.from({ length: lines }, (_, i) => (
            <div key={i} className={bad.has(i + 1) ? "bad" : undefined}>{i + 1}</div>
          ))}
        </div>
        <textarea
          className="mono"
          value={toml}
          readOnly={readOnly}
          spellCheck={false}
          aria-label="Flow TOML"
          onScroll={(e) => gutter.current && (gutter.current.scrollTop = e.currentTarget.scrollTop)}
          onChange={(e) => onChange(e.target.value)}
        />
      </div>
      <div className="sf-toml-foot">
        {errors.length ? <DiagList errors={errors} /> : <span className="muted">Valid. Text saves keep your comments; canvas saves rewrite the file.</span>}
      </div>
    </div>
  );
}

function NewFlowDialog({ api, flows, catalogue, onClose, onCreated }: { api: StudioApi; flows: FlowSummary[]; catalogue: import("./types").Catalogue | undefined; onClose: () => void; onCreated: (d: FlowDocument) => void }) {
  const [name, setName] = useState("");
  const [location, setLocation] = useState<"workspace" | "global">("workspace");
  const [on, setOn] = useState(catalogue?.triggers.find((t) => t.name === "tool.pre_call")?.name ?? catalogue?.triggers[0]?.name ?? "");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const taken = flows.some((f) => f.name === name && f.location === location);
  const problem = !name ? null : !FLOW_NAME_PATTERN.test(name) ? "Lowercase letters, digits and dashes, starting with a letter." : taken ? `A ${location} flow has this name.` : null;
  const trigger = catalogue?.triggers.find((t) => t.name === on);
  const submit = async () => {
    if (!name || problem || !trigger) return;
    setBusy(true);
    setError(null);
    const first = trigger.allowed_actions.includes("log") ? "log" : trigger.allowed_actions[0] ?? "log";
    const definition: Definition = {
      flow: { version: 1, name, on, enabled: true, blocking: false, budget_ms: null, inputs: {}, when: null, start: [first] },
      constants: {},
      node: [{ id: first, type: "action", label: null, action: first, params: { text: `${name} ran` }, inputs: {}, next: {} }],
      fallback: null,
      layout: { trigger: { x: 16, y: 120 }, [first]: { x: 280, y: 100 } },
    };
    try {
      onCreated(await api.create({ name, location, content: { format: "graph", definition, templates: {} } }));
    } catch (e) {
      setError(envelopeOf(e)?.error ?? errorMessage(e));
      setBusy(false);
    }
  };
  return (
    <div className="sf-backdrop" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <form className="sf-dialog" role="dialog" aria-label="New flow" onSubmit={(e) => { e.preventDefault(); void submit(); }} onKeyDown={(e) => e.key === "Escape" && onClose()}>
        <h2>New flow</h2>
        <label className="sf-field">
          <span className="sf-sec-label">Name · also the file name</span>
          <input className="sf-input mono" autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="guard-mutations" aria-invalid={!!problem} />
          {problem && <span className="sf-field-err">{problem}</span>}
        </label>
        <div className="sf-field">
          <span className="sf-sec-label">Where to save it</span>
          <div className="sf-types">
            {(["workspace", "global"] as const).map((l) => (
              <button key={l} type="button" role="radio" aria-checked={location === l} className="sf-type" onClick={() => setLocation(l)}>
                <span>{l === "workspace" ? "This workspace" : "My configuration"}</span>
                <span className="sf-type-sub mono">{l === "workspace" ? ".navai/flows/" : "~/.config/navai/flows/"}</span>
              </button>
            ))}
          </div>
          <span className="sf-note">{location === "workspace" ? "Under version control; runs once the workspace is trusted. Replaces a global flow of the same name." : "Runs in every workspace."}</span>
        </div>
        <label className="sf-field">
          <span className="sf-sec-label">Event it listens to</span>
          <select className="sf-select" value={on} onChange={(e) => setOn(e.target.value)}>
            {catalogue?.triggers.map((t) => (
              <option key={t.name} value={t.name}>
                {t.name} · {t.can_block ? "can block" : "async only"}
              </option>
            ))}
          </select>
        </label>
        {error && <div className="sf-field-err">{error}</div>}
        <div className="sf-dialog-actions">
          <button type="button" className="sf-btn ghost" onClick={onClose}>Cancel</button>
          <button type="submit" className="sf-btn accent" disabled={!name || !!problem || busy}>{busy ? "Creating…" : "Create"}</button>
        </div>
      </form>
    </div>
  );
}
