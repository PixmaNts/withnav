const pendingMarkdown = new WeakSet<HTMLElement>();
import { fromMarkdown } from "mdast-util-from-markdown";
import { gfm } from "micromark-extension-gfm";
import { gfmFromMarkdown } from "mdast-util-gfm";
/// <reference lib="dom" />
import type { HostToWebview, WebviewToHost, PermissionEvidence, PermissionDecision } from "../chatProtocol";
import { evidenceLabel, permissionOutcome } from "../eventView";
import { contextMeter } from "../contextMeter";
import { ConversationIdentity } from "./conversationIdentity";
import { mountConversations } from "./conversations";
import { diffKind, parseWake } from "./wake";
import { ranked, PICKER_LIMIT } from "./pickerSearch";
import type { CommandInfo } from "../chatProtocol";
import type { ChildInfo } from "../chatProtocol";

type ViewState = { includeContext?: boolean };
declare function acquireVsCodeApi(): { postMessage(message: WebviewToHost): void; getState(): ViewState | undefined; setState(state: ViewState): void };
const vscode = acquireVsCodeApi();
const rawPost = vscode.postMessage.bind(vscode);
const identity = new ConversationIdentity(() => {
  for (const button of document.querySelectorAll<HTMLButtonElement>(".control-card button, .control-card textarea")) button.disabled = true;
});
const post = (message: WebviewToHost): void => rawPost({ ...message, generation: identity.generation });
const bypassToggle = document.createElement("button");
bypassToggle.type = "button"; bypassToggle.className = "bypass-toggle";
function renderBypass(enabled: boolean, pending = false, yolo = false, unavailable?: string): void {
  enabled = enabled || yolo || !!unavailable;
  const label = yolo ? "YOLO" : unavailable ? "Permission rules unavailable" : enabled ? "Bypass active" : "Safe mode";
  bypassToggle.innerHTML = `<svg class="icon" aria-hidden="true" viewBox="0 0 16 16"><path d="M8 1 14 3v5c0 3-6 7-6 7S2 11 2 8V3Z"/>${enabled ? '<path d="m2 2 12 12"/>' : ''}</svg><span></span>`;
  bypassToggle.querySelector("span")!.textContent = label;
  bypassToggle.setAttribute("aria-label", label);
  bypassToggle.setAttribute("aria-pressed", String(enabled));
  bypassToggle.dataset.pending = String(pending);
  bypassToggle.title = unavailable && !yolo ? unavailable : yolo ? "YOLO: no permission control at all. Set yolo to false in config.json to turn it off." : enabled ? "Ordinary permission prompts are answered for you; protected configuration, credentials and keys still ask." : "Every gated call is reviewed and may ask you.";
}
renderBypass(false);
document.querySelector(".topbar .identity")!.after(bypassToggle);
bypassToggle.addEventListener("click", () => post({ type: "toggleBypass" }));
const transcript = document.querySelector<HTMLElement>("#transcript")!;
const empty = document.querySelector<HTMLElement>("#empty")!;
const status = document.querySelector<HTMLElement>("#status")!;
const statusLabel = document.querySelector<HTMLElement>("#status-label")!;
const form = document.querySelector<HTMLFormElement>("#composer")!;
const prompt = document.querySelector<HTMLTextAreaElement>("#prompt")!;
const send = document.querySelector<HTMLButtonElement>("#send")!;
const stop = document.querySelector<HTMLButtonElement>("#stop")!;
const includeContext = document.querySelector<HTMLInputElement>("#include-context")!;
const contextPreview = document.querySelector<HTMLElement>("#context-preview")!;
const modelSelect = document.querySelector<HTMLSelectElement>("#model")!;
const agentSelect = document.createElement("select");
agentSelect.className = "model-select"; agentSelect.setAttribute("aria-label", "Conversation agent");
modelSelect.before(agentSelect);
agentSelect.addEventListener("change", () => post({ type: "selectAgent", id: agentSelect.value }));
const childHeader = document.createElement("div");
childHeader.className = "child-header"; childHeader.hidden = true;
form.before(childHeader);
const childrenSection = document.createElement("details");
childrenSection.className = "subagents"; childrenSection.hidden = true;
const childrenSummary = document.createElement("summary"); childrenSummary.textContent = "Subagents";
const childrenRows = document.createElement("div"); childrenSection.append(childrenSummary, childrenRows);
form.before(childrenSection);
let readOnly = false;
let archiveReadOnly = false;
let parentId = "";
const childStates: Record<ChildInfo["state"], string> = { unknown: "Unknown", running: "Running", permission: "Waiting for permission", budget: "Out of steps", watchdog: "In doubt · watchdog paused", done: "Done", failed: "Failed", stopped: "Stopped" };
// The agent, then its state in the colour of what it asks of the Captain: running, waiting on
// him, finished, failed. The words stay, so colour is never the only signal.
function childLabel(child: ChildInfo): Node[] {
  const state = el("span", "child-state", childStates[child.state]);
  state.dataset.state = child.state;
  return [document.createTextNode(`${child.agent} · `), state];
}
function childActions(child: ChildInfo): HTMLElement {
  const actions = el("div", "child-actions");
  const boundPost = identity.bind(rawPost);
  for (const action of ["continue", "stop"] as const) {
    if (["unknown", "done", "failed", "stopped"].includes(child.state) || (action === "continue" && !["budget", "watchdog"].includes(child.state))) continue;
    const button = actionButton(action === "stop" ? "Stop" : "Continue", () => boundPost({ type: "controlChild", parentId, childId: child.id, action }));
    if (action === "stop") button.classList.add("control-danger");
    button.disabled = !!child.pending;
    actions.append(button);
  }
  return actions;
}
const palette = document.createElement("div");
palette.id = "command-palette";
palette.className = "command-palette";
const pickerSearch = document.createElement("input");
pickerSearch.className = "picker-search";
pickerSearch.type = "search";
pickerSearch.placeholder = "Search names and descriptions…";
pickerSearch.setAttribute("aria-label", "Search choices");
pickerSearch.setAttribute("role", "combobox");
pickerSearch.setAttribute("aria-autocomplete", "list");
pickerSearch.setAttribute("aria-controls", "picker-options");
const pickerCount = document.createElement("div");
pickerCount.className = "picker-count";
pickerCount.setAttribute("role", "status");
pickerCount.setAttribute("aria-live", "polite");
const pickerOptions = document.createElement("div");
pickerOptions.id = "picker-options";
pickerOptions.setAttribute("role", "listbox");
pickerOptions.setAttribute("aria-label", "Choices");
palette.append(pickerSearch, pickerCount, pickerOptions);
palette.hidden = true;
form.prepend(palette);
const commandError = document.createElement("div");
commandError.className = "command-error";
commandError.setAttribute("role", "alert");
commandError.hidden = true;
prompt.after(commandError);
const agentLabel = document.createElement("span");
agentLabel.className = "agent-label";
agentLabel.textContent = "build";
agentLabel.dataset.agent = "build";
const pendingModel = document.createElement("span");
pendingModel.className = "pending-model";
modelSelect.after(agentLabel, pendingModel);
let commands: CommandInfo[] = [];
let paletteRows: { name: string; description: string; args_hint?: string; gateway?: string; provider_group?: string }[] = [];
let pickerItems: typeof paletteRows = [];
let modelItems: typeof paletteRows = [];
let paletteIndex = 0;
let pickerKind: "model" | "variant" | "skills" | "agent" | undefined;
const collapsedGroups = new Set<string>();
// Favorite model names, kept by the host; the Favorites section lists those the catalogue still has.
let favorites = new Set<string>();
// After a favorite toggle the highlight follows the model: its row in the section if it is still
// there, else its row in its group.
let favoriteAnchor: { name: string; inSection: boolean } | undefined;
let favoriteRows: typeof paletteRows = [];
let selectorPicker = false;
let paletteDismissed = false;
function closePalette(): void {
  selectorPicker = false;
  pickerKind = undefined;
  palette.hidden = true;
  pickerSearch.setAttribute("aria-expanded", "false");
  pickerSearch.removeAttribute("aria-activedescendant");
  prompt.removeAttribute("aria-activedescendant");
  prompt.setAttribute("aria-expanded", "false");
}
function choosePalette(index: number): void {
  const row = paletteRows[index];
  if (!row) return;
  if (selectorPicker) post({ type: "selectModel", name: row.name });
  else if (pickerKind === "skills") prompt.value = `/skill ${row.name} `;
  else if (pickerKind) {
    const text = `/${pickerKind} ${row.name}`;
    prompt.value = text;
    post({ type: "send", text, includeContext: false });
  } else prompt.value = `/${row.name} `;
  pickerKind = undefined;
  selectorPicker = false;
  closePalette();
  prompt.focus();
  updateControls();
}
/** The canonical name without the `gateway/provider_group/` prefix the group headers show (`gateway/` for a direct gateway); the full name when the prefix does not match. */
function shortModelName(row: { name: string; gateway?: string; provider_group?: string }): string {
  if (!row.gateway) return row.name;
  const prefix = row.provider_group && row.provider_group !== row.gateway ? `${row.gateway}/${row.provider_group}/` : `${row.gateway}/`;
  return row.name.startsWith(prefix) && row.name.length > prefix.length ? row.name.slice(prefix.length) : row.name;
}
function renderPalette(): void {
  pickerOptions.replaceChildren();
  if (pickerKind === "model") pickerItems.sort((a,b) => (a.gateway ?? "").localeCompare(b.gateway ?? "") || (a.provider_group ?? "").localeCompare(b.provider_group ?? ""));
  const allMatches = ranked(pickerItems, pickerSearch.value, row => row.name, row => row.description);
  const matches = pickerKind === "model" && !pickerSearch.value ? allMatches.filter(row => !row.gateway || (!collapsedGroups.has(row.gateway) && !collapsedGroups.has(`${row.gateway}/${row.provider_group}`))) : allMatches;
  const groupRows = matches.slice(0, PICKER_LIMIT);
  // The Favorites section sits first, is never folded and is not part of the first-50 cut; its rows
  // come before the groups' rows in `paletteRows`, so indices and option ids stay unique.
  favoriteRows = pickerKind === "model" ? allMatches.filter(row => favorites.has(row.name)).sort((a, b) => a.name.localeCompare(b.name)) : [];
  paletteRows = [...favoriteRows, ...groupRows];
  pickerCount.textContent = `${allMatches.length} of ${pickerItems.length}${matches.length > PICKER_LIMIT ? " · Type to narrow (first 50 shown)" : ""}${pickerKind === "model" ? " · Alt+A folds or unfolds all · Ctrl+F favorite" : ""}`;
  if (favoriteAnchor) {
    const found = favoriteAnchor.inSection ? paletteRows.findIndex((row, i) => i < favoriteRows.length && row.name === favoriteAnchor!.name) : -1;
    const anywhere = found >= 0 ? found : paletteRows.findIndex((row, i) => i >= favoriteRows.length && row.name === favoriteAnchor!.name);
    if (anywhere >= 0) paletteIndex = anywhere;
    favoriteAnchor = undefined;
  }
  paletteIndex = Math.max(0, Math.min(paletteIndex, paletteRows.length - 1));
  palette.hidden = false;
  pickerSearch.setAttribute("aria-expanded", "true");
  pickerSearch.hidden = !pickerKind;
  if (paletteRows.length) pickerSearch.setAttribute("aria-activedescendant", `command-option-${paletteIndex}`);
  else { pickerSearch.removeAttribute("aria-activedescendant"); if (!allMatches.length) pickerOptions.append(el("p", "picker-empty", "No matches. Try another search.")); }
  if (!pickerKind) {
    prompt.setAttribute("role", "combobox");
    prompt.setAttribute("aria-autocomplete", "list");
    prompt.setAttribute("aria-controls", pickerOptions.id);
    prompt.setAttribute("aria-expanded", "true");
    if (paletteRows.length) prompt.setAttribute("aria-activedescendant", `command-option-${paletteIndex}`);
    else prompt.removeAttribute("aria-activedescendant");
  }

  // Gateway level 0, provider level 1, model level 2 (1 under a direct gateway); CSS indents by data-level.
  const group = (label: string, id: string, level: 0 | 1, gateway?: string): void => {
    const button = el("button", "picker-group", label); button.type = "button";
    button.dataset.level = String(level);
    button.setAttribute("aria-label", level === 0 ? `${label}, gateway` : `${label}, provider in ${gateway}`);
    button.setAttribute("aria-expanded", String(!collapsedGroups.has(id) || !!pickerSearch.value));
    button.addEventListener("click", () => { if (collapsedGroups.has(id)) collapsedGroups.delete(id); else collapsedGroups.add(id); renderPalette(); });
    pickerOptions.append(button);
  };
  const renderOption = (row: typeof paletteRows[number], index: number, level?: 1 | 2, inGroup = false): void => {
    const option = document.createElement("div");
    if (level !== undefined) option.dataset.level = String(level);
    option.id = `command-option-${index}`;
    option.className = "command-option";
    option.title = row.name;
    option.setAttribute("role", "option");
    // The group headers already show gateway and provider, so a row inside a group shows the
    // short name; the accessible name stays the full canonical name.
    if (inGroup) option.setAttribute("aria-label", row.name);
    option.setAttribute("aria-selected", String(index === paletteIndex));
    const name = document.createElement("strong");
    name.textContent = `${pickerKind ? "" : "/"}${inGroup ? shortModelName(row) : row.name}${row.args_hint ? ` ${row.args_hint}` : ""}`;
    const description = document.createElement("span");
    description.textContent = row.description;
    option.append(name, description);
    if (pickerKind === "model") {
      const starred = favorites.has(row.name);
      const star = el("button", "favorite-toggle", starred ? "★" : "☆"); star.type = "button";
      star.tabIndex = -1;
      star.setAttribute("aria-label", starred ? "Remove from favorites" : "Add to favorites");
      star.setAttribute("aria-pressed", String(starred));
      star.title = starred ? "Remove from favorites" : "Add to favorites";
      star.addEventListener("mousedown", event => event.preventDefault());
      star.addEventListener("click", event => { event.stopPropagation(); toggleFavorite(index); });
      option.append(star);
    }
    option.addEventListener("mousedown", event => event.preventDefault());
    option.addEventListener("click", () => choosePalette(index));
    pickerOptions.append(option);
  }
  if (pickerKind === "model") {
    if (favoriteRows.length) {
      const heading = el("h3", "picker-section", "Favorites");
      heading.dataset.level = "0";
      heading.setAttribute("aria-label", "Favorites, always open");
      pickerOptions.append(heading);
      favoriteRows.forEach((row, index) => renderOption(row, index, 1));
    }
    const gateways = [...new Set(allMatches.map(row => row.gateway ?? "Models"))];
    for (const gateway of gateways) {
      group(gateway, gateway, 0);
      if (!pickerSearch.value && collapsedGroups.has(gateway)) continue;
      const rows = allMatches.filter(row => (row.gateway ?? "Models") === gateway);
      for (const provider of [...new Set(rows.map(row => row.provider_group ?? gateway))]) {
        const id = `${gateway}/${provider}`;
        if (provider !== gateway) group(provider, id, 1, gateway);
        if (!pickerSearch.value && collapsedGroups.has(id)) continue;
        for (const row of rows.filter(row => (row.provider_group ?? gateway) === provider)) {
          const index = groupRows.indexOf(row);
          if (index >= 0) renderOption(row, favoriteRows.length + index, provider === gateway ? 1 : 2, true);
        }
      }
    }
  } else paletteRows.forEach((row, index) => renderOption(row, index));
  pickerOptions.querySelector(`#command-option-${paletteIndex}`)?.scrollIntoView?.({ block: "nearest" });
}
function filterPalette(): void {
  if (paletteDismissed || !/^\/[^\s/]*$/.test(prompt.value)) { closePalette(); return; }
  pickerKind = undefined;
  selectorPicker = false;
  pickerItems = commands;
  pickerSearch.value = prompt.value.slice(1);
  paletteIndex = 0;
  renderPalette();
  prompt.setAttribute("aria-controls", pickerOptions.id);
  prompt.setAttribute("aria-expanded", "true");
  if (paletteRows.length) prompt.setAttribute("aria-activedescendant", `command-option-${paletteIndex}`);
  else prompt.removeAttribute("aria-activedescendant");
}
// Asks the host to add or remove the model at `index`; the host answers with the new list.
function toggleFavorite(index: number): void {
  const row = paletteRows[index];
  if (!row || pickerKind !== "model") return;
  favoriteAnchor = { name: row.name, inSection: index < favoriteRows.length };
  paletteIndex = index;
  post({ type: "toggleFavorite", name: row.name });
}
pickerSearch.addEventListener("input", () => { paletteIndex = 0; renderPalette(); });
pickerSearch.addEventListener("keydown", event => {
  if (event.isComposing) return;
  if (event.key === "Escape") {
    event.preventDefault();
    if (pickerSearch.value) { pickerSearch.value = ""; paletteIndex = 0; renderPalette(); }
    else { paletteDismissed = true; closePalette(); prompt.focus(); }
  } else if (["ArrowUp", "ArrowDown"].includes(event.key)) {
    event.preventDefault();
    if (paletteRows.length) { paletteIndex = (paletteIndex + (event.key === "ArrowDown" ? 1 : -1) + paletteRows.length) % paletteRows.length; renderPalette(); }
  } else if (event.ctrlKey && !event.altKey && event.key.toLowerCase() === "f" && pickerKind === "model") {
    event.preventDefault();
    toggleFavorite(paletteIndex);
  } else if (event.altKey && event.code === "KeyA" && pickerKind === "model") {
    event.preventDefault();
    toggleAllGroups();
  } else if (event.key === "Enter" || event.key === "Tab") { event.preventDefault(); choosePalette(paletteIndex); }
});
// Folds every gateway and provider, or unfolds them all once everything is folded.
function toggleAllGroups(): void {
  const ids = pickerItems.flatMap(row => row.gateway ? [row.gateway, `${row.gateway}/${row.provider_group}`] : []);
  // A folded gateway hides its providers, so only gateways decide whether any group is open.
  if (pickerItems.every(row => !row.gateway || collapsedGroups.has(row.gateway))) collapsedGroups.clear();
  else for (const id of ids) collapsedGroups.add(id);
  renderPalette();
}
const usage = document.querySelector<HTMLElement>("#usage")!;
const usageFill = document.querySelector<HTMLElement>("#usage-fill")!;
const usageLabel = document.querySelector<HTMLElement>("#usage-label")!;
modelSelect.addEventListener("change", () => {
  if (modelSelect.value) post({ type: "selectModel", name: modelSelect.value });
});
modelSelect.addEventListener("mousedown", event => {
  if (modelSelect.disabled) return;
  event.preventDefault(); selectorPicker = true; pickerKind = "model"; pickerItems = modelItems; pickerSearch.value = ""; paletteIndex = 0; renderPalette(); pickerSearch.focus();
});
modelSelect.addEventListener("keydown", event => {
  if (!modelSelect.disabled && ["Enter", " ", "ArrowDown"].includes(event.key)) {
    event.preventDefault(); selectorPicker = true; pickerKind = "model"; pickerItems = modelItems; pickerSearch.value = ""; paletteIndex = 0; renderPalette(); pickerSearch.focus();
  }
});
includeContext.checked = vscode.getState()?.includeContext ?? true;
const conversations = mountConversations(message => post(message), () => {
  if (!prompt.disabled) prompt.focus();
  else document.querySelector<HTMLElement>(".conv-trigger")?.focus();
});
type TextSegment = { kind: "text"; stepId?: string; content: HTMLElement; reasoning: HTMLElement; details: HTMLDetailsElement; text: string; thoughts: string };
type ToolGroup = { kind: "tools"; details: HTMLDetailsElement; summary: HTMLElement; calls: Map<string, Extract<HostToWebview, { type: "tool" }>>; chosen: boolean; finished: boolean };
type Turn = { root: HTMLElement; last?: TextSegment | ToolGroup; calls: Map<string, HTMLElement>; groups: Map<string, ToolGroup> };
const turns = new Map<string, Turn>();
let state = "connecting";
let pendingQuestion = false;
let stickToBottom = true;
/** True while a history replay renders: only live wakes get the arrival cue. */
let replaying = false;
let sessionId = "";
let sending = false;
let pendingSendText: string | undefined;
let compacting = false;
let compactionRow: HTMLElement | undefined;
let lastConsumedDraft: string | undefined;
const permissionCards = new Map<string, { root: HTMLElement; outcome: HTMLElement; controls: HTMLElement; call: HTMLElement }>();
const questionCards = new Map<string, { root: HTMLElement; outcome: HTMLElement; answered: boolean }>();
const pauseCards = new Map<string, { root: HTMLElement; outcome: HTMLElement; controls: HTMLElement }>();

