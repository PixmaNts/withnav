import type { AgentEntry, LayerEntry, ModelEntry, SettingsBody } from "../api/types";
import { effective, fieldOf, type SectionId } from "../lib/settings";
import type { ThemeId } from "../theme/themes";
import type { SettingsSave } from "./useSettingsSave";

/** What every section reads and writes through. */
export interface SectionProps {
  workspace: string;
  body: SettingsBody;
  models: ModelEntry[];
  /** The catalogue has not answered yet: model details are unknown, not absent. */
  modelsLoading: boolean;
  agents: AgentEntry[];
  layers: LayerEntry[];
  save: SettingsSave;
  /** Whether a search entry stays visible (always true without a query). */
  visible: (id: string) => boolean;
  go: (section: SectionId) => void;
  theme: ThemeId;
  setTheme: (id: ThemeId) => void;
}

/** The value a control shows: the change being saved, else what config.json holds, else the default. */
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export function shown(p: Pick<SectionProps, "body" | "save">, key: string): any {
  const pending = p.save.pending(key);
  const field = fieldOf(p.body, key);
  if (pending.has) return pending.value === null ? field?.default ?? null : pending.value;
  return effective(field);
}

/** True when config.json holds the key (a change being saved counts as held). */
export function isSet(p: Pick<SectionProps, "body" | "save">, key: string): boolean {
  const pending = p.save.pending(key);
  if (pending.has) return pending.value !== null;
  return !!fieldOf(p.body, key)?.set;
}

/**
 * The message to show at a control: the server's refusal of the last change made from it (or from
 * any control of the key when none is named), else the error of the value stored in config.json.
 */
export function problem(p: Pick<SectionProps, "body" | "save">, key: string, control?: string): string | null {
  const refused = p.save.state.fieldErrors[key];
  const from = p.save.state.errorControl[key];
  if (refused && (!control || !from || from === control)) return refused;
  if (refused) return null;
  return control && control !== "model" ? null : fieldOf(p.body, key)?.error ?? null;
}
