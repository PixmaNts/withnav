/// <reference lib="dom" />
/// <reference lib="dom.iterable" />
import type { GatewayTest, HostToSettings, SettingsSnapshot, SettingsToHost } from "../settingsProtocol";
import { buildSections, countRows, matches, shortPath, type Control, type LinkAction, type Row, type Section } from "./settingsModel";

interface ViewState {
  section?: string;
  query?: string;
  modifiedOnly?: boolean;
}
declare function acquireVsCodeApi(): { postMessage(message: SettingsToHost): void; getState(): ViewState | undefined; setState(state: ViewState): void };
const vscode = acquireVsCodeApi();
const post = (message: SettingsToHost): void => vscode.postMessage(message);

const $ = <T extends HTMLElement>(id: string): T => document.getElementById(id) as T;
const searchInput = $<HTMLInputElement>("search");
const count = $("count");
const toc = $("toc");
const rowsHost = $("rows");
const paths = $("paths");
const saveState = $("save-state");
const filterButton = $<HTMLButtonElement>("filter");
const jsonButton = $<HTMLButtonElement>("open-json");

const saved = vscode.getState() ?? {};
let snapshot: SettingsSnapshot | undefined;
let sections: Section[] = [];
let selected = saved.section ?? "";
let query = saved.query ?? "";
let modifiedOnly = saved.modifiedOnly ?? false;
let loadError: string | null = null;
let noWorkspace = false;
/** Refusals (400) and failures kept under their key until it saves. */
const errors = new Map<string, string>();
const saving = new Set<string>();
let banner: string | null = null;
const tests = new Map<string, GatewayTest | "testing">();

searchInput.value = query;

const SVG = "http://www.w3.org/2000/svg";
function icon(paths: string[], size = 16): SVGSVGElement {
  const svg = document.createElementNS(SVG, "svg");
  svg.setAttribute("viewBox", "0 0 16 16");
  svg.setAttribute("width", String(size));
  svg.setAttribute("height", String(size));
  svg.setAttribute("aria-hidden", "true");
  for (const d of paths) {
    const p = document.createElementNS(SVG, "path");
    p.setAttribute("d", d);
    svg.append(p);
  }
  return svg;
}
// Codicon-like outlines (funnel, braces), drawn as strokes so they follow currentColor.
filterButton.append(icon(["M2 3h12l-4.5 5.2V13l-3 -1.5V8.2z"]));
jsonButton.append(icon(["M5.5 2.5c-1.5 0 -2 .6 -2 1.8v2c0 .9 -.5 1.4 -1.3 1.7c.8 .3 1.3 .8 1.3 1.7v2c0 1.2 .5 1.8 2 1.8", "M10.5 2.5c1.5 0 2 .6 2 1.8v2c0 .9 .5 1.4 1.3 1.7c-.8 .3 -1.3 .8 -1.3 1.7v2c0 1.2 -.5 1.8 -2 1.8"]));

function el<K extends keyof HTMLElementTagNameMap>(tag: K, className?: string, text?: string): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  if (className) e.className = className;
  if (text !== undefined) e.textContent = text;
  return e;
}

function persist(): void {
  vscode.setState({ section: selected, query, modifiedOnly });
}

function sectionById(id: string): Section | undefined {
  return sections.find((s) => s.id === id);
}

/** The section a TOC entry shows: top-level entries open their first child. */
function resolve(id: string): string {
  const s = sectionById(id);
  return s?.opens ?? id;
}

function isActive(entry: Section): boolean {
  if (query || modifiedOnly) return false;
  if (entry.id === selected) return true;
  if (entry.level === 0) {
    if (entry.id === "gateways") return selected.startsWith("gateway:");
    if (entry.id === "layers") return selected.startsWith("layer:") || selected.startsWith("agent:");
  }
  return entry.id === "layer:cognition" && selected.startsWith("agent:");
}

/** While searching, the TOC keeps only the sections with a hit and their parents, as in VS Code. */
function tocEntries(): Section[] {
  const q = `${query} ${modifiedOnly ? "@modified" : ""}`.trim();
  if (!q) return sections;
  const hit = new Set(sections.filter((s) => s.rows.some((r) => matches(r, q))).map((s) => s.id));
  if ([...hit].some((id) => id.startsWith("gateway:"))) hit.add("gateways");
  if ([...hit].some((id) => id.startsWith("layer:") || id.startsWith("agent:"))) hit.add("layers");
  if ([...hit].some((id) => id.startsWith("agent:"))) hit.add("layer:cognition");
  return sections.filter((s) => hit.has(s.id));
}