function updateControls(): void {
  prompt.disabled = readOnly || state === "connecting";
  if (compacting) prompt.placeholder = "Messages you send now are queued until compaction ends";
  else if (!readOnly) prompt.placeholder = "Message WithNav…";
  if (readOnly) modelSelect.disabled = true;
  send.disabled = prompt.disabled || sending || !prompt.value.trim();
  stop.hidden = readOnly || (state !== "busy" && !compacting);
}

function setStatus(next: string, message?: string): void {
  state = next;
  status.dataset.state = next;
  statusLabel.textContent = message ? `${next}: ${message}` : next;
  updateControls();
}

function follow(): void {
  if (stickToBottom) transcript.scrollTop = transcript.scrollHeight;
}
transcript.addEventListener("scroll", () => {
  stickToBottom = transcript.scrollHeight - transcript.scrollTop - transcript.clientHeight < 72;
});

function add(element: HTMLElement): void {
  for (const item of turns.values()) {
    finishGroup(item);
    item.last = undefined;
  }
  empty.hidden = true;
  transcript.append(element);
  follow();
}

function el<K extends keyof HTMLElementTagNameMap>(tag: K, className?: string, text?: string): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function decorateCrew(byline: HTMLElement, agent = "build"): void {
  const aliases: Record<string, string> = { build: "nav", plan: "nav", explore: "vigie", librarian: "vigie", review: "lux", oracle: "lux", explorer: "mousse" };
  const id = aliases[agent] ?? agent;
  const names: Record<string, string> = { captain: "Captain", nav: "Nav · The Navigator", vigie: "Vigie · The Lookout", lux: "Lux · The Expert Advisor", mousse: "Mousse · The Lightweight Deckhand", cartographe: "Cartographe · The Cartographer", sondeur: "Sondeur · The Deep Diver" };
  byline.textContent = names[id] ?? agent;
  if (names[id]) {
    byline.dataset.crew = id;
    const glyph = el("span", "crew-glyph");
    glyph.setAttribute("aria-hidden", "true");
    byline.prepend(glyph);
  }
}

