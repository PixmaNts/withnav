// The four themes of the design (docs/design/README.md, "Design tokens — themes").
// Colours live in theme.css as custom properties keyed by `data-theme`.

import { migratedValue } from "../lib/storage";
export const THEMES = [
  { id: "ayu-dark", label: "Ayu Dark", dark: true },
  { id: "ayu-light", label: "Ayu Light", dark: false },
  { id: "nord-dark", label: "Nord Dark", dark: true },
  { id: "nord-light", label: "Nord Light", dark: false },
  { id: "tide-dark", label: "Tide Dark", dark: true },
  { id: "tide-light", label: "Tide Light", dark: false },
  { id: "moss-dark", label: "Moss Dark", dark: true },
  { id: "moss-light", label: "Moss Light", dark: false },
] as const;

export type ThemeId = (typeof THEMES)[number]["id"];

const KEY = "withnav.theme";

export function storedTheme(): ThemeId {
  const value = migratedValue(localStorage, KEY, "navai.theme");
  return THEMES.some((t) => t.id === value) ? (value as ThemeId) : "tide-dark";
}

export function applyTheme(id: ThemeId): void {
  document.documentElement.dataset.theme = id;
  document.documentElement.style.colorScheme = THEMES.find((t) => t.id === id)?.dark ? "dark" : "light";
  localStorage.setItem(KEY, id);
}
