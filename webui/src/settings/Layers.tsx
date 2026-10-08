import { Crew, crewLabel } from "../components/Crew";
import { Brain, CaretDown, Compass, Gauge, Lightning, LockSimple, MagnifyingGlass, Star } from "@phosphor-icons/react";
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type { AgentEntry, LayerEntry, ModelEntry } from "../api/types";
import { asChoice, asInt, canLead, carryVariant, LAYER_API, type LayerId, type ModelChoice } from "../lib/settings";
import { Catalogue, type CatalogueHandle } from "./Catalogue";
import { isSet, problem, shown, type SectionProps } from "./context";
import { Applies, Field, FieldError, Head, Pill } from "./ui";

type Target = LayerId | "cognition";

const LAYERS: { id: Target; label: string; icon: ReactNode; api: string; role: string; note: string; modelNote: string }[] = [
  { id: "reflex", label: "Reflex · Completion", icon: <Lightning size={14} />, api: "fim", role: "Ghost-text suggestions in the editor.", note: "LSP only · IDE attached", modelNote: "Only fill-in-the-middle models are offered." },
  { id: "instinct", label: "Instinct · Triage", icon: <Gauge size={14} />, api: "systemone", role: "Typed decisions: InstinctFlow, permission review, pipeline.", note: "System One decisions", modelNote: "Only models behind the System One API are offered." },
  { id: "metacognition", label: "Metacognition · Beacon", icon: <Compass size={14} />, api: "chat", role: "Remarks, wakes, and approval of risky calls.", note: "fast model", modelNote: "A fast (flash) chat model works best here." },
  { id: "cognition", label: "Cognition · Nav and the crew", icon: <Brain size={14} />, api: "chat", role: "Agents, each with its own model.", note: "", modelNote: "" },
];

const fmt = (n: number | null | undefined) => (n ? n.toLocaleString("en-US") : null);

export function Layers(p: SectionProps) {
  const [target, setTarget] = useState<Target>("cognition");
  const visibleLayers = LAYERS.filter((l) => p.visible(`layer:${l.id}`) || (l.id === "cognition" && p.agents.some((a) => p.visible(`agent:${a.id}`))));
  useEffect(() => {
    if (visibleLayers.length && !visibleLayers.some((l) => l.id === target)) setTarget(visibleLayers[0]!.id);
  }, [visibleLayers, target]);
  const picker = useRef<CatalogueHandle>(null);
  const defaultAgent = String(shown(p, "default_agent") ?? "");
  const leads = p.agents.filter(canLead);
  const [agentId, setAgentId] = useState<string | null>(null);
  const agent = p.agents.find((a) => a.id === agentId) ?? p.agents.find((a) => a.id === defaultAgent) ?? p.agents[0];

  const layer = LAYERS.find((l) => l.id === target)!;
  const api = layer.api;
  const pickKey = target === "cognition" ? (agent ? `agents.${agent.id}` : null) : `layers.${target}`;
  const pickChoice = target === "cognition" ? (agent ? agentChoice(p, agent) : null) : asChoice(shown(p, `layers.${target}`));
  const pickable = useMemo(() => p.models.filter((m) => m.api === api), [p.models, api]);

  const assign = (m: ModelEntry) => {
    if (!pickKey) return;
    p.save.save(pickKey, { model: m.name, variant: carryVariant(pickChoice?.variant ?? null, m) }, "model");
  };

  if (!visibleLayers.length)
    return (
      <>
        <LayersHead />
        <div className="st-empty">No layer or agent matches this search.</div>
      </>
    );

  return (
    <>
      <LayersHead />
      <div className="st-cards">
        {LAYERS.map((l) => {
          if (!visibleLayers.includes(l)) return null;
          const entry = p.layers.find((x) => x.layer === l.id);
          const choice = l.id === "cognition" ? null : asChoice(shown(p, `layers.${l.id}`));
          const note =
            l.id === "cognition"
              ? `${p.agents.length} agents`
              : l.id === "metacognition"
                ? `fast model${metaVariant(p, choice, entry) ? ` · variant ${metaVariant(p, choice, entry)}` : ""}`
                : l.note;
          const disabled = entry?.status === "disabled";
          return (
            <button key={l.id} type="button" className="st-layer-card" aria-pressed={l.id === target} onClick={() => setTarget(l.id)}>
              <span className="row">
                <span className="icon">{l.icon}</span>
                <span className="name">{l.label}</span>
                <span className={`mono st-api ${l.api}`}>{l.api}</span>
              </span>
              <span className="mono model">{l.id === "cognition" ? `default agent: ${defaultAgent || "—"}` : choice?.model ?? "not set"}</span>
              <span className={disabled ? "note bad" : "note"} title={disabled ? entry?.reason ?? undefined : undefined}>
                {disabled ? `off · ${entry?.reason ?? "not usable"}` : note}
              </span>
            </button>
          );
        })}
      </div>

      {target === "cognition" ? (
        <Cognition p={p} agent={agent} leads={leads} defaultAgent={defaultAgent} onAgent={setAgentId} onSearch={() => picker.current?.focusFilter()} />
      ) : (
        <FixedLayer p={p} layer={layer} id={target} onSearch={() => picker.current?.focusFilter()} />
      )}

      {pickKey && (
        <div className="st-pick">
          <Catalogue
            ref={picker}
            models={pickable}
            current={pickChoice?.model ?? null}
            onPick={assign}
            busy={p.save.pending(pickKey).has}
            loading={p.modelsLoading}
            label={`Catalogue · ${api} models · choosing for ${target === "cognition" ? `agent ${agent?.id}` : `layer ${target}`}`}
          />
        </div>
      )}
    </>
  );
}

