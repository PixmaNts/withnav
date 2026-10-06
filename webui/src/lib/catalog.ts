import type { ModelEntry } from "../api/types";
import { ranked } from "./search";

// Catalogue rows grouped gateway › provider, as every model list of the design
// shows them (docs/design/SETTINGS.md, "Model names"). Shared by the session
// model picker and the Settings page so both group, sort and search alike.

export type CatalogRow =
  | { type: "gateway"; key: string; label: string; count: number; collapsed: boolean }
  | { type: "provider"; key: string; label: string; count: number; collapsed: boolean; direct: boolean }
  | { type: "model"; model: ModelEntry; direct: boolean };

export interface GroupOptions {
  /** Whether a group key (`gateway` or `gateway/provider`) is folded. Ignored while searching. */
  collapsed: (key: string) => boolean;
  /** Emit gateway header rows (off when the list already belongs to one gateway). */
  gatewayRows?: boolean;
  /** Emit a `(direct)` provider row for models served by the gateway itself. */
  directRows?: boolean;
}

export function groupCatalog(models: ModelEntry[], filter: string, options: GroupOptions): CatalogRow[] {
  const { collapsed, gatewayRows = true, directRows = false } = options;
  const groups = new Map<string, Map<string, ModelEntry[]>>();
  for (const model of ranked(models, filter, (m) => m.name, (m) => m.upstream_id)) {
    const providers = groups.get(model.gateway) ?? new Map<string, ModelEntry[]>();
    groups.set(model.gateway, providers);
    const list = providers.get(model.provider_group) ?? [];
    providers.set(model.provider_group, list);
    list.push(model);
  }
  const searching = !!filter.trim();
  const rows: CatalogRow[] = [];
  for (const gateway of [...groups.keys()].sort()) {
    const providers = groups.get(gateway)!;
    const count = [...providers.values()].reduce((n, l) => n + l.length, 0);
    const gClosed = gatewayRows && !searching && collapsed(gateway);
    if (gatewayRows) rows.push({ type: "gateway", key: gateway, label: gateway, count, collapsed: gClosed });
    if (gClosed) continue;
    for (const provider of [...providers.keys()].sort()) {
      const list = providers.get(provider)!;
      const key = `${gateway}/${provider}`;
      const direct = provider === gateway;
      const showRow = !direct || directRows;
      const pClosed = showRow && !searching && collapsed(key);
      if (showRow) rows.push({ type: "provider", key, label: direct ? "(direct)" : provider, count: list.length, collapsed: pClosed, direct });
      if (pClosed) continue;
      const sorted = searching ? list : [...list].sort((a, b) => a.name.localeCompare(b.name));
      for (const model of sorted) rows.push({ type: "model", model, direct: direct && !directRows });
    }
  }
  return rows;
}

/** Every group key of a model list: to fold or unfold them all. */
export function groupKeys(models: ModelEntry[]): string[] {
  const keys = new Set<string>();
  for (const m of models) {
    keys.add(m.gateway);
    keys.add(`${m.gateway}/${m.provider_group}`);
  }
  return [...keys];
}

/**
 * The canonical name without the `gateway/provider_group/` prefix the group headers already show
 * (`gateway/` for a direct gateway); the full name when the prefix does not match. Same rule as
 * the terminal and VS Code pickers (`src/tui/catalog.rs::short_name`).
 */
export function shortName(m: Pick<ModelEntry, "name" | "gateway" | "provider_group">): string {
  const prefix = m.provider_group && m.provider_group !== m.gateway ? `${m.gateway}/${m.provider_group}/` : `${m.gateway}/`;
  return m.name.startsWith(prefix) && m.name.length > prefix.length ? m.name.slice(prefix.length) : m.name;
}