function message(author: string, text: string, context?: string, skill?: { name: string; origin: string }, source?: string): void {
  if ((author === "navigator" || author === "nav")) { add(navNote(text)); return; }
  const root = el("article", `message ${author === "driver" ? "captain" : author}`);
  if (author === "cognition") root.dataset.agent = agentLabel.dataset.agent;
  const byline = el("div", "byline", (author === "driver" || author === "captain") ? pendingQuestion ? "Captain · Answer" : "Captain" : author === "cognition" ? "Cognition" : "Subagent output · untrusted data");
  if (author === "cognition") decorateCrew(byline, agentLabel.dataset.agent);
  if (author === "driver" || author === "captain") decorateCrew(byline, "captain");
  if (author === "subagent" && source) { const id = source.split(":").at(-1)!; if (["vigie", "lux", "mousse", "cartographe", "sondeur", "explore", "librarian", "review", "oracle", "explorer"].includes(id)) { decorateCrew(byline, id); byline.append(el("span", "skill-note", "Untrusted data")); } }
  if (author === "subagent" && source) byline.title = `Source: ${source}`;
  if (skill) {
    const note = el("span", "skill-note", `skill: ${skill.name}`);
    note.title = `Selected skill · ${skill.origin}`;
    byline.append(note);
  }
  root.append(byline);
  const body = el("div", "message-body");
  renderMarkdown(body, text);
  root.append(body);
  if (context) {
    const chip = el("div", "context-chip sent", context);
    chip.title = "Editor context sent with this message";
    root.append(chip);
  }
  add(root);
  if (pendingQuestion && (author === "driver" || author === "captain")) pendingQuestion = false;
}