function LayersHead() {
  return (
    <Head title="Layers" file="config.json → layers, agents, default_agent">
      Reflex, instinct and metacognition run one model each. Nav runs agents, several at once as subagents.
    </Head>
  );
}

/** What null means for the metacognition variant: the server's resolved variant, else the model's default. */
function metaVariant(p: SectionProps, choice: ModelChoice | null, entry: LayerEntry | undefined): string | null {
  if (choice?.variant) return choice.variant;
  if (entry?.variant && entry.model === choice?.model) return entry.variant;
  return p.models.find((m) => m.name === choice?.model)?.default_variant ?? null;
}

function FixedLayer({ p, layer, id, onSearch }: { p: SectionProps; layer: (typeof LAYERS)[number]; id: LayerId; onSearch: () => void }) {
  const key = `layers.${id}`;
  const choice = asChoice(shown(p, key));
  const entry = p.layers.find((x) => x.layer === id);
  const model = p.models.find((m) => m.name === choice?.model);
  const error = problem(p, key, "model");
  const variantError = problem(p, key, "variant");
  const defaultVariant = id === "metacognition" ? (entry?.model === choice?.model && !choice?.variant ? entry?.variant : null) ?? model?.default_variant ?? null : null;
  const current = id === "metacognition" ? metaVariant(p, choice, entry) : null;
  // Until the catalogue answers, the model's details are unknown rather than missing.
  const wait = p.modelsLoading && !!choice && !model;
  const loading = <span className="muted">loading…</span>;
  return (
    <div className="st-box narrow">
      <div className="st-box-head">
        <span className="title">{layer.label}</span>
        <span className="muted">{layer.role}</span>
        <span className="right">
          <Applies body={p.body} keyName={key} />
          {entry?.status === "disabled" ? (
            <span className="st-pill bad" title={entry.reason ?? undefined}>
              off · {entry.reason ?? "not usable"}
            </span>
          ) : (
            <span className="st-pill mu">
              <LockSimple size={11} aria-hidden /> one model at a time
            </span>
          )}
        </span>
      </div>
      <div className="st-grid">
        <Field label="Gateway › provider">
          <div className="st-input">
            {model ? (
              <>
                <span className="mono">{model.gateway}</span>
                <span className="muted">›</span>
                <span className="mono">{model.provider_group === model.gateway ? "(direct)" : model.provider_group}</span>
              </>
            ) : wait ? (
              loading
            ) : (
              <span className="muted">—</span>
            )}
            <span className="end mono small">config.json → {key}</span>
          </div>
        </Field>
        <Field label="API">
          <div className="st-input">
            <span className="mono">{LAYER_API[id]}</span>
            <span className="end">
              <LockSimple size={12} aria-hidden /> fixed for this layer
            </span>
          </div>
        </Field>
        <Field label="Model" wide error={error} note={layer.modelNote}>
          <button type="button" className="st-input as-btn mono" onClick={onSearch} title="Choose in the catalogue below">
            {choice?.model ?? <span className="muted">not set · choose one below</span>}
            <MagnifyingGlass size={14} className="end" aria-hidden />
          </button>
        </Field>
        <Field label="Context window">
          <div className="st-input mono">{fmt(model?.context_window) ?? (wait ? loading : <span className="muted">not known</span>)}</div>
        </Field>
        <Field label="Max output tokens">
          <div className="st-input mono">{fmt(model?.reported_output) ?? (wait ? loading : <span className="muted">not reported by the catalogue</span>)}</div>
        </Field>
        {id === "metacognition" && (
          <Field
            label="Variant · offered by this model"
            wide
            error={variantError}
            note={model ? (model.variants?.length ? `Variants offered by ${model.upstream_id.split("/").pop()}.${defaultVariant ? ` ${defaultVariant} is the default.` : ""}` : `${model.name} offers no variant.`) : null}
          >
            <div className="st-pills">
              {(model?.variants ?? []).map((v) => (
                <Pill key={v} mono on={current === v} onClick={() => choice && current !== v && p.save.save(key, { model: choice.model, variant: v }, "variant")}>
                  {v}
                  {v === defaultVariant && <span className="tag">default</span>}
                </Pill>
              ))}
              {!!model?.variants?.length && (
                <Pill mono on={current === "off"} onClick={() => choice && current !== "off" && p.save.save(key, { model: choice.model, variant: "off" }, "variant")} title="Run the model without a variant">
                  off
                </Pill>
              )}
              {!model?.variants?.length && <span className="muted">{wait ? "loading…" : "none offered"}</span>}
            </div>
          </Field>
        )}
        {id === "reflex" && (
          <div className="st-field wide st-plain">The reflex runs only through the language server, while an editor is attached (VS Code today; Zed and IntelliJ are planned).</div>
        )}
        {id === "instinct" && <div className="st-field wide st-plain">Every call has a 2 s deadline, whatever the provider timeout. Used by the permission review, the WithNav pipeline and InstinctFlow.</div>}
      </div>
    </div>
  );
}

