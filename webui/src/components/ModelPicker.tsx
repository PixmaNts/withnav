import { Crew } from "./Crew";
import { CaretDown, CaretRight, MagnifyingGlass, Plugs } from "@phosphor-icons/react";
import { useEffect, useMemo, useRef, useState } from "react";
import type { AgentEntry, ModelEntry } from "../api/types";
import { windowSize } from "../lib/format";
import { groupCatalog, shortName, type CatalogRow } from "../lib/catalog";
import type { Chat } from "../session/useChat";

// The model picker popover: a session override of the active agent's model and
// variant. Chat models only, grouped gateway › provider and collapsible, like
// the terminal picker (`src/tui/catalog.rs`). It never writes config.json.

type Row = CatalogRow;

const SOURCE_LABEL: Record<string, string> = {
  config: "config.json",
  header: "its agent file",
  default_agent: "the default agent",
  parent: "its parent session",
  layer_preference: "the layer preference",
  model_default: "the model's default",
  override: "a session override",
};

function buildRows(models: ModelEntry[], filter: string, collapsed: Set<string>): Row[] {
  return groupCatalog(models, filter, { collapsed: (key) => collapsed.has(key) });
}

export { shortName };

/** The highlighted model's full name, its variants and what it supports. */
function detail(m: ModelEntry): string {
  const variants = m.variants?.length ? `variants: ${m.variants.join(" ")}` : "no variants";
  const caps = [m.supports_tools ? "tools" : "", m.supports_parallel_tool_calls ? "parallel" : ""].filter(Boolean).join(" · ");
  return [m.name, variants, caps].filter(Boolean).join(" · ");
}