// A wake: Nav glancing over the Captain's shoulder. One quiet
// summary, the change and the intent behind a disclosure.
let noteCount = 0;
function navNote(text: string): HTMLElement {
  const wake = parseWake(text);
  const root = el("article", "message note");
  if (!replaying) root.classList.add("arrived");
  const head = el("div", "note-head");
  head.append(navMark());
  const line = el("div", "note-line");
  line.append(el("span", "note-label", "Beacon · Nav · metacognition → nav"));
  if (wake.reason) line.append(el("span", "note-reason", wake.reason));
  head.append(line);
  root.append(head);

  const meta = el("div", "note-meta");
  if (wake.path) {
    const link = el("button", "file-link note-file", displayPath(wake.path) + (wake.line ? `:${wake.line}` : ""));
    link.type = "button";
    link.title = `Open ${wake.path}${wake.line ? ` at line ${wake.line}` : ""}`;
    const target = { path: wake.path, line: wake.line };
    link.addEventListener("click", () => post({ type: "openFile", ...target }));
    meta.append(link);
  }
  if (wake.diff) meta.append(diffCount(wake.diff));
  if (wake.score) {
    const score = el("span", "note-score", wake.score);
    score.title = "Score, from 0 to 1";
    meta.append(score);
  }

  const details = el("div", "note-details");
  details.id = `note-${++noteCount}`;
  details.hidden = true;
  if (wake.lead) details.append(el("p", "note-lead", wake.lead));
  if (wake.diff) details.append(diffBlock(wake.diff));
  if (wake.intent) {
    const intent = el("section", "note-intent");
    intent.append(el("div", "note-section", "Intent on record"));
    const body = el("div", "message-body");
    renderMarkdown(body, wake.intent);
    intent.append(body);
    details.append(intent);
  }
  if (wake.rest) {
    const body = el("div", "message-body note-rest");
    renderMarkdown(body, wake.rest);
    details.append(body);
  }
  if (details.childElementCount) {
    const toggle = el("button", "note-toggle");
    toggle.type = "button";
    toggle.setAttribute("aria-expanded", "false");
    toggle.setAttribute("aria-controls", details.id);
    const label = el("span", "", wake.diff ? "Show change" : "Show details");
    toggle.append(chevron(), label);
    toggle.addEventListener("click", () => {
      const open = details.hidden;
      details.hidden = !open;
      toggle.setAttribute("aria-expanded", String(open));
      label.textContent = open ? "Hide" : wake.diff ? "Show change" : "Show details";
      if (open) follow();
    });
    meta.append(toggle);
  }
  if (meta.childElementCount) root.append(meta);
  root.append(details);
  return root;
}

/** A relative path stays whole; an absolute one keeps its last two parts. */
function displayPath(path: string): string {
  if (!/^(\/|[A-Za-z]:[\\/])/.test(path)) return path;
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts.length > 2 ? `…/${parts.slice(-2).join("/")}` : path;
}

const SVG = "http://www.w3.org/2000/svg";
function strong(children: Node[]): HTMLElement {
  const node = el("strong");
  node.append(...children);
  return node;
}

function svgIcon(className: string, d: string): SVGSVGElement {
  const svg = document.createElementNS(SVG, "svg");
  svg.setAttribute("viewBox", "0 0 16 16");
  svg.setAttribute("aria-hidden", "true");
  svg.setAttribute("class", className);
  const path = document.createElementNS(SVG, "path");
  path.setAttribute("d", d);
  svg.append(path);
  return svg;
}
const navMark = (): SVGSVGElement => svgIcon("note-mark", "M8 2.5 12.5 13 8 10.4 3.5 13Z");
const chevron = (): SVGSVGElement => svgIcon("note-chevron", "M6 4l4 4-4 4");

function codeBlock(label: string, value: string, fill: (code: HTMLElement) => void): HTMLElement {
  const wrap = el("div", "code-block");
  const bar = el("div", "code-bar");
  bar.append(el("span", "", label));
  const copy = el("button", "copy", "Copy");
  copy.type = "button";
  copy.addEventListener("click", async () => {
    try { await navigator.clipboard.writeText(value); copy.textContent = "Copied"; }
    catch { copy.textContent = "Copy failed"; }
  });
  bar.append(copy);
  const pre = el("pre");
  const code = el("code");
  fill(code);
  pre.append(code);
  wrap.append(bar, pre);
  return wrap;
}

// `+3 −1`: the size of the change a wake is about, in the editor's git colours.
function diffCount(diff: string): HTMLElement {
  const kinds = diff.split("\n").map(diffKind);
  const added = kinds.filter(kind => kind === "add").length;
  const removed = kinds.filter(kind => kind === "del").length;
  const count = el("span", "diff-count");
  count.title = `${added} ${added === 1 ? "line" : "lines"} added, ${removed} removed`;
  count.append(el("span", "diff-added", `+${added}`), el("span", "diff-removed", `−${removed}`));
  return count;
}

function diffBlock(diff: string): HTMLElement {
  const block = codeBlock("Diff", diff, code => {
    for (const text of diff.split("\n")) code.append(el("span", `diff-line ${diffKind(text)}`, text || " "));
  });
  block.classList.add("diff");
  return block;
}

function turn(id: string): Turn {
  const existing = turns.get(id);
  if (existing) return existing;
  const root = el("article", "message cognition");
  root.dataset.agent = agentLabel.dataset.agent;
  const byline = el("div", "byline"); decorateCrew(byline, agentLabel.dataset.agent); root.append(byline);
  const item: Turn = { root, calls: new Map(), groups: new Map() };
  turns.set(id, item);
  add(root);
  return item;
}

function finishGroup(item: Turn): void {
  if (item.last?.kind !== "tools") return;
  item.last.finished = true;
  if (!item.last.chosen) item.last.details.open = false;
}

function continueTurn(item: Turn): void {
  if (transcript.lastElementChild === item.root) return;
  const root = el("article", "message cognition");
  root.dataset.agent = item.root.dataset.agent;
  item.root = root;
  add(root);
}

function textSegment(item: Turn, stepId?: string): TextSegment {
  continueTurn(item);
  if (item.last?.kind === "text" && item.last.stepId === stepId) return item.last;
  finishGroup(item);
  const content = el("div", "message-body");
  const details = el("details", "reasoning");
  details.append(el("summary", "", "Reasoning"));
  const reasoning = el("div", "reasoning-body");
  details.append(reasoning);
  details.hidden = true;
  item.root.append(details, content);
  const segment: TextSegment = { kind: "text", stepId, content, reasoning, details, text: "", thoughts: "" };
  item.last = segment;
  return segment;
}

function toolPath(summary: string): { path: string; line?: number } | undefined {
  // Only file tools get a link. A shell command is shown as plain text: the
  // first path-looking word of `cat -n src/a.rs | …` is not what it operates
  // on in general, and linking it split the row into misleading pieces.
  const match = /^(?:read|edit|write)\s+([\w./\\-]+)(?::(\d+))?$/i.exec(summary);
  return match ? { path: match[1], line: match[2] ? Number(match[2]) : undefined } : undefined;
}