function renderToc(): void {
  toc.replaceChildren();
  for (const s of tocEntries()) {
    const item = el("a", `toc-item level-${s.level}`, s.label);
    item.href = "#";
    item.dataset.section = s.id;
    if (s.id === selected && !query && !modifiedOnly) {
      item.classList.add("selected");
      item.setAttribute("aria-current", "true");
    } else if (isActive(s)) item.classList.add("active");
    item.addEventListener("click", (event) => {
      event.preventDefault();
      go(s.id);
    });
    toc.append(item);
  }
}

function go(id: string): void {
  selected = resolve(id);
  query = "";
  modifiedOnly = false;
  searchInput.value = "";
  persist();
  render();
  rowsHost.scrollTop = 0;
}

function act(action: LinkAction): void {
  if ("open" in action) post({ type: "open", target: action.open });
  else if ("openAgent" in action) post({ type: "openAgent", id: action.openAgent });
  else go(action.section);
}

function save(row: Row, input: string | boolean | string[]): void {
  if (saving.size || !row.key || !row.write) return;
  const value = row.write(input);
  if (value === undefined) return;
  errors.delete(row.key);
  post({ type: "save", key: row.key, value });
}

function controlFor(row: Row): HTMLElement {
  const c: Control = row.control;
  const label = `${row.category} ${row.title}`;
  switch (c.kind) {
    case "text": {
      const input = el("input", `control text${c.mono !== false ? " mono" : ""}`);
      input.type = "text";
      input.value = c.value;
      input.disabled = !!c.disabled;
      input.setAttribute("aria-label", label);
      if (!c.disabled) input.addEventListener("change", () => save(row, input.value));
      return input;
    }
    case "number": {
      const input = el("input", "control number mono");
      input.type = "number";
      input.value = c.value === null ? "" : String(c.value);
      if (c.placeholder) input.placeholder = c.placeholder;
      if (c.min !== undefined) input.min = String(c.min);
      if (c.max !== undefined) input.max = String(c.max);
      input.step = "1";
      input.disabled = !!c.disabled;
      input.setAttribute("aria-label", label);
      if (!c.disabled)
        input.addEventListener("change", () => {
          if (input.value.trim() === "") {
            if (row.key) post({ type: "save", key: row.key, value: null });
            return;
          }
          save(row, input.value);
        });
      return input;
    }
    case "select": {
      const wrap = el("div", `select-wrap${c.wide ? " wide" : ""}`);
      const select = el("select", "control select mono");
      select.setAttribute("aria-label", label);
      for (const o of c.options) select.append(option(o.value, o.label, o.disabled));
      for (const g of c.groups ?? []) {
        const group = el("optgroup");
        group.label = g.label;
        for (const o of g.options) group.append(option(o.value, o.label, o.disabled));
        select.append(group);
      }
      select.value = c.value;
      select.disabled = !!c.disabled;
      if (!c.disabled) select.addEventListener("change", () => save(row, select.value));
      wrap.append(select);
      return wrap;
    }
    case "checkbox": {
      const wrap = el("label", "checkbox-row");
      const box = el("input", "checkbox");
      box.type = "checkbox";
      box.checked = c.checked;
      box.setAttribute("aria-label", label);
      box.addEventListener("change", () => save(row, box.checked));
      wrap.append(box, el("span", "checkbox-label", c.label));
      return wrap;
    }
    case "list":
      return listControl(row, c);
    case "link": {
      const a = el("a", "link", c.label);
      a.href = "#";
      a.addEventListener("click", (event) => {
        event.preventDefault();
        act(c.action);
      });
      return a;
    }
    case "test":
      return testControl(c.gateway);
  }
}

function option(value: string, label: string, disabled?: boolean): HTMLOptionElement {
  const o = el("option", undefined, label);
  o.value = value;
  o.disabled = !!disabled;
  return o;
}