/** The model an agent runs: config.json's value, else what the server resolves for it. */
function agentChoice(p: SectionProps, a: AgentEntry): ModelChoice | null {
  return asChoice(shown(p, `agents.${a.id}`)) ?? (a.effective ? { model: a.effective.model, variant: a.effective.variant } : null);
}

const SOURCE: Record<string, string> = {
  config: "config.json",
  header: "its .md file",
  default_agent: "the default agent",
  parent: "the parent session",
  layer_preference: "the layer preference",
  model_default: "the model's default",
};

function Cognition({ p, agent, leads, defaultAgent, onAgent, onSearch }: { p: SectionProps; agent: AgentEntry | undefined; leads: AgentEntry[]; defaultAgent: string; onAgent: (id: string) => void; onSearch: () => void }) {
  const rows = p.agents.filter((a) => p.visible(`agent:${a.id}`) || p.visible("layer:cognition"));
  const defError = problem(p, "default_agent");
  const maxSub = asInt(shown(p, "subagents.max_concurrent"), 4);
  return (
    <div className="st-box">
      <div className="st-box-head">
        <span className="title">Cognition</span>
        <span className="muted">Agents, each with its own model.</span>
        <span className="right">
          <Applies body={p.body} keyName={agent ? `agents.${agent.id}` : "default_agent"} />
          <label className="muted small" htmlFor="st-default-agent">
            Default agent
          </label>
          <span className="st-select">
            <select id="st-default-agent" className="mono" value={defaultAgent} onChange={(e) => p.save.save("default_agent", e.target.value)}>
              {!leads.some((a) => a.id === defaultAgent) && <option value={defaultAgent}>{defaultAgent ? crewLabel(defaultAgent) : "—"}</option>}
              {leads.map((a) => (
                <option key={a.id} value={a.id}>
                  {crewLabel(a.id)}
                </option>
              ))}
            </select>
            <CaretDown size={12} aria-hidden />
          </span>
        </span>
      </div>
      {defError && (
        <div className="st-box-part tight">
          <FieldError message={defError} />
        </div>
      )}
      <div className="st-agents" role="table" aria-label="Agents">
        <div className="st-agents-head" role="row">
          <span>Agent</span>
          <span>Mode</span>
          <span>Model</span>
          <span>Gateway</span>
          <span>Variant</span>
          <span />
        </div>
        {rows.map((a) => {
          const set = isSet(p, `agents.${a.id}`);
          const c = agentChoice(p, a);
          const m = p.models.find((x) => x.name === c?.model);
          const isDefault = a.id === defaultAgent;
          return (
            <div key={a.id} role="row" className="st-agent-row" aria-selected={a.id === agent?.id} onClick={() => onAgent(a.id)}>
              <button type="button" className="mono name" onClick={() => onAgent(a.id)}>
                <Crew id={a.id} />
              </button>
              <span className={`mode ${a.mode}`}>{a.mode}</span>
              <span className={`mono model${set ? "" : " inherited"}`} title={c ? (set ? `config.json → agents.${a.id}` : `from ${SOURCE[a.effective?.source ?? ""] ?? a.effective?.source}`) : undefined}>
                {c?.model ?? (a.inherits_parent ? "inherits the parent's model" : "unassigned")}
              </span>
              <span className="mono muted">{m?.gateway ?? c?.model.split("/")[0] ?? ""}</span>
              <span className="mono muted" title={c?.variant === "off" ? "variant: off in config.json: the model runs without a variant" : c?.variant ? undefined : "No variant in config.json: the model's default applies"}>
                {c?.variant ?? (m?.default_variant ?? (m?.variants?.length ? "default" : "—"))}
              </span>
              {canLead(a) ? (
                <button
                  type="button"
                  className="st-def"
                  aria-pressed={isDefault}
                  onClick={(e) => {
                    e.stopPropagation();
                    onAgent(a.id);
                    if (!isDefault) p.save.save("default_agent", a.id);
                  }}
                >
                  <Star size={11} weight={isDefault ? "fill" : "regular"} aria-hidden /> {isDefault ? "default" : "set default"}
                </button>
              ) : (
                <span />
              )}
            </div>
          );
        })}
        {!rows.length && <div className="st-empty pad">No agent matches this search.</div>}
      </div>
      {agent && <AgentDetail p={p} a={agent} maxSub={maxSub} onSearch={onSearch} />}
    </div>
  );
}