export function ModelPicker({ chat, onClose, onSwitchAgent }: { chat: Chat; onClose: () => void; onSwitchAgent: (id: string) => void }) {
  const [filter, setFilter] = useState("");
  const [collapsed, setCollapsed] = useState<Set<string>>(() => new Set());
  const [tab, setTab] = useState(chat.agentId);
  const [cursor, setCursor] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);
  const search = useRef<HTMLInputElement>(null);
  const primaries = (chat.agents?.agents ?? []).filter((a) => a.mode === "primary" || a.mode === "all");
  const tabs = primaries.length ? primaries : [{ id: chat.agentId } as AgentEntry];
  const editing = tab === chat.agentId;
  const sel = chat.selection;
  const rows = useMemo(() => buildRows(chat.chatModels, filter, collapsed), [chat.chatModels, filter, collapsed]);
  const modelRows = rows.flatMap((row, i) => (row.type === "model" ? [i] : []));

  // What each primary agent runs now: the active one per the stream, the others per the server.
  const current = (a: AgentEntry): { model: string | null; variant: string | null; source?: string } =>
    a.id === chat.agentId ? { model: sel.model, variant: sel.variant } : { model: a.effective?.model ?? null, variant: a.effective?.variant ?? null, source: a.effective?.source };
  const usedBy = (name: string) => tabs.filter((a) => current(a).model === name).map((a) => <Crew key={a.id} id={a.id} />);

  const shownAgent = tabs.find((a) => a.id === tab);
  const shown = shownAgent ? current(shownAgent) : { model: sel.model, variant: sel.variant };
  const assigned = chat.agents?.agents.find((a) => a.id === tab)?.assignment ?? null;
  const overridden = !!assigned && (assigned.model !== shown.model || (assigned.variant ?? null) !== (shown.variant ?? null));
  const shownModel = chat.chatModels.find((m) => m.name === shown.model);
  const variants = shownModel?.variants ?? [];

  useEffect(() => {
    search.current?.focus();
  }, []);
  useEffect(() => {
    // Start on the model in use; while searching, on the best match.
    const inUse = rows.findIndex((r) => r.type === "model" && r.model.name === shown.model);
    const first = rows.findIndex((r) => r.type === "model");
    setCursor(filter.trim() ? first : inUse >= 0 ? inUse : first);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [filter]);
  useEffect(() => {
    listRef.current?.querySelector(`[data-row="${cursor}"]`)?.scrollIntoView({ block: "nearest" });
  }, [cursor]);

  const toggle = (key: string) =>
    setCollapsed((s) => {
      const next = new Set(s);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });

  const choose = (model: ModelEntry) => {
    if (!editing) return;
    void chat.pickModel(model.name);
  };

  const onKey = (e: React.KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      if (filter) setFilter("");
      else onClose();
      return;
    }
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (!modelRows.length) return;
      const pos = modelRows.indexOf(cursor);
      const next = e.key === "ArrowDown" ? (pos + 1) % modelRows.length : (pos - 1 + modelRows.length) % modelRows.length;
      setCursor(modelRows[pos === -1 ? 0 : next]!);
      return;
    }
    if (e.key === "Enter") {
      e.preventDefault();
      const row = rows[cursor];
      if (row?.type === "model") choose(row.model);
      return;
    }
    if (e.altKey && e.code === "KeyA") {
      // Folds every group, or unfolds them all once everything is folded.
      e.preventDefault();
      const anyOpen = rows.some((r) => (r.type === "gateway" || r.type === "provider") && !r.collapsed);
      if (!anyOpen) return setCollapsed(new Set());
      const all = new Set<string>();
      for (const m of chat.chatModels) {
        all.add(m.gateway);
        if (m.provider_group !== m.gateway) all.add(`${m.gateway}/${m.provider_group}`);
      }
      setCollapsed(all);
    }
  };

  const variantDesc = (() => {
    if (!shownModel) return "";
    const none = variants.length ? "" : "No variants for this model. ";
    if (!editing) return `${none}Switch the session to ${tab} to change its model here.`;
    if (!assigned) return `${none}${tab} has no model of its own in config.json.`;
    if (overridden) return `${none}Session override for ${tab}; config.json keeps ${assigned.model}${assigned.variant ? ` · ${assigned.variant}` : ""}.`;
    return `${none}Same as ${SOURCE_LABEL[assigned.source] ?? assigned.source} → agents.${tab}.`;
  })();

  return (
    <div className="layer" onMouseDown={onClose}>
      <div className="picker" role="dialog" aria-label="Model for this session" onMouseDown={(e) => e.stopPropagation()} onKeyDown={onKey}>
        <div className="picker-tabs" role="tablist">
          {tabs.map((a) => {
            const c = current(a);
            return (
              <button key={a.id} type="button" role="tab" aria-selected={a.id === tab} className="picker-tab" onClick={() => setTab(a.id)}>
                <span className="t">
                  <Crew id={a.id} />
                  {a.id === chat.agentId ? " · Active" : ""}
                </span>
                <span className="s">
                  {c.model ? tabModelName(c.model, chat.chatModels) : "unassigned"}
                  {c.variant ? ` · ${c.variant}` : ""}
                </span>
              </button>
            );
          })}
        </div>
        <div className="search">
          <label>
            <MagnifyingGlass size={14} aria-hidden />
            <input
              ref={search}
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
              placeholder="Search models"
              aria-label="Search models"
              role="combobox"
              aria-expanded="true"
              aria-controls="model-list"
              aria-activedescendant={rows[cursor]?.type === "model" ? `model-${cursor}` : undefined}
            />
          </label>
        </div>
        <div className="picker-list" id="model-list" role="listbox" aria-label="Chat models" ref={listRef}>
          {!rows.length && <div className="picker-empty">{chat.chatModels.length ? "No model matches this search." : "The server lists no chat models."}</div>}
          {rows.map((row, i) => {
            if (row.type === "gateway")
              return (
                <button key={`g:${row.key}`} type="button" className="g-row" aria-expanded={!row.collapsed} onClick={() => toggle(row.key)}>
                  {row.collapsed ? <CaretRight size={11} /> : <CaretDown size={11} />}
                  <Plugs size={14} aria-hidden />
                  <span className="mono">{row.label}</span>
                  <span className="count">{row.count} models</span>
                </button>
              );
            if (row.type === "provider")
              return (
                <button key={`p:${row.key}`} type="button" className="p-row" aria-expanded={!row.collapsed} onClick={() => toggle(row.key)}>
                  {row.collapsed ? <CaretRight size={10} /> : <CaretDown size={10} />}
                  <span className="mono">{row.label}</span>
                  <span className="count">{row.count}</span>
                </button>
              );
            const m = row.model;
            const on = m.name === shown.model;
            return (
              <button
                key={m.name}
                id={`model-${i}`}
                data-row={i}
                type="button"
                role="option"
                aria-selected={on}
                aria-disabled={!editing}
                aria-label={m.name}
                className={`m-row${row.direct ? " direct" : ""}${i === cursor ? " active" : ""}`}
                onClick={() => {
                  setCursor(i);
                  choose(m);
                }}
                title={m.name}
              >
                <span className="mark">{on ? "●" : "○"}</span>
                <span className="id">{shortName(m)}</span>
                <span className="used">{usedBy(m.name)}</span>
                <span className="ctx">{windowSize(m.context_window) || "—"}</span>
              </button>
            );
          })}
        </div>
        <div className="picker-detail mono" aria-live="polite">
          {rows[cursor]?.type === "model" ? detail((rows[cursor] as Extract<Row, { type: "model" }>).model) : "\u00a0"}
        </div>
        <div className="picker-variants">
          <div className="variant-row">
            <span className="label">Variant</span>
            {variants.map((v) => (
              <button key={v} type="button" className="variant" aria-pressed={shown.variant === v} disabled={!editing} onClick={() => void chat.pickVariant(v)}>
                {v}
              </button>
            ))}
            {variants.length > 0 && (
              <button type="button" className="variant" aria-pressed={shown.variant === null} disabled={!editing} onClick={() => void chat.pickVariant(null)} title="No variant">
                off
              </button>
            )}
            {!variants.length && <span className="muted">none offered</span>}
          </div>
          {variantDesc && <div className="variant-desc">{variantDesc}</div>}
        </div>
        <div className="picker-foot">
          {editing ? (
            <>
              <span className="mono">
                {overridden
                  ? `session override · agent ${tab} · config.json unchanged`
                  : `agent ${tab} · from ${assigned ? SOURCE_LABEL[assigned.source] ?? assigned.source : "the server"}${assigned?.source === "config" ? ` → agents.${tab}.model` : ""}`}
              </span>
              {overridden && (
                <button type="button" className="btn" onClick={() => void chat.backToConfig()}>
                  Back to config
                </button>
              )}
            </>
          ) : (
            <>
              <span className="mono">agent {tab} · not active in this session</span>
              <button type="button" className="btn" onClick={() => onSwitchAgent(tab)}>
                Switch to {tab}
              </button>
            </>
          )}
        </div>
        {chat.error && <div className="picker-foot" style={{ color: "var(--rd)" }}>{chat.error}</div>}
      </div>
    </div>
  );
}


/** In the tab subtitle, the model's own id (like the design: `claude-opus-5-5`). */
function tabModelName(name: string, models: ModelEntry[]): string {
  const m = models.find((x) => x.name === name);
  return m ? shortName(m) : name.split("/").pop() ?? name;
}