function listControl(row: Row, c: Extract<Control, { kind: "list" }>): HTMLElement {
  const wrap = el("div", "list");
  wrap.setAttribute("role", "list");
  wrap.setAttribute("aria-label", `${row.category} ${row.title}`);
  c.items.forEach((item, i) => {
    const link = c.links?.[i];
    const line = el("div", `list-item mono${link ? " clickable" : ""}`);
    line.setAttribute("role", "listitem");
    if (link) {
      const a = el("a", "list-link", link.label);
      a.href = "#";
      a.addEventListener("click", (event) => {
        event.preventDefault();
        go(link.section);
      });
      line.append(a);
    } else line.append(el("span", "list-text", item));
    if (c.editable) {
      const remove = el("button", "list-remove", "×");
      remove.type = "button";
      remove.title = "Remove Item";
      remove.setAttribute("aria-label", `Remove ${item}`);
      remove.addEventListener("click", () => save(row, c.items.filter((_, n) => n !== i)));
      line.append(remove);
    }
    wrap.append(line);
  });
  if (!c.items.length && c.editable) wrap.append(el("div", "list-empty", "Empty: scratch space is off."));
  if (c.editable) {
    const add = el("button", "button primary", "Add Item");
    add.type = "button";
    add.addEventListener("click", () => {
      add.hidden = true;
      const editor = el("div", "list-edit");
      const input = el("input", "control text mono");
      input.type = "text";
      input.placeholder = c.placeholder ?? "";
      input.setAttribute("aria-label", `New item for ${row.title}`);
      const ok = el("button", "button primary", "OK");
      ok.type = "button";
      const cancel = el("button", "button secondary", "Cancel");
      cancel.type = "button";
      const commit = () => {
        const value = input.value.trim();
        if (value) save(row, [...c.items, value]);
        else close();
      };
      const close = () => {
        editor.remove();
        add.hidden = false;
        add.focus();
      };
      ok.addEventListener("click", commit);
      cancel.addEventListener("click", close);
      input.addEventListener("keydown", (event) => {
        if (event.key === "Enter") commit();
        else if (event.key === "Escape") close();
      });
      editor.append(input, ok, cancel);
      wrap.insertBefore(editor, add);
      input.focus();
    });
    wrap.append(add);
  }
  return wrap;
}

function testControl(name: string): HTMLElement {
  const wrap = el("div", "test-row");
  const a = el("a", "link", "Test connection");
  a.href = "#";
  const state = tests.get(name);
  a.addEventListener("click", (event) => {
    event.preventDefault();
    if (tests.get(name) !== "testing") post({ type: "testGateway", name });
  });
  wrap.append(a);
  if (state === "testing") wrap.append(el("span", "test-result", "Testing…"));
  else if (state) {
    const text = state.ok
      ? `Connected in ${state.latency_ms} ms${state.model_count !== null ? ` · ${state.model_count} models` : ""}`
      : `Failed${state.status ? ` (${state.status})` : ""}: ${state.error ?? "no answer"}`;
    wrap.append(el("span", `test-result ${state.ok ? "ok" : "failed"}`, text));
  }
  return wrap;
}

function rowElement(row: Row): HTMLElement {
  const item = el("div", `row${row.modified ? " modified" : ""}`);
  item.dataset.row = row.id;
  const head = el("div", "row-title");
  head.append(el("span", "row-category", row.category), el("span", "row-name", row.title));
  if (row.modified) head.append(el("span", "row-modified", "Modified"));
  if (row.key && saving.has(row.key)) head.append(el("span", "row-saving", "Saving…"));
  if (row.resettable && row.key) {
    const reset = el("a", "row-reset", "Reset Setting");
    reset.href = "#";
    reset.title = "Remove the key from config.json; the default applies";
    reset.addEventListener("click", (event) => {
      event.preventDefault();
      if (row.key && !saving.size) {
        errors.delete(row.key);
        post({ type: "save", key: row.key, value: null });
      }
    });
    head.append(reset);
  }
  const description = el("div", "row-description", row.description);
  if (row.applies) description.append(el("span", `row-applies${row.applies.includes("restarting") ? " restart" : ""}`, ` ${row.applies}`));
  item.append(head, description, controlFor(row));
  const error = (row.key && errors.get(row.key)) || row.error;
  if (error) {
    const e = el("div", "row-error", error);
    e.setAttribute("role", "alert");
    item.append(e);
    item.classList.add("invalid");
  }
  if (row.warning) item.append(el("div", "row-warning", row.warning));
  return item;
}

function sectionHead(s: Section, showLink = true): HTMLElement[] {
  const title = el("h2", "section-title", s.title);
  const desc = el("div", "section-description", s.description);
  if (showLink && s.descriptionLink) {
    const a = el("a", "link", s.descriptionLink.label);
    a.href = "#";
    const action = s.descriptionLink.action;
    a.addEventListener("click", (event) => {
      event.preventDefault();
      act(action);
    });
    desc.append(" ", a);
  }
  const notes = s.notes?.map((note) => el("div", "row-warning", note)) ?? [];
  return [title, desc, ...notes];
}

function renderRows(): number {
  rowsHost.replaceChildren();
  if (banner) {
    const b = el("div", "banner", banner);
    b.setAttribute("role", "alert");
    rowsHost.append(b);
  }
  const filtering = !!query.trim() || modifiedOnly;
  if (!filtering) {
    const s = sectionById(selected);
    if (!s) return 0;
    rowsHost.append(...sectionHead(s));
    for (const row of s.rows) rowsHost.append(rowElement(row));
    return s.rows.length;
  }
  const q = `${query} ${modifiedOnly ? "@modified" : ""}`;
  let found = 0;
  for (const s of sections) {
    const hits = s.rows.filter((r) => matches(r, q));
    if (!hits.length) continue;
    found += hits.length;
    const head = el("h3", "group-title", s.level === 2 ? `Cognition › ${s.title}` : s.level === 1 && s.id.startsWith("gateway:") ? `Gateways › ${s.title}` : s.title);
    rowsHost.append(head, ...hits.map(rowElement));
  }
  if (!found) rowsHost.append(el("p", "placeholder", "No settings found."));
  return found;
}