function AgentDetail({ p, a, maxSub, onSearch }: { p: SectionProps; a: AgentEntry; maxSub: number; onSearch: () => void }) {
  const key = `agents.${a.id}`;
  const set = isSet(p, key);
  const c = agentChoice(p, a);
  const m = p.models.find((x) => x.name === c?.model);
  const error = problem(p, key, "model");
  const variantError = problem(p, key, "variant");
  const variant = c?.variant ?? null;
  const dirs = p.body.paths.agents;
  const wait = p.modelsLoading && !!c && !m;
  return (
    <div className="st-grid">
      <div className="st-field wide st-agent-title">
        <span className="mono name"><Crew id={a.id} /></span>
        <span className="muted">{a.description}</span>
        <span className="mono muted small end" title={`Looked up in ${dirs.join(" and ")}; built-in agents have no file.`}>
          {a.id}.md
        </span>
      </div>
      <Field
        label="Model"
        error={error}
        note={
          set ? (
            <>
              Saved in config.json → {key}.model. It wins over a model: line in {a.id}.md.{" "}
              <button type="button" className="link-btn" onClick={() => p.save.save(key, null)}>
                Remove from config.json
              </button>
            </>
          ) : c ? (
            `Not in config.json: comes from ${SOURCE[a.effective?.source ?? ""] ?? a.effective?.source ?? "the server"}. Choosing a model saves it in config.json → ${key}.model.`
          ) : (
            `Not in config.json. Choosing a model saves it in config.json → ${key}.model.`
          )
        }
      >
        <button type="button" className="st-input as-btn mono" onClick={onSearch} title="Choose in the catalogue below">
          <span className={set ? "" : "muted"}>{c?.model ?? "unassigned · choose one below"}</span>
          <MagnifyingGlass size={14} className="end" aria-hidden />
        </button>
      </Field>
      <Field
        label="Variant · offered by this model"
        error={variantError}
        note={m && !m.variants?.length ? `${m.name} offers no variant.` : m?.default_variant ? `${m.default_variant} is the model's default.` : m ? "default: no variant is saved, the model decides." : null}
      >
        <div className="st-pills">
          {(m?.variants ?? []).map((v) => (
            <Pill key={v} mono on={variant === v} onClick={() => c && variant !== v && p.save.save(key, { model: c.model, variant: v }, "variant")}>
              {v}
              {v === m?.default_variant && <span className="tag">default</span>}
            </Pill>
          ))}
          {!!m?.variants?.length && (
            <Pill mono on={variant === "off"} onClick={() => c && variant !== "off" && p.save.save(key, { model: c.model, variant: "off" }, "variant")} title="Run the model without a variant">
              off
            </Pill>
          )}
          {!!m?.variants?.length && (
            <Pill on={variant === null} onClick={() => c && variant !== null && p.save.save(key, { model: c.model, variant: null }, "variant")} title="Remove the variant from config.json">
              default
            </Pill>
          )}
          {!m?.variants?.length && <span className="muted">{wait ? "loading…" : "none offered"}</span>}
        </div>
      </Field>
      <Field label="Mode" note={<>primary drives a conversation, subagent runs as a background flow, all does both. Set in {a.id}.md, not here.</>}>
        <div className="st-pills">
          {["primary", "subagent", "all"].map((mode) => (
            <span key={mode} className="st-pillbtn static" aria-current={a.mode === mode || undefined} title={`Set in ${a.id}.md`}>
              {mode}
            </span>
          ))}
        </div>
      </Field>
      <Field label="Simultaneous flows">
        <div className="st-input plain">
          Up to&nbsp;<span className="mono or">{maxSub}</span>&nbsp;subagents per session ·&nbsp;
          <button type="button" className="st-link" onClick={() => p.go("subagents")}>
            Subagents
          </button>
        </div>
      </Field>
      {a.warning && (
        <div className="st-field wide">
          <div className="st-note warn">{a.warning}</div>
        </div>
      )}
    </div>
  );
}