function activity(item: Turn, event: Extract<HostToWebview, { type: "tool" }>): void {
  let row = item.calls.get(event.callId);
  if (!row) {
    continueTurn(item);
    if (item.last?.kind !== "tools") {
      const details = el("details", "tool-group");
      const summary = el("summary");
      details.append(summary);
      const group: ToolGroup = { kind: "tools", details, summary, calls: new Map(), chosen: false, finished: false };
      // Native summary activation records intent, unlike asynchronous toggle events
      // which also fire when streaming updates change `open` programmatically.
      summary.addEventListener("click", () => { group.chosen = true; });
      item.root.append(details);
      item.last = group;
    }
    row = el("div", "tool-row");
    item.calls.set(event.callId, row);
    item.groups.set(event.callId, item.last);
    item.last.details.append(row);
  }
  const group = item.groups.get(event.callId)!;
  group.calls.set(event.callId, event);
  const calls = [...group.calls.values()];
  const failed = calls.filter(call => call.phase === "end" && call.ok === false).length;
  const running = calls.find(call => call.phase === "start");
  group.summary.replaceChildren(el("span", "", `${calls.length} tool calls`));
  if (failed) group.summary.append(el("span", "tool-group-failed", ` · ${failed} failed`));
  if (running) group.summary.append(el("span", "tool-group-running", ` · running ${running.summary || running.name}`));
  if (!group.chosen) group.details.open = !replaying && !group.finished;
  row.replaceChildren();
  row.className = `tool-row ${event.phase === "start" ? "working" : event.ok === false ? "failed" : "complete"}`;
  row.append(el("span", "tool-mark", event.phase === "start" ? "◌" : event.ok === false ? "×" : "✓"));
  const summary = event.summary || event.name;
  const target = toolPath(summary);
  if (target) {
    const at = summary.indexOf(target.path);
    // A span with preserved whitespace: a bare text node in the flex row
    // lost its trailing space and glued "read" to the path.
    row.append(el("span", "tool-verb", summary.slice(0, at)));
    const link = el("button", "file-link", target.path + (target.line ? `:${target.line}` : ""));
    link.type = "button";
    link.addEventListener("click", () => post({ type: "openFile", ...target }));
    row.append(link);
  } else {
    const text = el("span", "tool-text");
    const name = summary.startsWith(event.name) ? event.name : "";
    text.append(el("span", "tool-verb", name), document.createTextNode(summary.slice(name.length)));
    row.append(text);
  }
  row.title = summary;
  row.setAttribute("aria-label", `${event.phase === "start" ? "Running" : event.ok === false ? "Failed" : "Finished"}: ${summary}`);
  if (event.evidence) row.append(evidenceDetails(event.evidence));
  follow();
}

function evidenceDetails(evidence: PermissionEvidence, context: "tool" | "permission" = "tool"): HTMLElement {
  const details = el("details", "approval-evidence");
  const label = evidenceLabel(evidence, context);
  details.append(el("summary", "", label));
  details.setAttribute("aria-label", `Permission review: ${label}`);
  const body = el("div", "evidence-body");
  if (evidence.reason) body.append(el("p", "", evidence.reason));
  body.append(el("div", "", `Policy version ${evidence.policy_version}`));
  for (const key of ["verdicts", "overrides", "models", "latency_ms", "truncated"] as const) {
    if (evidence[key] === undefined) continue;
    const value = typeof evidence[key] === "object" ? JSON.stringify(evidence[key], null, 2) : String(evidence[key]);
    body.append(el("div", "evidence-field", `${key}: ${value}`));
  }
  details.append(body);
  return details;
}

function actionButton(label: string, action: () => void, primary = false): HTMLButtonElement {
  const button = el("button", primary ? "control-primary" : "control-secondary", label);
  button.type = "button";
  button.addEventListener("click", action);
  return button;
}

function renderQuestion(event: Extract<HostToWebview, { type: "question" }>, parent: HTMLElement): void {
  const choices = event.choices!;
  const id = event.questionCallId!;
  if (questionCards.has(id)) return;
  const post = identity.bind(rawPost);
  const root = el("article", "control-card question-card");
  root.setAttribute("aria-label", choices.header ?? "Question");
  root.append(el("div", "control-kicker", choices.header ?? "Question"));
  const body = el("div", "message-body");
  renderMarkdown(body, event.text);
  root.append(body);
  const selected = new Set<number>();
  const custom = el("input", "deny-message");
  custom.placeholder = "Other answer…";
  custom.setAttribute("aria-label", "Other answer…");
  const options = el("div", "question-options");
  const outcome = el("div", "control-outcome", readOnly ? "Waiting for an answer in the parent conversation." : "Waiting for your answer");
  outcome.setAttribute("role", "status");
  const send = actionButton("Send", () => submit([...selected]), true);
  const refresh = () => {
    for (const [index, button] of Array.from(options.querySelectorAll("button")).entries()) {
      button.classList.toggle("selected", selected.has(index));
      button.setAttribute("aria-pressed", String(selected.has(index)));
      const check = button.querySelector(".question-check");
      if (check) check.textContent = selected.has(index) ? "☑" : "☐";
    }
    send.disabled = readOnly || (!selected.size && !custom.value.trim());
  };
  const submit = (indices: number[]) => {
    if (!sessionId || readOnly) return;
    for (const control of root.querySelectorAll<HTMLButtonElement | HTMLInputElement>("button,input")) control.disabled = true;
    outcome.textContent = "Sending…";
    post({ type: "answerQuestion", sessionId, answer: { question_call_id: id, selected: indices, ...(custom.value.trim() ? { custom: custom.value.trim() } : {}) } });
  };
  choices.options.forEach((option, index) => {
    const button = actionButton("", () => {
      if (choices.multiple) { if (selected.has(index)) selected.delete(index); else selected.add(index); refresh(); }
      else { selected.clear(); selected.add(index); refresh(); submit([index]); }
    });
    button.classList.add("question-option");
    button.disabled = readOnly;
    button.setAttribute("aria-pressed", "false");
    if (choices.multiple) button.append(el("span", "question-check", "☐"));
    const text = el("span", "question-option-text");
    text.append(el("span", "", option.label));
    if (choices.recommended === index) text.append(el("span", "control-meta", " · Recommended"));
    if (option.description) text.append(el("span", "question-description", option.description));
    button.append(text);
    options.append(button);
  });
  root.append(options);
  if (choices.allow_custom) {
    custom.disabled = readOnly;
    custom.addEventListener("input", refresh);
    custom.addEventListener("keydown", e => { if (e.key === "Enter" && custom.value.trim()) { e.preventDefault(); submit([...selected]); } });
    root.append(custom);
  }
  if (choices.multiple || choices.allow_custom) { const actions = el("div", "control-actions"); actions.append(send); root.append(actions); }
  refresh();
  root.append(outcome);
  questionCards.set(id, { root, outcome, answered: false });
  parent.append(root);
}

function renderPermission(event: Extract<HostToWebview, { type: "permission" }>): void {
  if (readOnly) {
    const note = el("article", "control-card");
    note.append(el("h3", "control-title", `Permission needed · ${event.card.agent}`), el("p", "permission-reason", event.card.reason));
    note.append(el("p", "", `Review this permission in the parent conversation (${parentId}).`));
    add(note); return;
  }
  const post = identity.bind(rawPost);
  const card = event.card;
  const existing = permissionCards.get(card.requestId);
  if (existing) {
    existing.call.replaceChildren(codeBlock("Call to review", card.call, code => { code.textContent = card.call; }));
    follow();
    return;
  }
  const root = el("article", "control-card permission-card");
  root.setAttribute("aria-label", `Permission: ${card.agent} wants to run ${card.tool}`);
  root.append(el("div", "control-kicker", "Permission needed"), el("h3", "control-title", `${card.agent} wants to run ${card.tool}`));
  if (card.childSessionId) root.append(el("div", "control-meta", `Subagent · ${card.childSessionId}`));
  root.append(el("p", "control-meta", `${card.scope.kind}: ${card.scope.value}${card.tool === "bash" ? " · working directory, not confinement" : ""}`));
  root.append(el("p", "permission-reason", card.reason));
  const call = el("div", "permission-call");
  call.append(codeBlock("Call to review", card.call, code => { code.textContent = card.call; }));
  root.append(call);
  if (card.evidence) root.append(evidenceDetails(card.evidence, "permission"));
  const controls = el("div", "permission-controls");
  const denyMessage = el("textarea", "deny-message");
  denyMessage.rows = 2;
  denyMessage.placeholder = "Reject message (optional)";
  denyMessage.setAttribute("aria-label", `Optional reject message for ${card.tool}`);
  const messageFeedback = el("div", "control-meta", "0 / 2048 UTF-8 bytes");
  messageFeedback.setAttribute("role", "status");
  let previousMessage = "";
  denyMessage.addEventListener("input", () => {
    const bytes = new TextEncoder().encode(denyMessage.value).length;
    if (bytes > 2048) {
      denyMessage.value = previousMessage;
      messageFeedback.textContent = "Reject message must be at most 2048 UTF-8 bytes";
    } else {
      previousMessage = denyMessage.value;
      messageFeedback.textContent = `${bytes} / 2048 UTF-8 bytes`;
    }
  });
  const buttons = el("div", "control-actions");
  const answer = (decision: PermissionDecision): void => {
    if (!sessionId) return;
    if (decision === "reject" && new TextEncoder().encode(denyMessage.value).length > 2048) {
      messageFeedback.textContent = "Reject message must be at most 2048 UTF-8 bytes";
      return;
    }
    for (const button of buttons.querySelectorAll("button")) button.disabled = true;
    denyMessage.disabled = true;
    outcome.textContent = "Sending answer…";
    post({ type: "answerPermission", sessionId, requestId: card.requestId, decision,
      ...(decision === "reject" && denyMessage.value.trim() ? { message: denyMessage.value.trim() } : {}) });
  };
  buttons.append(actionButton("Once", () => answer("allow_once"), true));
  if (card.alwaysLabel !== undefined) buttons.append(actionButton(card.alwaysLabel, () => answer("allow_always")));
  buttons.append(actionButton("Reject", () => answer("reject")));
  controls.append(denyMessage, messageFeedback, buttons);
  const outcome = el("div", "control-outcome", "Waiting for your answer");
  outcome.setAttribute("role", "status");
  root.append(controls, outcome);
  permissionCards.set(card.requestId, { root, controls, outcome, call });
  add(root);
}