function render(): void {
  if (!snapshot) {
    rowsHost.replaceChildren(el("p", loadError ? "placeholder error" : "placeholder", noWorkspace ? "Open a folder to edit Navai settings" : loadError ?? "Loading Navai settings…"));
    if (loadError || noWorkspace) {
      const retry = el("button", "button primary", noWorkspace ? "Open Folder" : "Retry");
      retry.type = "button";
      retry.addEventListener("click", () => post({ type: noWorkspace ? "openFolder" : "reload" }));
      rowsHost.append(retry);
    }
    count.textContent = "";
    return;
  }
  // Keep focus on the control being used across a re-render.
  const active = document.activeElement as HTMLElement | null;
  const focusRow = active?.closest<HTMLElement>(".row")?.dataset.row;
  const focusTag = active?.tagName;
  if (!sectionById(selected)) selected = resolve(sections[0]?.id ?? "");
  renderToc();
  const shown = renderRows();
  if (saving.size) {
    for (const control of rowsHost.querySelectorAll<HTMLInputElement | HTMLSelectElement | HTMLButtonElement>("input, select, button")) control.disabled = true;
    for (const reset of rowsHost.querySelectorAll<HTMLElement>(".row-reset")) reset.setAttribute("aria-disabled", "true");
  }
  const total = countRows(sections);
  count.textContent = query.trim() || modifiedOnly ? `${shown} Setting${shown === 1 ? "" : "s"} Found` : `${total} Settings`;
  filterButton.setAttribute("aria-pressed", String(modifiedOnly));
  filterButton.classList.toggle("on", modifiedOnly);
  const s = snapshot.settings.paths;
  paths.textContent = `${shortPath(s.config, snapshot.home)} · ${s.models.split("/").pop()}`;
  paths.title = `${s.config}\n${s.models}`;
  if (focusRow && focusTag) {
    const again = [...rowsHost.querySelectorAll<HTMLElement>(".row")].find((r) => r.dataset.row === focusRow);
    again?.querySelector<HTMLElement>(focusTag.toLowerCase())?.focus();
  }
}

function setSaveState(text: string, kind = ""): void {
  saveState.textContent = text;
  saveState.className = `save-state ${kind}`;
}

window.addEventListener("message", (event: MessageEvent<HostToSettings>) => {
  const m = event.data;
  switch (m.type) {
    case "noWorkspace":
      noWorkspace = true;
      render();
      break;
    case "snapshot":
      snapshot = m.data;
      loadError = null;
      sections = buildSections(m.data);
      render();
      break;
    case "loadError":
      loadError = m.message;
      if (!snapshot) render();
      else {
        banner = m.message;
        render();
      }
      break;
    case "saving":
      saving.add(m.key);
      setSaveState("Saving…");
      render();
      break;
    case "saved":
      saving.delete(m.key);
      errors.delete(m.key);
      banner = null;
      setSaveState(saving.size ? "Saving…" : "Saved", "ok");
      render();
      break;
    case "invalid":
      saving.delete(m.key);
      errors.set(m.key, m.message);
      setSaveState("Not saved", "error");
      render();
      break;
    case "stale":
      saving.clear();
      errors.clear();
      banner = m.message;
      setSaveState("Reloaded", "error");
      render();
      break;
    case "saveFailed":
      saving.delete(m.key);
      errors.set(m.key, m.message);
      setSaveState("Not saved", "error");
      render();
      break;
    case "testing":
      tests.set(m.name, "testing");
      render();
      break;
    case "testResult":
      tests.set(m.name, m.result);
      render();
      break;
  }
});

searchInput.addEventListener("input", () => {
  query = searchInput.value;
  persist();
  render();
});
searchInput.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && searchInput.value) {
    searchInput.value = "";
    query = "";
    persist();
    render();
  }
});
filterButton.addEventListener("click", () => {
  modifiedOnly = !modifiedOnly;
  persist();
  render();
});
jsonButton.addEventListener("click", () => post({ type: "open", target: "config" }));
$("extension-settings").addEventListener("click", (event) => {
  event.preventDefault();
  post({ type: "openExtensionSettings" });
});
// Ctrl/Cmd+F focuses the search, as in the VS Code settings editor.
window.addEventListener("keydown", (event) => {
  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "f") {
    event.preventDefault();
    searchInput.focus();
    searchInput.select();
  }
});

render();
// The VS Code settings editor opens with the search focused.
searchInput.focus();
post({ type: "ready" });
