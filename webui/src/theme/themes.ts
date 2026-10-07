// The four themes of the design (docs/design/README.md, "Design tokens — themes").
// Colours live in theme.css as custom properties keyed by `data-theme`.

export const THEMES = [
  { id: "ayu-dark", label: "Ayu Dark", dark: true },
  { id: "ayu-light", label: "Ayu Light", dark: false },
  { id: "nord-dark", label: "Nord Dark", dark: true },
  { id: "nord-light", label: "Nord Light", dark: false },
] as const;

export type ThemeId = (typeof THEMES)[number]["id"];

const KEY = "navai.theme";

export function storedTheme(): ThemeId {
  const value = localStorage.getItem(KEY);
  return THEMES.some((t) => t.id === value) ? (value as ThemeId) : "ayu-dark";
}

export function applyTheme(id: ThemeId): void {
  document.documentElement.dataset.theme = id;
  document.documentElement.style.colorScheme = THEMES.find((t) => t.id === id)?.dark ? "dark" : "light";
  localStorage.setItem(KEY, id);
}