function renderPause(event: Extract<HostToWebview, { type: "pause" }>): void {
  if (readOnly) { add(el("article", "message control-notice", event.text)); return; }
  const post = identity.bind(rawPost);
  if (pauseCards.has(event.id)) return;
  const root = el("article", "control-card pause-card");
  root.dataset.kind = event.kind;
  root.append(el("div", "control-kicker", event.kind === "budget" ? "Step limit reached" : "Watchdog pause"),
    el("h3", "control-title", event.text));
  const controls = el("div", "control-actions");
  const outcome = el("div", "control-outcome");
  outcome.setAttribute("role", "status");
  const control = (action: "continue" | "stop"): void => {
    if (!sessionId) return;
    for (const button of controls.querySelectorAll("button")) button.disabled = true;
    outcome.textContent = action === "continue" ? "Continuing…" : "Stopping…";
    post({ type: "controlPause", sessionId, id: event.id, action });
  };
  const resume = actionButton("Continue", () => control("continue"), true);
  if (event.kind === "watchdog" && !event.token) {
    resume.disabled = true;
    outcome.textContent = "Pause token unavailable. Send a new message or stop instead.";
  }
  controls.append(resume);
  if (event.kind === "watchdog") {
    const stop = actionButton("Stop", () => control("stop"));
    stop.classList.add("control-danger");
    controls.append(stop);
  }
  root.append(controls, outcome);
  pauseCards.set(event.id, { root, controls, outcome });
  add(root);
  setStatus("ready");
}

