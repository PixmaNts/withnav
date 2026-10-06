import { CaretDown, CaretRight, MagnifyingGlass, Plugs } from "@phosphor-icons/react";
import { forwardRef, useImperativeHandle, useMemo, useRef, useState } from "react";
import type { ModelEntry } from "../api/types";
import { groupCatalog, shortName } from "../lib/catalog";
import { windowSize } from "../lib/format";

/** Above this many models, provider groups start folded (except the one holding the current model). */
const FOLD_ABOVE = 30;

export interface CatalogueHandle {
  focusFilter: () => void;
}

interface Props {
  models: ModelEntry[];
  /** A list that belongs to one gateway: no gateway rows. */
  oneGateway?: boolean;
  /** Show the API in each row's details (the gateway view mixes APIs). */
  showApi?: boolean;
  /** The model currently assigned: marked ● and its group opened. */
  current?: string | null;
  /** Clicking a row assigns it; without this the list is read-only. */
  onPick?: (model: ModelEntry) => void;
  label: string;
  busy?: boolean;
  /** The catalogue is still loading: say so rather than "no model". */
  loading?: boolean;
}

/** The catalogue grouped gateway › provider, collapsible, as in the design's Settings lists. */
export const Catalogue = forwardRef<CatalogueHandle, Props>(function Catalogue({ models, oneGateway, showApi, current, onPick, label, busy, loading }, ref) {
  const [filter, setFilter] = useState("");
  const [overrides, setOverrides] = useState<Map<string, boolean>>(() => new Map());
  const input = useRef<HTMLInputElement>(null);
  useImperativeHandle(ref, () => ({
    focusFilter: () => {
      input.current?.scrollIntoView({ block: "center", behavior: "smooth" });
      input.current?.focus({ preventScroll: true });
    },
  }));
  const currentEntry = models.find((m) => m.name === current);
  const fold = models.length > FOLD_ABOVE;
  const rows = useMemo(
    () =>
      groupCatalog(models, filter, {
        gatewayRows: !oneGateway,
        directRows: true,
        collapsed: (key) => {
          const set = overrides.get(key);
          if (set !== undefined) return set;
          if (!fold || !key.includes("/")) return false;
          return !currentEntry || key !== `${currentEntry.gateway}/${currentEntry.provider_group}`;
        },
      }),
    [models, filter, overrides, oneGateway, fold, currentEntry],
  );
  const toggle = (key: string, collapsed: boolean) => setOverrides((m) => new Map(m).set(key, !collapsed));

  return (
    <div className="st-cat">
      <div className="st-cat-head">
        <span>{label}</span>
        <label className="st-cat-filter">
          <MagnifyingGlass size={12} aria-hidden />
          <input ref={input} value={filter} onChange={(e) => setFilter(e.target.value)} placeholder="Filter models" aria-label="Filter models" onKeyDown={(e) => e.key === "Escape" && setFilter("")} />
        </label>
      </div>
      <div className={`st-cat-list${oneGateway ? " one" : ""}`} role={onPick ? "listbox" : "list"} aria-label={label} aria-busy={busy || undefined}>
        {!rows.length && <div className="st-cat-empty">{loading ? "Loading the catalogue…" : models.length ? "No model matches this filter." : "The catalogue has no model for this."}</div>}
        {rows.map((row) => {
          if (row.type === "gateway")
            return (
              <button key={`g:${row.key}`} type="button" className="st-g" aria-expanded={!row.collapsed} onClick={() => toggle(row.key, row.collapsed)}>
                {row.collapsed ? <CaretRight size={11} /> : <CaretDown size={11} />}
                <Plugs size={14} aria-hidden />
                <span className="mono">{row.label}</span>
                <span className="count">
                  {row.count} model{row.count === 1 ? "" : "s"}
                </span>
              </button>
            );
          if (row.type === "provider")
            return (
              <button key={`p:${row.key}`} type="button" className="st-p" aria-expanded={!row.collapsed} onClick={() => toggle(row.key, row.collapsed)}>
                {row.collapsed ? <CaretRight size={10} /> : <CaretDown size={10} />}
                <span className="mono">{row.label}</span>
                <span className="count">{row.count}</span>
              </button>
            );
          const m = row.model;
          const on = m.name === current;
          const meta = [showApi ? m.api : "", windowSize(m.context_window), m.variants?.length ? m.variants.join(" / ") : ""].filter(Boolean).join(" · ");
          const body = (
            <>
              <span className="mark">{onPick ? (on ? "●" : "○") : ""}</span>
              <span className="names">
                <span className="mono id">{shortName(m)}</span> <span className="mono full">{m.name}</span>
              </span>
              <span className="mono meta">{m.enabled === false ? m.disabled_reason ?? "unavailable" : meta}</span>
            </>
          );
          return onPick ? (
            <button key={m.name} type="button" role="option" aria-selected={on} className="st-m" onClick={() => !on && onPick(m)} title={on ? `${m.name} is assigned` : `Assign ${m.name}`}>
              {body}
            </button>
          ) : (
            <div key={m.name} role="listitem" className="st-m static">
              {body}
            </div>
          );
        })}
      </div>
    </div>
  );
});