function receive(event: HostToWebview): void {
  if (!identity.receive(event)) return;
  if (event.type === "conversationOpened") {
    renderBypass(false);
    form.hidden = false;
    archiveReadOnly = false;
    compacting = false; compactionRow = undefined;
    usage.removeAttribute("aria-busy"); usage.removeAttribute("data-updating");
    readOnly = false; parentId = ""; childHeader.hidden = true; childrenSection.hidden = true;
    sessionId = event.id ?? "";
    sending = false;
    pendingSendText = undefined;
    lastConsumedDraft = undefined;
    closePalette();
    commandError.hidden = true;
    agentLabel.textContent = "build";
    agentLabel.dataset.agent = "build";
    pendingModel.textContent = "";
  }
  if (conversations(event)) return;
  switch (event.type) {
    case "bypass":
      bypassToggle.disabled = event.disabled || event.pending || event.yolo === true;
      renderBypass(event.enabled, event.pending, event.yolo === true, event.permissionsUnavailable);
      break;
    case "compaction": {
      compacting = event.pending;
      if (!compactionRow || !compactionRow.isConnected) { compactionRow = el("article", "message compaction-progress"); add(compactionRow); }
      compactionRow.textContent = event.text;
      compactionRow.dataset.pending = String(event.pending);
      compactionRow.dataset.operationId = event.operationId;
      compactionRow.setAttribute("role", "status");
      compactionRow.classList.toggle("command-error", event.ok === false);
      usage.setAttribute("aria-busy", String(event.pending));
      usage.dataset.updating = String(event.pending);
      updateControls(); break;
    }
    case "subagents": {
      parentId = event.parentId;
      childrenRows.replaceChildren();
      childrenSection.hidden = !event.items.length || readOnly;
      childrenSummary.textContent = `Subagents · ${event.items.length}`;
      if (event.items.length) childrenSection.open = true;
      const boundPost = identity.bind(rawPost);
      for (const child of event.items) {
        const row = el("article", "child-row");
        const open = actionButton("", () => boundPost({ type: "openChild", parentId: event.parentId, childId: child.id }));
        open.classList.add("child-open");
        open.append(...childLabel(child));
        const heading = el("div", "child-row-heading");
        heading.append(open, childActions(child));
        const goal = el("p", "child-goal", child.goal);
        goal.title = child.goal;
        row.append(heading, goal);
        // Delivery routing is protocol metadata, not a status for the Captain.
        if (child.reason) row.append(el("p", "child-reason", child.reason));
        if (child.error) row.append(el("p", "command-error", child.error));
        childrenRows.append(row);
      }
      break;
    }
    case "viewMode": {
      archiveReadOnly = !!event.archiveReason;
      readOnly = event.readOnly;
      agentSelect.disabled = readOnly;
      if (event.archiveReason) { childHeader.hidden = false; childHeader.replaceChildren(el("strong", "", "Read-only archive"), el("p", "", event.archiveReason)); form.hidden = true; }
      for (const control of document.querySelectorAll<HTMLButtonElement | HTMLTextAreaElement>(".control-card button, .control-card textarea")) control.disabled = readOnly;
      updateControls(); break;
    }
    case "childView": {
      if (archiveReadOnly) break;
      readOnly = !!event.child;
      if (event.child) parentId = event.parentId ?? "";
      childHeader.replaceChildren(); childHeader.hidden = !readOnly;
      if (event.child) {
        agentSelect.disabled = true;
        childHeader.append(el("p", "", `Read-only · subagent of ${event.parentTitle ?? "parent conversation"}`),
          strong(childLabel(event.child)), el("p", "", "Read-only subagent transcript. Send messages and answer permissions in the parent."), childActions(event.child));
        if (event.child.error) childHeader.append(el("p", "command-error", event.child.error));
        childrenSection.hidden = true;
      }
      form.hidden = readOnly;
      prompt.placeholder = readOnly ? "Read-only · return to parent to send" : "Message WithNav…";
      closePalette(); updateControls(); break;
    }
    case "commands": commands = event.items; filterPalette(); break;
    case "commandError": commandError.textContent = event.text; commandError.hidden = false; updateControls(); break;
    case "agent": agentLabel.textContent = event.name; agentLabel.dataset.agent = event.name; break;
    case "assignments": {
      if (event.models) renderModels({ type: "models", items: event.models, current: event.model, locked: !!event.locked });
      agentSelect.replaceChildren(...event.agents.map(agent => { const option = el("option", "", agent.id); option.value = agent.id; option.title = `${agent.description} · ${agent.effective?.model ?? "Unassigned"}${agent.warning ? ` · ${agent.warning}` : ""}`; return option; }));
      agentSelect.value = event.current; agentSelect.disabled = readOnly || !!event.locked;
      agentLabel.textContent = `${event.current}${event.source === "override" ? " · model override" : ""}${event.variant_source === "override" ? " · variant override" : ""}`;
      agentLabel.dataset.agent = event.current;
      const types: Record<string,string> = { reflex: "FIM · editor", instinct: "SystemOne", metacognition: "Chat" };
      const tooltip = [`Cognition · ${event.current}: ${event.model ?? "Unassigned"}${event.variant === null ? " · variant off" : event.variant ? ` · ${event.variant}` : ""} · model source: ${event.source ?? "assignment"} · variant source: ${event.variant_source ?? "assignment"}`, ...event.layers.map(layer => `${layer.layer} (${types[layer.layer]}): ${layer.model ?? "Unassigned"}${layer.variant ? ` · ${layer.variant}` : ""} · ${layer.status}${layer.reason ? ` — ${layer.reason}` : ""}`), "Usable describes configuration, not network availability. Assignments are read-only."].join("\n");
      document.querySelector<HTMLElement>(".topbar")!.title = tooltip; agentLabel.title = tooltip;
      break;
    }
    case "modelPending": pendingModel.textContent = event.name ? `→ ${event.name} · pending after turn` : ""; break;
    case "commandPicker":
      sending = false;
      if (event.disabledReason) { commandError.textContent = event.disabledReason; commandError.hidden = false; closePalette(); }
      else { selectorPicker = false; pickerKind = event.kind; pickerItems = event.items; pickerSearch.value = ""; paletteIndex = 0; renderPalette(); pickerSearch.focus(); }
      updateControls();
      break;
    case "history":
      compactionRow = undefined;
      if (!event.items.length) usage.hidden = true;
      transcript.replaceChildren(empty);
      empty.hidden = false;
      turns.clear();
      permissionCards.clear();
      questionCards.clear();
      pauseCards.clear();
      pendingQuestion = false;
      stickToBottom = true;
      replaying = true;
      try { for (const item of event.items) if (item.type !== "history") receive(item); }
      finally { replaying = false; }
      if (readOnly) { agentSelect.disabled = true; modelSelect.disabled = true; for (const control of document.querySelectorAll<HTMLButtonElement | HTMLTextAreaElement>(".control-card button, .control-card textarea")) control.disabled = true; }
      if (state === "connecting") setStatus("ready");
      follow();
      break;
    case "status": setStatus(event.state, event.message); break;
    case "message": {
      if (event.answer) {
        const card = questionCards.get(event.answer.question_call_id);
        if (card) {
          card.answered = true;
          for (const [index, button] of Array.from(card.root.querySelectorAll<HTMLButtonElement>(".question-option")).entries()) {
            button.classList.toggle("selected", event.answer.selected.includes(index));
            button.setAttribute("aria-pressed", String(event.answer.selected.includes(index)));
            const check = button.querySelector(".question-check");
            if (check) check.textContent = event.answer.selected.includes(index) ? "☑" : "☐";
          }
          for (const control of card.root.querySelectorAll<HTMLButtonElement | HTMLInputElement>("button,input")) control.disabled = true;
          card.outcome.textContent = `Answered${event.answer.custom ? ` · ${event.answer.custom}` : ""}`;
        }
      }
      message(event.author, event.text, event.context, event.selectedSkill, event.source); break;
    }
    case "questionState": {
      const card = questionCards.get(event.questionCallId);
      if (card && !card.answered) {
        for (const control of card.root.querySelectorAll<HTMLButtonElement | HTMLInputElement>("button,input")) control.disabled = readOnly;
        card.outcome.textContent = event.error;
      }
      break;
    }
    case "sendResult":
      if (pendingSendText === event.text) { sending = false; pendingSendText = undefined; }
      if (event.accepted && prompt.value === event.text) { lastConsumedDraft = event.text; prompt.value = ""; }
      if (!event.accepted && event.restoreDraft && !prompt.value && lastConsumedDraft === event.text) {
        prompt.value = event.text;
        lastConsumedDraft = undefined;
      }
      updateControls();
      break;
    case "permission": renderPermission(event); break;
    case "permissionCall": {
      const card = permissionCards.get(event.requestId);
      if (card) card.call.replaceChildren(codeBlock("Call to review", event.call, code => { code.textContent = event.call; }));
      follow();
      break;
    }
    case "permissionState": {
      const card = permissionCards.get(event.requestId);
      if (!card) break;
      card.outcome.textContent = permissionOutcome(event.resolution, event.message);
      card.root.dataset.resolution = event.resolution;
      const review = card.root.querySelector(".approval-evidence summary");
    if (review?.textContent === "Captain review required" && ["allow_once", "allow_always", "allow_folder"].includes(event.resolution)) review.textContent = "approved · Captain";
      card.controls.hidden = !["pending", "error"].includes(event.resolution);
      for (const control of card.controls.querySelectorAll<HTMLButtonElement | HTMLTextAreaElement>("button, textarea")) control.disabled = readOnly || event.resolution === "pending";
      follow();
      break;
    }
    case "pause": renderPause(event); break;
    case "pauseState": {
      const card = pauseCards.get(event.id);
      if (!card) break;
      card.outcome.textContent = event.message ?? (event.state === "pending" ? "Waiting for the server…" : "No longer paused");
      card.controls.hidden = event.state === "resolved";
      card.root.dataset.state = event.state;
      for (const button of card.controls.querySelectorAll("button")) button.disabled = readOnly || event.state === "pending";
      follow();
      break;
    }
    case "notice": {
      const notice = el("article", "message control-notice", event.text);
      if (event.operationId) { notice.dataset.operationId = event.operationId; notice.title = `Operation: ${event.operationId}`; }
      if (event.ok === false) notice.classList.add("command-error");
      if (event.details !== undefined) {
        const details = el("details");
        details.append(el("summary", "", "Details"), el("pre", "", event.details));
        notice.append(details);
      }
      add(notice);
      break;
    }
    case "models": renderModels(event); break;
    case "favorites": {
      favorites = new Set(event.items.filter(name => typeof name === "string"));
      if (!palette.hidden && pickerKind === "model") renderPalette(); else favoriteAnchor = undefined;
      break;
    }
    case "usage": renderUsage(event.snapshot); break;
    case "editorContext":
      contextPreview.textContent = event.label;
      contextPreview.title = `Attached to your next message: ${event.label}`;
      break;
    case "turnStart": turn(event.id); setStatus("busy"); break;
    case "delta": {
      const item = textSegment(turn(event.id), event.stepId);
      item.text += event.text;
      if (!pendingMarkdown.has(item.content)) {
        pendingMarkdown.add(item.content);
        requestAnimationFrame(() => {
          pendingMarkdown.delete(item.content);
          renderMarkdown(item.content, item.text);
          follow();
        });
      }
      break;
    }
    case "reasoning": {
      const item = textSegment(turn(event.id), event.stepId);
      item.thoughts += event.text;
      item.details.hidden = false;
      item.reasoning.textContent = item.thoughts;
      follow();
      break;
    }
    case "tool": activity(turn(event.id), event); break;
    case "question": {
      const item = turn(event.id);
      continueTurn(item);
      finishGroup(item);
      item.last = undefined;
      if (event.choices && event.questionCallId) {
        renderQuestion(event, item.root);
        pendingQuestion = true; setStatus("ready"); follow(); break;
      }
      const note = el("div", "question");
      note.append(el("span", "question-label", "Question"));
      const body = el("div", "message-body");
      renderMarkdown(body, event.text);
      note.append(body);
      item.root.append(note);
      pendingQuestion = true;
      setStatus("ready");
      follow();
      break;
    }
    case "turnEnd": {
      const item = turn(event.id);
      finishGroup(item);
      if (event.outcome !== "done") item.root.append(el("div", "turn-result", event.error || (event.outcome === "stopped" ? "Stopped" : "The turn failed.")));
      setStatus(event.outcome === "error" ? "error" : "ready", event.error);
      follow();
      break;
    }
  }
}

/** Rebuilds the model picker; a locked picker shows the open conversation's model. */
function renderModels(event: Extract<HostToWebview, { type: "models" }>): void {
  modelItems = event.items.filter(model => model.api === "chat" && model.enabled !== false).map(model => ({ name: model.name, description: model.upstream_id, gateway: model.gateway, provider_group: model.provider_group })).sort((a,b) => a.gateway.localeCompare(b.gateway) || a.provider_group.localeCompare(b.provider_group));
  const options = event.items.map(model => {
    const option = el("option", undefined, model.name);
    option.value = model.name;
    option.title = model.contextWindow ? `${model.name} · ${tokens(model.contextWindow)} context` : model.name;
    return option;
  });
  if (event.current && !event.items.some(model => model.name === event.current)) {
    const option = el("option", undefined, event.current);
    option.value = event.current;
    options.push(option);
  }
  if (!options.length) options.push(el("option", undefined, "Default model"));
  modelSelect.replaceChildren(...options);
  if (event.current) modelSelect.value = event.current;
  modelSelect.disabled = readOnly || event.locked || event.items.length === 0;
  modelSelect.title = "Switch model · running turns finish before the switch";
}

/** The next request's estimate, against the usable input budget. */
function tokens(count: number): string {
  return count >= 1000 ? `${Math.round(count / 1000)}k` : String(count);
}

function renderUsage(snapshot: import("../contextMeter").ContextSnapshot): void {
  const meter = contextMeter(snapshot);
  usage.hidden = false;
  usageFill.style.width = `${meter.fill}%`;
  usage.dataset.level = meter.level;
  usageLabel.textContent = meter.label;
  usage.setAttribute("aria-valuenow", String(meter.fill));
  usage.setAttribute("aria-valuetext", meter.detail);
  usage.tabIndex = 0;
  usage.setAttribute("aria-label", "Next request context estimate");
  usage.title = meter.detail;
  usage.querySelector<HTMLElement>(".usage-bar")!.hidden = false;
}

// Source text never becomes executable markup, including raw HTML nodes.
function fileReference(parent: HTMLElement, text: string, code = false): void {
  const pattern = /(?:https?:\/\/|mailto:)[^\s]+|(?:\.?\.?\/|\/)?[\w@.-]+(?:\/[\w@.-]+)+\.[A-Za-z0-9]+(?::[1-9]\d*(?::[1-9]\d*)?|#L[1-9]\d*)?/g;
  let at = 0;
  for (const match of text.matchAll(pattern)) {
    parent.append(document.createTextNode(text.slice(at, match.index)));
    const value = match[0];
    const before = text[(match.index ?? 0) - 1];
    if (/^(?:https?:\/\/|mailto:)/.test(value) || (before && /[\w/:]/.test(before))) parent.append(document.createTextNode(value));
    else {
      const location = /(?::([1-9]\d*)(?::[1-9]\d*)?|#L([1-9]\d*))$/.exec(value);
      const path = location ? value.slice(0, location.index) : value;
      const line = location ? Number(location[1] ?? location[2]) : undefined;
      const button = el("button", "file-link", value); button.type = "button";
      button.title = `Open ${path}${line ? ` at line ${line}` : ""}`;
      button.addEventListener("click", () => post({ type: "openFile", path, ...(line ? { line } : {}) }));
      if (code) { const span = el("code"); span.append(button); parent.append(span); } else parent.append(button);
    }
    at = (match.index ?? 0) + value.length;
  }
  parent.append(document.createTextNode(text.slice(at)));
}

function renderMarkdown(root: HTMLElement, source: string): void {
  const tree = fromMarkdown(source, { extensions: [gfm()], mdastExtensions: [gfmFromMarkdown()] });
  const definitions = new Map<string, { url: string }>();
  const collect = (node: any): void => {
    if (node.type === "definition") definitions.set(node.identifier, node);
    for (const child of node.children ?? []) collect(child);
  };
  collect(tree);
  const render = (parent: HTMLElement, node: any): void => {
    if (node.type === "definition") return;
    if (node.type === "html") { parent.append(document.createTextNode(node.value)); return; }
    if (node.type === "text") { if (parent.closest("a")) parent.append(document.createTextNode(node.value)); else fileReference(parent, node.value); return; }
    if (node.type === "inlineCode") {
      const span = el("code");
      if (parent.closest("a")) span.textContent = node.value; else fileReference(span, node.value);
      parent.append(span); return;
    }
    if (node.type === "code") {
      parent.append(node.lang === "diff" ? diffBlock(node.value) : codeBlock(node.lang || "Code", node.value, target => { target.textContent = node.value; }));
      return;
    }
    if (node.type === "image" || node.type === "imageReference") { parent.append(document.createTextNode(node.alt || "")); return; }
    if (node.type === "link" || node.type === "linkReference") {
      let url: URL | undefined;
      try { url = new URL(node.url ?? definitions.get(node.identifier)?.url); } catch { /* Unsafe or relative links stay text. */ }
      if (!url || !["http:", "https:", "mailto:"].includes(url.protocol)) {
        for (const child of node.children ?? []) render(parent, child);
        return;
      }
      const link = el("a"); link.href = url.href; link.rel = "noopener noreferrer";
      for (const child of node.children ?? []) render(link, child);
      parent.append(link); return;
    }
    if (node.type === "table") {
      const wrapper = el("div", "markdown-table"); const table = el("table");
      const head = el("thead"); const body = el("tbody");
      node.children.forEach((row: any, index: number) => {
        const tr = el("tr");
        row.children.forEach((cell: any, column: number) => {
          const td = el(index === 0 ? "th" : "td");
          if (index === 0) td.setAttribute("scope", "col");
          if (node.align[column]) td.style.textAlign = node.align[column];
          for (const child of cell.children) render(td, child);
          tr.append(td);
        });
        (index === 0 ? head : body).append(tr);
      });
      table.append(head, body); wrapper.append(table); parent.append(wrapper); return;
    }
    const tags: Record<string, keyof HTMLElementTagNameMap> = { paragraph: "p", blockquote: "blockquote", thematicBreak: "hr", strong: "strong", emphasis: "em", delete: "del", break: "br", listItem: "li" };
    const tag = node.type === "heading" ? `h${node.depth}` : node.type === "list" ? (node.ordered ? "ol" : "ul") : tags[node.type];
    if (!tag) { for (const child of node.children ?? []) render(parent, child); return; }
    const target = document.createElement(tag);
    if (node.type === "list" && node.ordered && node.start !== null) target.setAttribute("start", String(node.start));
    if (node.type === "listItem" && typeof node.checked === "boolean") {
      const checkbox = el("input"); checkbox.type = "checkbox"; checkbox.disabled = true; checkbox.checked = node.checked;
      checkbox.setAttribute("aria-label", node.checked ? "Completed task" : "Incomplete task"); target.append(checkbox);
    }
    for (const child of node.children ?? []) render(target, child);
    parent.append(target);
  };
  const container = el("div");
  render(container, tree);
  root.replaceChildren(...Array.from(container.childNodes));
}

window.addEventListener("message", event => {
  if (event.data && typeof event.data.type === "string") receive(event.data as HostToWebview);
});
form.addEventListener("submit", event => {
  event.preventDefault();
  const text = prompt.value;
  if (!text.trim() || prompt.disabled || sending) return;
  closePalette();
  commandError.hidden = true;
  // Idle compaction holds its HTTP response until completion. It must not own
  // the ordinary message submission lock while the server queues input safely.
  if (!/^\/compact(?:\s|$)/.test(text)) { sending = true; pendingSendText = text; }
  post({ type: "send", text, includeContext: includeContext.checked });
  updateControls();
  stickToBottom = true;
  follow();
});
prompt.addEventListener("input", () => { lastConsumedDraft = undefined; paletteDismissed = false; commandError.hidden = true; filterPalette(); updateControls(); });
function updateContextToggle(): void {
  contextPreview.classList.toggle("off", !includeContext.checked);
}
includeContext.addEventListener("change", () => {
  vscode.setState({ ...vscode.getState(), includeContext: includeContext.checked });
  updateContextToggle();
});
updateContextToggle();
prompt.addEventListener("keydown", event => {
  if (!palette.hidden && !event.isComposing) {
    if (event.key === "Escape") { event.preventDefault(); paletteDismissed = true; pickerKind = undefined; closePalette(); return; }
    if (["ArrowUp", "ArrowDown"].includes(event.key)) {
      event.preventDefault(); if (paletteRows.length) { paletteIndex = (paletteIndex + (event.key === "ArrowDown" ? 1 : -1) + paletteRows.length) % paletteRows.length; renderPalette(); } return;
    }
    if ((event.key === "Enter" && !event.shiftKey) || event.key === "Tab") {
      if (paletteRows.length || pickerKind || event.key === "Tab") { event.preventDefault(); choosePalette(paletteIndex); return; }
      // An unknown slash command is still submitted for host validation.
    }
  }
  if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
    event.preventDefault();
    form.requestSubmit();
  }
});
stop.addEventListener("click", () => post({ type: "stop" }));
post({ type: "ready" });
