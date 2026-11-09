/// <reference lib="dom" />
import { fromMarkdown } from "mdast-util-from-markdown";
import { gfm } from "micromark-extension-gfm";
import { gfmFromMarkdown } from "mdast-util-gfm";
import type { HostToWebview, WebviewToHost, PermissionEvidence, PermissionDecision, ImageRef } from "../chatProtocol";
import { evidenceLabel, permissionOutcome } from "../eventView";
import { contextMeter } from "../contextMeter";
import { displayTitle } from "../sessionTitle";
import { ConversationIdentity } from "./conversationIdentity";
import { diffKind, parseWake } from "./wake";
import { ranked, narrowHint, PICKER_LIMIT } from "./pickerSearch";
import { ImageCache } from "./imageCache";
import { createSecretCard, expiresIn, KEY_ICON, scopeText, type SecretCard } from "./secretCard";
import type { SecretInfo } from "../chatProtocol";
import type { CommandInfo } from "../chatProtocol";
import type { ChildInfo } from "../chatProtocol";
import { actionsSummary, childGroup, crewOf, orderAgents, childStateLabels, countChildren, durationLabel, filterChildren, footerText, oneLine, stripChildren, type ChildFilter } from "./agentStrip";

const pendingMarkdown = new WeakSet<HTMLElement>();
let nextUid = 0;

/** What a tab's status icon shows. `waiting` means the conversation needs the Captain. */
export type TabStatus = "idle" | "running" | "waiting" | "done" | "error";

export interface ConversationEnv {
  /** Sends a message to the host; the shell adds the tab. */
  post(message: WebviewToHost): void;
  /** A subagent editor panel: no way back to the session list. */
  editor: boolean;
  /** Per-viewer preference kept by the shell. */
  getAttach(): boolean;
  setAttach(value: boolean): void;
  /** Status, title, agent or model changed: the shell redraws its tabs and list. */
  changed(): void;
  showSessions(): void;
}

export interface ConversationView {
  readonly root: HTMLElement;
  receive(event: HostToWebview): void;
  status(): TabStatus;
  title(): string;
  sessionId(): string;
  agent(): string;
  model(): string;
  setEditorContext(label: string): void;
  setMessageCount(count: number | undefined): void;
  focusComposer(): void;
  dispose(): void;
}

const svg = (path: string, className = "icon"): string =>
  `<svg class="${className}" viewBox="0 0 16 16" aria-hidden="true"><path d="${path}"/></svg>`;
const ICON = {
  back: svg("M10 3.5 5.5 8l4.5 4.5"),
  chevron: svg("M4.5 6.5 8 10l3.5-3.5"),
  send: svg("M2.5 8 13.5 2.5 10.5 13.5 8 9Z"),
  stop: svg("M4.5 4.5h7v7h-7Z"),
  attach: svg("M10.5 5.5 6 10a1.4 1.4 0 0 0 2 2l5-5a3 3 0 0 0-4.2-4.2L4.2 7.6a4.5 4.5 0 0 0 6.4 6.4"),
  close: svg("m4.5 4.5 7 7m0-7-7 7"),
};

const TEMPLATE = `
<header class="session-header">
  <div class="sh-row">
    <button type="button" class="icon-button sh-back" aria-label="Show sessions" title="Sessions">${ICON.back}</button>
    <div class="sh-text"><div class="sh-title">New conversation</div><div class="sh-meta"></div></div>
    <button type="button" class="icon-button sh-secrets" aria-haspopup="dialog" aria-expanded="false" aria-label="Secrets of this conversation" title="Secrets" hidden>${KEY_ICON}</button>
    <span class="ctx-ring" role="meter" aria-label="Next request context estimate" aria-valuemin="0" aria-valuemax="100" tabindex="0" hidden><svg class="ring" viewBox="0 0 20 20" aria-hidden="true"><circle class="ring-track" cx="10" cy="10" r="8"/><circle class="ring-fill" cx="10" cy="10" r="8" transform="rotate(-90 10 10)"/></svg><span class="ring-label"></span></span>
    <span class="status" role="status" aria-live="polite" data-state="connecting"><span class="status-dot"></span><span class="status-label">connecting</span></span>
  </div>
  <div class="agent-strip"></div>
  <div class="sa-menu" hidden></div>
  <div class="secrets-pop" role="dialog" aria-label="Secrets" hidden></div>
</header>
<div class="child-header" hidden></div>
<main class="transcript" role="log" aria-label="Conversation" aria-live="polite" aria-relevant="additions text"><div class="empty">Ask about the code, or describe what you want to do.</div></main>
<form class="composer">
  <div class="ctx-chips"></div>
  <textarea class="prompt" rows="2" placeholder="Message Nav…" aria-label="Message Nav" disabled></textarea>
  <div class="toolbar">
    <button type="button" class="icon-button attach-btn" aria-pressed="true" aria-label="Attach editor context" title="Attach editor context to your messages">${ICON.attach}</button>
    <span class="pending-model"></span>
    <span class="spacer"></span>
    <button type="button" class="model-btn" disabled aria-haspopup="listbox" aria-label="Model"><span class="model-spark" aria-hidden="true">✦</span><span class="model-name">Default model</span>${ICON.chevron}</button>
    <button type="button" class="agent-btn" disabled aria-haspopup="listbox" aria-expanded="false" aria-label="Agent"><span class="agent-dot" aria-hidden="true"></span><span class="agent-name">build</span>${ICON.chevron}</button>
    <button type="button" class="icon-button stop-btn" aria-label="Stop" title="Stop" hidden>${ICON.stop}</button>
    <button type="submit" class="icon-button send-btn" aria-label="Send" title="Send (Enter)" disabled>${ICON.send}</button>
  </div>
  <div class="agent-popover" hidden>
    <input class="picker-search agent-search" type="search" placeholder="Search agents · Ctrl+F favorite" aria-label="Search agents" role="combobox" aria-autocomplete="list" aria-expanded="true" aria-keyshortcuts="Control+F">
    <span class="sr-only agent-favorite-hint">Ctrl+F adds or removes the highlighted agent from favorites.</span>
    <ul class="agent-list" role="listbox" aria-label="Agents"></ul>
  </div>
</form>`;

/** One conversation: its header, transcript and composer. The shell keeps one per tab. */
export function createConversation(env: ConversationEnv): ConversationView {
const uid = ++nextUid;
const root = document.createElement("section");
root.className = "tab-view";
root.innerHTML = TEMPLATE;
const q = <T extends HTMLElement>(selector: string): T => root.querySelector<T>(selector)!;
const disposers: (() => void)[] = [];
const rawPost = (message: WebviewToHost): void => env.post(message);
const identity = new ConversationIdentity(() => {
  for (const button of root.querySelectorAll<HTMLButtonElement>(".control-card button, .control-card textarea")) button.disabled = true;
});
const post = (message: WebviewToHost): void => rawPost({ ...message, generation: identity.generation });
const header = q<HTMLElement>(".session-header");
const titleText = q<HTMLElement>(".sh-title");
const metaText = q<HTMLElement>(".sh-meta");
const backButton = q<HTMLButtonElement>(".sh-back");
backButton.hidden = env.editor;
backButton.addEventListener("click", () => env.showSessions());
const ring = q<HTMLElement>(".ctx-ring");
const ringFill = root.querySelector<SVGCircleElement>(".ring-fill")!;
const ringLabel = q<HTMLElement>(".ring-label");
const RING_LENGTH = 2 * Math.PI * 8;
const bypassToggle = document.createElement("button");
bypassToggle.type = "button"; bypassToggle.className = "bypass-toggle";
function renderBypass(enabled: boolean, pending = false, yolo = false, unavailable?: string): void {
  enabled = enabled || yolo || !!unavailable;
  const label = yolo ? "YOLO" : unavailable ? "Permission rules unavailable" : enabled ? "Bypass active" : "Safe mode";
  bypassToggle.innerHTML = `<svg class="icon" aria-hidden="true" viewBox="0 0 16 16"><path d="M8 1 14 3v5c0 3-6 7-6 7S2 11 2 8V3Z"/>${enabled ? '<path d="m2 2 12 12"/>' : ''}</svg><span class="bypass-label"></span>`;
  bypassToggle.querySelector("span")!.textContent = label;
  bypassToggle.setAttribute("aria-label", label);
  bypassToggle.setAttribute("aria-pressed", String(enabled));
  bypassToggle.dataset.pending = String(pending);
  bypassToggle.title = unavailable && !yolo ? unavailable : yolo ? "YOLO: no permission control at all. Set yolo to false in config.json to turn it off." : enabled ? "Ordinary permission prompts are answered for you; protected configuration, credentials and keys still ask." : "Every gated call is reviewed and may ask you.";
}
renderBypass(false);
q(".status").before(bypassToggle);
bypassToggle.addEventListener("click", () => post({ type: "toggleBypass" }));
const transcript = q<HTMLElement>(".transcript");
const empty = q<HTMLElement>(".empty");
const status = q<HTMLElement>(".status");
const statusLabel = q<HTMLElement>(".status-label");
const form = q<HTMLFormElement>(".composer");
const prompt = q<HTMLTextAreaElement>(".prompt");
const send = q<HTMLButtonElement>(".send-btn");
const stop = q<HTMLButtonElement>(".stop-btn");
const modelBtn = q<HTMLButtonElement>(".model-btn");
const modelNameEl = q<HTMLElement>(".model-name");
const pendingModel = q<HTMLElement>(".pending-model");
const attachBtn = q<HTMLButtonElement>(".attach-btn");
const chips = q<HTMLElement>(".ctx-chips");
const childHeader = q<HTMLElement>(".child-header");
let readOnly = false;
let archiveReadOnly = false;
let parentId = "";
let currentTitle = "New conversation";
let currentModel = "";
let currentVariant = "";
let agentTooltip = "";
let messageCount: number | undefined;

// ---- Composer: editor-context chip, attach toggle, agent toggle -------------------------------
let contextLabel = "No editor open";
let attach = env.getAttach();
let skipNext = false;
const contextOn = (): boolean => attach && !skipNext;
function renderContext(): void {
  attachBtn.setAttribute("aria-pressed", String(contextOn()));
  attachBtn.title = contextOn() ? "Editor context is attached to your next message. Click to stop attaching it." : attach ? "Editor context is left out of the next message. Click to attach it again." : "Attach editor context to your messages";
  chips.replaceChildren();
  chips.hidden = !contextOn() || contextLabel === "No editor open";
  if (chips.hidden) return;
  const chip = el("span", "context-chip");
  chip.title = `Attached to your next message: ${contextLabel}`;
  chip.append(el("span", "context-chip-label", contextLabel));
  const drop = el("button", "chip-close"); drop.type = "button";
  drop.setAttribute("aria-label", "Leave editor context out of the next message");
  drop.title = "Leave editor context out of the next message";
  drop.innerHTML = ICON.close;
  drop.addEventListener("click", () => { skipNext = true; renderContext(); });
  chip.append(drop);
  chips.append(chip);
}
attachBtn.addEventListener("click", () => {
  if (skipNext) skipNext = false; else { attach = !attach; env.setAttach(attach); }
  renderContext();
});

// ---- Agent picker: a button in the toolbar, a popover above it -------------------------------
interface AgentRow { id: string; description: string }
const agentBtn = q<HTMLButtonElement>(".agent-btn");
const agentDot = q<HTMLElement>(".agent-btn .agent-dot");
const agentName = q<HTMLElement>(".agent-btn .agent-name");
const agentPop = q<HTMLElement>(".agent-popover");
const agentSearch = q<HTMLInputElement>(".agent-search");
const agentList = q<HTMLElement>(".agent-list");
// The stars sit inside listbox options and take no focus, so the search box carries the favorite
// shortcut and describes it to screen readers.
const agentFavoriteHint = q<HTMLElement>(".agent-favorite-hint");
agentFavoriteHint.id = `agent-favorite-hint-${uid}`;
agentSearch.setAttribute("aria-describedby", agentFavoriteHint.id);
agentList.id = `agent-list-${uid}`;
agentSearch.setAttribute("aria-controls", agentList.id);
let agentFavorites = new Set<string>();
let agentRows: AgentRow[] = [];
let agentIndex = 0;
function paintCrew(node: HTMLElement, agent: string): void {
  const crew = crewOf(agent);
  if (crew) node.dataset.crew = crew; else delete node.dataset.crew;
}
const agentSeg = {
  agents: [] as AgentRow[], current: "build", disabled: false,
  render(): void {
    agentName.textContent = this.current;
    paintCrew(agentDot, this.current);
    agentBtn.disabled = this.disabled || this.agents.length === 0;
    agentBtn.setAttribute("aria-label", `Agent: ${this.current}`);
    agentBtn.title = this.disabled ? "The agent cannot be changed here" : "Switch agent";
    if (agentBtn.disabled) closeAgents(); else if (!agentPop.hidden) renderAgents();
  },
  setAgents(agents: AgentRow[], current: string): void { this.agents = agents; this.current = current; this.render(); },
  setCurrent(current: string): void { this.current = current; this.render(); },
  setDisabled(disabled: boolean): void { this.disabled = disabled; this.render(); },
};
function openAgents(): void {
  if (agentBtn.disabled) return;
  closePalette();
  agentSearch.value = ""; agentIndex = Math.max(0, agentSeg.agents.findIndex(row => row.id === agentSeg.current));
  agentPop.hidden = false; agentBtn.setAttribute("aria-expanded", "true");
  renderAgents();
  agentIndex = Math.max(0, agentRows.findIndex(row => row.id === agentSeg.current)); renderAgents();
  agentSearch.focus();
}
function closeAgents(restoreFocus = false): void {
  if (agentPop.hidden) return;
  agentPop.hidden = true; agentBtn.setAttribute("aria-expanded", "false");
  if (restoreFocus) agentBtn.focus();
}
function pickAgent(id: string): void {
  closeAgents(true);
  if (id !== agentSeg.current) post({ type: "selectAgent", id });
}
function renderAgents(): void {
  agentRows = orderAgents(agentSeg.agents, agentSearch.value, agentFavorites);
  agentIndex = Math.max(0, Math.min(agentIndex, agentRows.length - 1));
  agentList.replaceChildren(...agentRows.map((row, index) => {
    const item = el("li", "agent-row"); item.id = `agent-option-${uid}-${index}`;
    item.setAttribute("role", "option"); item.setAttribute("aria-selected", String(index === agentIndex));
    item.dataset.id = row.id;
    const dot = el("span", "agent-dot"); dot.setAttribute("aria-hidden", "true"); paintCrew(dot, row.id);
    const text = el("span", "agent-text");
    text.append(el("span", "agent-row-name", row.id));
    if (row.description) text.append(el("span", "agent-row-desc", oneLine(row.description, 120)));
    item.append(dot, text);
    if (row.id === agentSeg.current) { const check = el("span", "agent-check", "✓"); check.setAttribute("aria-label", "Current agent"); item.append(check); }
    const starred = agentFavorites.has(row.id);
    const star = el("button", "favorite-toggle agent-star", starred ? "★" : "☆"); star.type = "button"; star.tabIndex = -1;
    star.setAttribute("aria-pressed", String(starred));
    star.setAttribute("aria-label", `${starred ? "Remove" : "Add"} ${row.id} ${starred ? "from" : "to"} favorites`);
    star.title = starred ? "Remove from favorites" : "Add to favorites";
    star.addEventListener("mousedown", event => event.preventDefault());
    star.addEventListener("click", event => { event.stopPropagation(); post({ type: "toggleFavoriteAgent", name: row.id }); });
    item.append(star);
    item.addEventListener("mousedown", event => event.preventDefault());
    item.addEventListener("click", () => pickAgent(row.id));
    return item;
  }));
  if (!agentRows.length) agentList.append(el("li", "agent-empty", "No matching agents"));
  if (agentRows.length) agentSearch.setAttribute("aria-activedescendant", `agent-option-${uid}-${agentIndex}`); else agentSearch.removeAttribute("aria-activedescendant");
  agentList.children[agentIndex]?.scrollIntoView?.({ block: "nearest" });
}
agentBtn.addEventListener("click", () => { if (agentPop.hidden) openAgents(); else closeAgents(); });
agentSearch.addEventListener("input", () => { agentIndex = 0; renderAgents(); });
agentSearch.addEventListener("keydown", event => {
  if (event.isComposing) return;
  if (event.key === "ArrowDown" || event.key === "ArrowUp") {
    event.preventDefault();
    if (agentRows.length) { agentIndex = (agentIndex + (event.key === "ArrowDown" ? 1 : -1) + agentRows.length) % agentRows.length; renderAgents(); }
  } else if (event.key === "Enter") { event.preventDefault(); const row = agentRows[agentIndex]; if (row) pickAgent(row.id); }
  else if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); closeAgents(true); }
  else if (event.ctrlKey && !event.altKey && event.key.toLowerCase() === "f") {
    event.preventDefault();
    const row = agentRows[agentIndex];
    if (row) post({ type: "toggleFavoriteAgent", name: row.id });
  }
});
const onAgentOutside = (event: MouseEvent): void => {
  if (agentPop.hidden) return;
  const path = event.composedPath();
  if (!path.includes(agentPop) && !path.includes(agentBtn)) closeAgents();
};
document.addEventListener("click", onAgentOutside);
disposers.push(() => document.removeEventListener("click", onAgentOutside));

// ---- Session header: agent strip and the subagent dropdown ------------------------------------
const strip = q<HTMLElement>(".agent-strip");
const menu = q<HTMLElement>(".sa-menu");
const agentLabel = document.createElement("span");
agentLabel.className = "agent-label";
agentLabel.textContent = "build";
agentLabel.dataset.agent = "build";
const chipModel = el("span", "chip-model");
const primaryChip = el("span", "chip chip-primary");
primaryChip.append(agentLabel, chipModel);
const subChips = el("span", "chip-list");
const moreBtn = el("button", "chip chip-more"); moreBtn.type = "button";
moreBtn.setAttribute("aria-haspopup", "true"); moreBtn.setAttribute("aria-expanded", "false");
strip.append(primaryChip, subChips, moreBtn);
let children: ChildInfo[] = [];
let menuFilter: ChildFilter = "all";
const shownChildren = (): ChildInfo[] => (readOnly ? [] : children);
function openChild(child: ChildInfo): void {
  identity.bind(rawPost)({ type: "openChild", parentId, childId: child.id });
}
function renderStrip(): void {
  chipModel.textContent = currentModel ? shortName(currentModel) : "";
  chipModel.hidden = !currentModel;
  primaryChip.title = agentTooltip || (currentModel ? `${agentLabel.dataset.agent} · ${currentModel}` : String(agentLabel.dataset.agent));
  const all = shownChildren();
  const { shown, hidden } = stripChildren(all);
  subChips.replaceChildren(...shown.map(child => {
    const chip = el("button", "chip chip-sub"); chip.type = "button";
    chip.dataset.state = child.state; chip.dataset.group = childGroup(child.state);
    const dot = el("span", "sa-dot"); dot.dataset.state = child.state;
    chip.append(dot, el("span", "chip-text", `@${child.agent}${child.goal ? ` · ${oneLine(child.goal, 60)}` : ""}`));
    chip.title = `${child.agent} · ${childStateLabels[child.state]}${child.goal ? `\n${child.goal}` : ""}`;
    chip.addEventListener("click", () => openChild(child));
    return chip;
  }));
  strip.hidden = all.length === 0 && !readOnly;
  moreBtn.hidden = all.length === 0;
  moreBtn.innerHTML = "";
  moreBtn.append(document.createTextNode(hidden > 0 ? `+${hidden}` : "All"));
  moreBtn.insertAdjacentHTML("beforeend", ICON.chevron);
  moreBtn.setAttribute("aria-label", `All subagents (${all.length})`);
  if (!all.length && !menu.hidden) toggleMenu(false);
  else if (!menu.hidden) renderMenu();
}
function toggleMenu(open: boolean): void {
  menu.hidden = !open;
  moreBtn.setAttribute("aria-expanded", String(open));
  if (open) renderMenu();
}
function renderMenu(): void {
  const all = shownChildren();
  const counts = countChildren(all);
  menu.replaceChildren();
  const pills = el("div", "sa-pills");
  for (const [filter, label] of [["all", "All"], ["running", "Running"], ["done", "Done"]] as const) {
    const pill = el("button", "pill", `${label} ${counts[filter]}`); pill.type = "button";
    pill.setAttribute("aria-pressed", String(menuFilter === filter));
    pill.addEventListener("click", () => { menuFilter = filter; renderMenu(); });
    pills.append(pill);
  }
  menu.append(pills);
  const list = el("div", "sa-list");
  const titles = { running: "Running", error: "Error", done: "Done" } as const;
  for (const entry of filterChildren(all, menuFilter)) {
    list.append(el("h4", "sa-group", `${titles[entry.group]} · ${entry.items.length}`));
    for (const child of entry.items) {
      const row = el("article", "child-row sa-row"); row.dataset.state = child.state;
      const open = actionButton("", () => openChild(child));
      open.className = "child-open control-secondary sa-open";
      const dot = el("span", "sa-dot"); dot.dataset.state = child.state;
      const goal = el("span", "child-goal sa-goal", oneLine(child.goal) || "No goal reported");
      goal.title = child.goal;
      const state = el("span", "child-state", childStateLabels[child.state]); state.dataset.state = child.state;
      open.append(dot, el("span", "sa-agent", `@${child.agent}`), goal, state);
      const heading = el("div", "child-row-heading");
      heading.append(open, childActions(child));
      row.append(heading);
      // Delivery routing is protocol metadata, not a status for the Captain.
      if (child.reason) row.append(el("p", "child-reason", child.reason));
      if (child.error) row.append(el("p", "command-error", child.error));
      list.append(row);
    }
  }
  if (!list.childElementCount) list.append(el("p", "picker-empty", "No subagents match."));
  menu.append(list, el("div", "sa-footer", footerText(all)));
}
moreBtn.addEventListener("click", () => toggleMenu(!!menu.hidden));
const onDocumentClick = (event: MouseEvent): void => { if (!menu.hidden && !header.contains(event.target as Node)) toggleMenu(false); };
const onDocumentKey = (event: KeyboardEvent): void => { if (event.key === "Escape" && !menu.hidden) { toggleMenu(false); moreBtn.focus(); } };
document.addEventListener("click", onDocumentClick);
document.addEventListener("keydown", onDocumentKey);
disposers.push(() => { document.removeEventListener("click", onDocumentClick); document.removeEventListener("keydown", onDocumentKey); });

let rawTitle = "";
let firstCaptain = "";
/** A human title: the server's, unless it is empty or only the session id; else the first Captain message; never the raw id. */
function setTitle(title?: string): void {
  if (title !== undefined) rawTitle = title;
  currentTitle = displayTitle(rawTitle, sessionId, firstCaptain);
  titleText.textContent = currentTitle;
  titleText.title = currentTitle;
}
function renderMeta(): void {
  const parts: string[] = [];
  if (messageCount !== undefined) parts.push(`${messageCount} ${messageCount === 1 ? "message" : "messages"}`);
  if (currentModel) parts.push(shortName(currentModel));
  metaText.textContent = parts.join(" · ");
  metaText.hidden = parts.length === 0;
}
/** The model name without its `gateway/provider/` prefix. */
function shortName(name: string): string { return name.split("/").at(-1) || name; }

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
palette.className = "command-palette";
const pickerSearch = document.createElement("input");
pickerSearch.className = "picker-search";
pickerSearch.type = "search";
pickerSearch.placeholder = "Search names and descriptions…";
pickerSearch.setAttribute("aria-label", "Search choices");
pickerSearch.setAttribute("role", "combobox");
pickerSearch.setAttribute("aria-autocomplete", "list");
pickerSearch.setAttribute("aria-controls", `picker-options-${uid}`);
// Model picker only: the stars take no focus, so Ctrl+F on the search box is their keyboard path.
const pickerFavoriteHint = el("span", "sr-only", "Ctrl+F adds or removes the highlighted model from favorites.");
pickerFavoriteHint.id = `picker-favorite-hint-${uid}`;
const pickerCount = document.createElement("div");
pickerCount.className = "picker-count";
pickerCount.setAttribute("role", "status");
pickerCount.setAttribute("aria-live", "polite");
const pickerOptions = document.createElement("div");
pickerOptions.id = `picker-options-${uid}`; pickerOptions.className = "picker-options";
pickerOptions.setAttribute("role", "listbox");
pickerOptions.setAttribute("aria-label", "Choices");
palette.append(pickerSearch, pickerFavoriteHint, pickerCount, pickerOptions);
palette.hidden = true;
form.prepend(palette);
const commandError = document.createElement("div");
commandError.className = "command-error";
commandError.setAttribute("role", "alert");
commandError.hidden = true;
prompt.after(commandError);
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
  // The Favorites section sits first, is never folded and is not part of the PICKER_LIMIT cut; its rows
  // come before the groups' rows in `paletteRows`, so indices and option ids stay unique.
  favoriteRows = pickerKind === "model" ? allMatches.filter(row => favorites.has(row.name)).sort((a, b) => a.name.localeCompare(b.name)) : [];
  paletteRows = [...favoriteRows, ...groupRows];
  // Hidden outside the model picker so browse mode does not read a shortcut that does nothing there.
  pickerFavoriteHint.hidden = pickerKind !== "model";
  if (pickerKind === "model") {
    pickerSearch.setAttribute("aria-keyshortcuts", "Control+F");
    pickerSearch.setAttribute("aria-describedby", pickerFavoriteHint.id);
  } else {
    pickerSearch.removeAttribute("aria-keyshortcuts");
    pickerSearch.removeAttribute("aria-describedby");
  }
  pickerCount.textContent = `${allMatches.length} of ${pickerItems.length}${narrowHint(matches.length)}${pickerKind === "model" ? " · Alt+A folds or unfolds all · Ctrl+F favorite" : ""}`;
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
  if (paletteRows.length) pickerSearch.setAttribute("aria-activedescendant", `command-option-${uid}-${paletteIndex}`);
  else { pickerSearch.removeAttribute("aria-activedescendant"); if (!allMatches.length) pickerOptions.append(el("p", "picker-empty", "No matches. Try another search.")); }
  if (!pickerKind) {
    prompt.setAttribute("role", "combobox");
    prompt.setAttribute("aria-autocomplete", "list");
    prompt.setAttribute("aria-controls", pickerOptions.id);
    prompt.setAttribute("aria-expanded", "true");
    if (paletteRows.length) prompt.setAttribute("aria-activedescendant", `command-option-${uid}-${paletteIndex}`);
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
    option.id = `command-option-${uid}-${index}`;
    option.className = "command-option";
    option.title = row.name;
    option.setAttribute("role", "option");
    // The group headers already show gateway and provider, so a row inside a group shows the
    // short name; the accessible name stays the full canonical name.
    // This label replaces the option's content, star included, so it carries the favorite state itself.
    if (inGroup) option.setAttribute("aria-label", `${row.name}${pickerKind === "model" && favorites.has(row.name) ? ", favorite" : ""}`);
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
  pickerOptions.querySelector(`#command-option-${uid}-${paletteIndex}`)?.scrollIntoView?.({ block: "nearest" });
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
  if (paletteRows.length) prompt.setAttribute("aria-activedescendant", `command-option-${uid}-${paletteIndex}`);
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
function openModelPicker(): void {
  selectorPicker = true; pickerKind = "model"; pickerItems = modelItems; pickerSearch.value = ""; paletteIndex = 0; renderPalette(); pickerSearch.focus();
}
modelBtn.addEventListener("click", () => { if (!modelBtn.disabled) { closeAgents(); openModelPicker(); } });
modelBtn.addEventListener("keydown", event => {
  if (!modelBtn.disabled && ["Enter", " ", "ArrowDown"].includes(event.key)) { event.preventDefault(); openModelPicker(); }
});
type TextSegment = { kind: "text"; stepId?: string; content: HTMLElement; reasoning: HTMLElement; details: HTMLDetailsElement; text: string; thoughts: string };
type ToolGroup = { kind: "tools"; details: HTMLDetailsElement; summary: HTMLElement; calls: Map<string, Extract<HostToWebview, { type: "tool" }>>; chosen: boolean; finished: boolean };
type Turn = { root: HTMLElement; roots: HTMLElement[]; activityButton?: HTMLButtonElement; activityOpen?: boolean; last?: TextSegment | ToolGroup; calls: Map<string, HTMLElement>; groups: Map<string, ToolGroup>; time?: HTMLElement; startedAt?: number };
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
const secretCards = new Map<string, SecretCard>();
const secretsButton = q<HTMLButtonElement>(".sh-secrets");
const secretsPop = q<HTMLElement>(".secrets-pop");
let secretsList: SecretInfo[] = [];

/** A secret request still waits for the Captain's value. */
function secretWaiting(): boolean {
  if (readOnly) return false;
  for (const card of secretCards.values()) if (card.state() === "pending") return true;
  return false;
}

function renderSecretsButton(): void {
  secretsButton.hidden = readOnly || !sessionId || (!secretCards.size && !secretsList.length);
}

function toggleSecrets(open: boolean): void {
  secretsPop.hidden = !open;
  secretsButton.setAttribute("aria-expanded", String(open));
  if (open) { renderSecrets(); if (sessionId) post({ type: "listSecrets", sessionId }); }
}

/** The header list: each secret this conversation sees, its expiry, and Revoke. */
function renderSecrets(error?: string): void {
  secretsPop.replaceChildren(el("div", "secrets-title", "Secrets"));
  if (error) secretsPop.append(el("p", "command-error", error));
  if (!secretsList.length) secretsPop.append(el("p", "secrets-empty", "No secrets in this conversation."));
  const list = el("ul", "secrets-list");
  for (const secret of secretsList) {
    const row = el("li", "secrets-row");
    const text = el("div", "secrets-text");
    text.append(el("code", "", `$${secret.name}${secret.last4 ? ` ••••${secret.last4}` : ""}`));
    const when = expiresIn(secret.expiresAt);
    text.append(el("span", "secrets-meta", `${when ? `expires ${when}` : "expired"} · ${scopeText(secret.scope)} · used ${secret.useCount}×`));
    const revoke = actionButton("Revoke", () => { revoke.disabled = true; post({ type: "revokeSecret", sessionId, name: secret.name }); });
    revoke.setAttribute("aria-label", `Revoke ${secret.name}`);
    row.append(text, revoke);
    list.append(row);
  }
  secretsPop.append(list);
  secretsPop.append(el("p", "secrets-foot", "Values stay on the server, out of the chat; the agent only sees their names."));
}
secretsButton.addEventListener("click", () => toggleSecrets(secretsPop.hidden !== false));
const onSecretsOutside = (event: MouseEvent): void => {
  if (!secretsPop.hidden && !secretsPop.contains(event.target as Node) && !secretsButton.contains(event.target as Node)) toggleSecrets(false);
};
document.addEventListener("click", onSecretsOutside);

/** Grows the composer with its text, from about three lines up to 40% of the view; it scrolls beyond. */
let sizedFor = "";
function autosizePrompt(): void {
  // Measuring forces two layouts: skip it when nothing that changes the height did.
  const key = `${window.innerWidth}x${window.innerHeight}|${document.documentElement.style.getPropertyValue("--nav-font-size")}|${prompt.value}`;
  if (key === sizedFor) return;
  sizedFor = key;
  const max = Math.max(120, Math.round(window.innerHeight * 0.4));
  prompt.style.height = "auto";
  const wanted = prompt.scrollHeight;
  prompt.style.height = `${Math.min(wanted, max)}px`;
  prompt.style.overflowY = wanted > max ? "auto" : "hidden";
}
window.addEventListener("resize", autosizePrompt);
disposers.push(() => window.removeEventListener("resize", autosizePrompt));

function updateControls(): void {
  autosizePrompt();
  prompt.disabled = readOnly || state === "connecting";
  if (compacting) prompt.placeholder = "Messages you send now are queued until compaction ends";
  else if (!readOnly) prompt.placeholder = secretWaiting() ? "Nav is waiting for the key above… (or write to answer otherwise)" : "Message WithNav…";
  if (readOnly) modelBtn.disabled = true;
  send.disabled = prompt.disabled || sending || imagesReading() || (!prompt.value.trim() && !pendingImages.length);
  stop.hidden = readOnly || (state !== "busy" && !compacting);
}

function setStatus(next: string, message?: string): void {
  state = next;
  status.dataset.state = next;
  statusLabel.textContent = message ? `${next}: ${message}` : next;
  updateControls();
}

/** What the conversation asks of the Captain right now: an open permission, pause or question. */
function waitingCount(): number {
  if (readOnly) return 0;
  let count = pendingQuestion ? 1 : 0;
  for (const card of permissionCards.values()) if (!card.controls.hidden) count++;
  for (const card of pauseCards.values()) if (!card.controls.hidden) count++;
  for (const card of questionCards.values()) if (!card.answered && !pendingQuestion) count++;
  // A waiting secret request is the pending question when there is one: counted once.
  let secrets = 0;
  for (const card of secretCards.values()) if (card.state() === "pending") secrets++;
  return count + (pendingQuestion && secrets ? secrets - 1 : secrets);
}

/** One word for the tab's icon: waiting on the Captain beats an error, which beats running. */
function computeStatus(): TabStatus {
  if (waitingCount() > 0) return "waiting";
  if (state === "error") return "error";
  if (state === "busy" || compacting) return "running";
  return transcript.querySelector(".message, .control-card") ? "done" : "idle";
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
  const id = crewOf(agent) ?? agent;
  const names: Record<string, string> = { captain: "Captain", nav: "Nav", vigie: "Vigie", lux: "Lux", mousse: "Mousse", cartographe: "Cartographe", sondeur: "Sondeur" };
  byline.textContent = names[id] ?? agent;
  if (names[id]) {
    byline.dataset.crew = id;
    const glyph = el("span", "crew-glyph");
    glyph.setAttribute("aria-hidden", "true");
    byline.prepend(glyph);
  }
}

// ---- Images: pending ones in the composer, stored ones as thumbnails ---------------------------

/** Most images a message carries, and the largest file read: the server's 3.75 MiB per image. */
const MAX_IMAGES = 8;
const MAX_IMAGE_FILE = 3_932_160;
// Each tab keeps its own cache and the chat shows up to 12 tabs, so these bound one view: 64
// pictures, and 16 Mi characters, three full-size attachments or many screenshots.
const IMAGE_CACHE_ENTRIES = 64;
const IMAGE_CACHE_CHARS = 16 * 1024 * 1024;
/**
 * Composer images in the order the Captain added them. A slot is reserved when its file is
 * accepted, before the read finishes (`data` stays undefined meanwhile), so the cap counts reads
 * in flight and a send waits for every slot instead of leaving late reads behind.
 */
type PendingSlot = { name: string; size: number; data?: string };
let pendingImages: PendingSlot[] = [];
/**
 * Submissions that carried images and have no acceptance yet, by the id the host echoes in
 * `sendResult`. Text cannot tell submissions apart (every image-only message is ""). A refusal
 * keeps its entry: a `/skill` send reports `accepted: false` while the skill is pending and
 * `accepted: true` once it settles, and only that later result may take its images away.
 */
const submittedImages = new Map<number, PendingSlot[]>();
let nextSubmission = 0;
const pendingList = el("ul", "pending-images");
pendingList.setAttribute("aria-label", "Images to send");
pendingList.hidden = true;
const imagesReading = (): boolean => pendingImages.some(image => image.data === undefined);

function renderPending(): void {
  pendingList.replaceChildren(...pendingImages.map(image => {
    const item = el("li", "pending-image", `${image.name} · ${image.data === undefined ? "reading…" : `${Math.ceil(image.size / 1024)} KiB`}`);
    const remove = el("button", "pending-image-remove", "×");
    remove.type = "button";
    remove.setAttribute("aria-label", `Remove ${image.name}`);
    remove.addEventListener("click", () => { pendingImages = pendingImages.filter(other => other !== image); renderPending(); updateControls(); });
    item.append(remove);
    return item;
  }));
  pendingList.hidden = !pendingImages.length;
}

function showImageProblems(problems: string[]): void {
  if (!problems.length) return;
  commandError.textContent = problems.join(" ");
  commandError.hidden = false;
}

function addImageFiles(files: Iterable<File>): void {
  const problems: string[] = [];
  const omitted: string[] = [];
  for (const file of files) {
    if (!file.type.startsWith("image/") && !/\.(png|jpe?g|gif|webp|svg)$/i.test(file.name)) continue;
    const name = file.name || "image";
    if (pendingImages.length >= MAX_IMAGES) { omitted.push(name); continue; }
    if (file.size > MAX_IMAGE_FILE) { problems.push(`${name} is too large to attach.`); continue; }
    const slot: PendingSlot = { name, size: file.size };
    pendingImages.push(slot);
    // A read that ends after the Captain removed its slot changes nothing and reports nothing.
    const settle = (data: string): void => {
      if (!pendingImages.includes(slot)) return;
      if (data) slot.data = data;
      else { pendingImages = pendingImages.filter(other => other !== slot); showImageProblems([`${name} could not be read.`]); }
      renderPending(); updateControls();
    };
    const reader = new FileReader();
    reader.onload = () => { const url = String(reader.result ?? ""); settle(url.slice(url.indexOf(",") + 1)); };
    reader.onerror = () => settle("");
    reader.onabort = () => settle("");
    try { reader.readAsDataURL(file); } catch { pendingImages = pendingImages.filter(other => other !== slot); problems.push(`${name} could not be read.`); }
  }
  if (omitted.length) problems.unshift(`A message carries at most ${MAX_IMAGES} images; not attached: ${omitted.join(", ")}.`);
  showImageProblems(problems);
  renderPending(); updateControls();
}

/** Per attachment: its `data:` URL, or `false` once it is known unavailable; bounded, see ImageCache. */
const imageUrls = new ImageCache(IMAGE_CACHE_ENTRIES, IMAGE_CACHE_CHARS);
/** Attachments asked of the extension host and not answered yet: one request each, whatever the cache evicts. */
const imagesLoading = new Set<string>();
function thumbnails(images: ImageRef[] | undefined): HTMLElement | undefined {
  if (!images?.length) return undefined;
  const row = el("div", "thumbs");
  for (const image of images) {
    const name = image.name || "image";
    const button = el("button", "thumb");
    button.type = "button";
    button.title = `${name}${image.width && image.height ? ` · ${image.width}×${image.height}` : ""}: open beside`;
    // The image alt only names the picture; the button's name says what activating it does.
    button.setAttribute("aria-label", `Open ${name} beside`);
    button.dataset.attachment = image.attachment_id;
    const img = el("img") as HTMLImageElement;
    img.alt = name;
    button.append(img);
    button.addEventListener("click", () => post({ type: "openImage", attachmentId: image.attachment_id, name }));
    row.append(button);
    const known = imageUrls.get(image.attachment_id);
    if (known) img.src = known;
    else if (known === false) button.classList.add("missing");
    else if (!imagesLoading.has(image.attachment_id)) { imagesLoading.add(image.attachment_id); post({ type: "loadImage", attachmentId: image.attachment_id }); }
  }
  return row;
}

function showImageData(attachmentId: string, url: string | undefined): void {
  imagesLoading.delete(attachmentId);
  imageUrls.set(attachmentId, url ?? false);
  for (const button of transcript.querySelectorAll<HTMLButtonElement>(`.thumb[data-attachment="${CSS.escape(attachmentId)}"]`)) {
    const img = button.querySelector("img");
    if (url && img) { img.src = url; button.classList.remove("missing"); }
    else button.classList.add("missing");
  }
}

function message(author: string, text: string, context?: string, skill?: { name: string; origin: string }, source?: string, images?: ImageRef[]): void {
  if ((author === "navigator" || author === "nav")) { add(navNote(text)); return; }
  if ((author === "driver" || author === "captain") && !firstCaptain && text.trim()) { firstCaptain = text; if (sessionId) setTitle(); }
  const root = el("article", `message ${author === "driver" ? "captain" : author}`);
  if (author === "cognition") root.dataset.agent = agentLabel.dataset.agent;
  const byline = el("div", "byline", (author === "driver" || author === "captain") ? pendingQuestion ? "Captain · Answer" : "Captain" : author === "cognition" ? "Cognition" : "Subagent output · untrusted data");
  if (author === "cognition") decorateCrew(byline, agentLabel.dataset.agent);
  if (author === "driver" || author === "captain") decorateCrew(byline, "captain");
  if (author === "subagent" && source) { const id = source.split(":").at(-1)!; if (["vigie", "lux", "mousse", "cartographe", "sondeur", "explore", "librarian", "review", "oracle", "explorer"].includes(id)) { decorateCrew(byline, id); byline.append(el("span", "skill-note", "Untrusted data")); } }
  if (byline.dataset.crew && author !== "captain" && author !== "driver") root.dataset.by = byline.dataset.crew;
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
  const thumbs = thumbnails(images);
  if (thumbs) root.append(thumbs);
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

/** A ```mermaid block. No renderer is bundled (it would need inline styles the CSP refuses), so the card shows the source, labelled as a diagram. */
function diagramCard(source: string): HTMLElement {
  const card = codeBlock("mermaid", source, code => { code.textContent = source; });
  card.classList.add("diagram-card");
  card.querySelector(".code-bar > span")?.after(el("span", "diagram-note", "Diagram source"));
  return card;
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
  const byline = el("div", "byline"); decorateCrew(byline, agentLabel.dataset.agent);
  const time = el("span", "by-time");
  byline.append(el("span", "by-agent", ` · ${agentLabel.dataset.agent ?? "build"}`), time);
  root.append(byline);
  const item: Turn = { root, roots: [root], calls: new Map(), groups: new Map(), time };
  turns.set(id, item);
  add(root);
  return item;
}

function finishGroup(item: Turn): void {
  if (item.last?.kind !== "tools") return;
  item.last.finished = true;
  item.last.details.removeAttribute("data-live");
  if (!item.last.chosen) item.last.details.open = false;
}

function continueTurn(item: Turn): void {
  if (transcript.lastElementChild === item.root) return;
  const root = el("article", "message cognition");
  root.dataset.agent = item.root.dataset.agent;
  item.root = root;
  item.roots.push(root);
  if (item.activityButton) root.dataset.activity = item.activityOpen ? "open" : "closed";
  add(root);
}

/** A turn with two or more runs of actions gets one "Activity" line at its top; the per-step lines fold into it. */
function refreshActivity(item: Turn): void {
  const groups = new Set(item.groups.values());
  if (groups.size < 2) return;
  const events = [...groups].flatMap(group => [...group.calls.values()]);
  const failed = events.filter(call => call.phase === "end" && call.ok === false).length;
  if (!item.activityButton) {
    const button = el("button", "turn-activity"); button.type = "button";
    button.addEventListener("click", () => { item.activityOpen = !item.activityOpen; applyActivity(item); });
    item.activityButton = button;
    item.roots[0].firstElementChild?.after(button);
  }
  item.activityButton.replaceChildren(el("span", "", `Activity · ${actionsSummary(events)}`));
  if (failed) item.activityButton.append(el("span", "turn-activity-failed", ` · ${failed} failed`));
  applyActivity(item);
}
function applyActivity(item: Turn): void {
  for (const root of item.roots) root.dataset.activity = item.activityOpen ? "open" : "closed";
  item.activityButton?.setAttribute("aria-expanded", String(!!item.activityOpen));
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
      details.dataset.live = "";
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
  group.summary.replaceChildren(el("span", "", actionsSummary(calls)));
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
  if (event.mcpUi && (event.mcpUi.resource_uri || event.mcpUi.html || event.mcpUi.url)) {
    const ui = event.mcpUi;
    const btn = el("button", "mcp-open-btn", "Open App");
    btn.type = "button";
    btn.title = `Open ${ui.server} (${ui.tool || event.name}) beside`;
    btn.addEventListener("click", () => {
      post({
        type: "openMcpApp",
        app: {
          callId: event.callId,
          server: ui.server,
          tool: ui.tool || event.name,
          resourceUri: ui.resource_uri,
          html: ui.html,
          url: ui.url,
          args: event.args,
          output: event.output,
          metadata: event.metadata,
        },
      });
    });
    row.append(btn);
  }
  if (event.mcpMedia && event.mcpMedia.length > 0) {
    if (!group.chosen) group.details.open = true;
    const grid = el("div", "mcp-media-grid");
    event.mcpMedia.forEach((media, idx) => {
      // A button, so the zoom works from the keyboard; aria-pressed reports whether it is enlarged.
      const card = el("button", "mcp-media-card");
      card.type = "button";
      card.setAttribute("aria-label", `Zoom ${event.name} preview ${idx + 1}`);
      card.setAttribute("aria-pressed", "false");
      const img = document.createElement("img");
      img.src = media.svg
        ? `data:image/svg+xml;utf8,${encodeURIComponent(media.svg)}`
        : `data:${media.mime_type};base64,${media.data ?? ""}`;
      img.alt = `${event.name} preview ${idx + 1}`;
      card.addEventListener("click", () => card.setAttribute("aria-pressed", String(card.classList.toggle("zoomed"))));
      card.append(img);
      grid.append(card);
    });
    row.append(grid);
  }
  if (event.evidence) row.append(evidenceDetails(event.evidence));
  refreshActivity(item);
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
  buttons.append(actionButton("Allow", () => answer("allow_once"), true));
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
  // Favorites are shared by every conversation and name none.
  if (event.type !== "favorites" && event.type !== "favoriteAgents" && !identity.receive(event)) return;
  if (event.type === "conversationOpened") {
    renderBypass(false);
    form.hidden = false;
    archiveReadOnly = false;
    compacting = false; compactionRow = undefined;
    ring.removeAttribute("aria-busy"); ring.removeAttribute("data-updating"); ring.hidden = true;
    readOnly = false; parentId = ""; childHeader.hidden = true; children = []; toggleMenu(false);
    sessionId = event.id ?? "";
    sending = false;
    pendingSendText = undefined;
    lastConsumedDraft = undefined;
    closePalette();
    commandError.hidden = true;
    agentLabel.textContent = "build";
    agentLabel.dataset.agent = "build";
    pendingModel.textContent = "";
    currentModel = ""; currentVariant = ""; messageCount = undefined; agentTooltip = "";
    firstCaptain = "";
    setTitle(event.id === undefined ? "" : event.title);
    agentSeg.setCurrent("build"); renderStrip(); renderMeta();
  }
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
      ring.setAttribute("aria-busy", String(event.pending));
      ring.dataset.updating = String(event.pending);
      updateControls(); break;
    }
    case "subagents": {
      parentId = event.parentId;
      children = event.items;
      renderStrip();
      break;
    }
    case "viewMode": {
      archiveReadOnly = !!event.archiveReason;
      readOnly = event.readOnly;
      agentSeg.setDisabled(readOnly); renderStrip();
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
        agentSeg.setDisabled(true);
        childHeader.append(el("p", "", `Read-only · subagent of ${event.parentTitle ?? "parent conversation"}`),
          strong(childLabel(event.child)), el("p", "", "Read-only subagent transcript. Send messages and answer permissions in the parent."), childActions(event.child));
        if (event.child.error) childHeader.append(el("p", "command-error", event.child.error));
        renderStrip();
      }
      form.hidden = readOnly;
      prompt.placeholder = readOnly ? "Read-only · return to parent to send" : "Message WithNav…";
      closePalette(); updateControls(); break;
    }
    case "commands": commands = event.items; filterPalette(); break;
    case "commandError": commandError.textContent = event.text; commandError.hidden = false; updateControls(); break;
    case "agent": agentLabel.textContent = event.name; agentLabel.dataset.agent = event.name; agentSeg.setCurrent(event.name); renderStrip(); break;
    case "assignments": {
      if (event.models) renderModels({ type: "models", items: event.models, current: event.model, locked: !!event.locked });
      if (event.model) currentModel = event.model;
      currentVariant = event.variant ?? "";
      paintModelName();
      agentSeg.disabled = readOnly || !!event.locked;
      agentSeg.setAgents(event.agents.filter(agent => agent.mode !== "subagent").map(agent => ({ id: agent.id, description: agent.description })), event.current);
      agentLabel.textContent = event.current;
      agentLabel.dataset.agent = event.current;
      const types: Record<string,string> = { reflex: "FIM · editor", instinct: "SystemOne", metacognition: "Chat" };
      const tooltip = [`Cognition · ${event.current}: ${event.model ?? "Unassigned"}${event.variant === null ? " · variant off" : event.variant ? ` · ${event.variant}` : ""} · model source: ${event.source ?? "assignment"} · variant source: ${event.variant_source ?? "assignment"}`, ...event.layers.map(layer => `${layer.layer} (${types[layer.layer]}): ${layer.model ?? "Unassigned"}${layer.variant ? ` · ${layer.variant}` : ""} · ${layer.status}${layer.reason ? ` — ${layer.reason}` : ""}`), "Usable describes configuration, not network availability. Assignments are read-only."].join("\n");
      header.title = tooltip; agentTooltip = tooltip; renderStrip(); renderMeta();
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
      if (!event.items.length) ring.hidden = true;
      transcript.replaceChildren(empty);
      empty.hidden = false;
      turns.clear();
      permissionCards.clear();
      questionCards.clear();
      pauseCards.clear();
      for (const card of secretCards.values()) card.dispose();
      secretCards.clear();
      secretsList = [];
      pendingQuestion = false;
      stickToBottom = true;
      replaying = true;
      try { for (const item of event.items) if (item.type !== "history") receive(item); }
      finally { replaying = false; }
      // The history knows a secret's name and expiry, not its last characters or a revocation.
      if (sessionId && !readOnly && [...secretCards.values()].some(card => card.state() === "saved")) post({ type: "listSecrets", sessionId });
      renderSecretsButton(); updateControls();
      if (readOnly) { agentSeg.setDisabled(true); modelBtn.disabled = true; for (const control of document.querySelectorAll<HTMLButtonElement | HTMLTextAreaElement>(".control-card button, .control-card textarea")) control.disabled = true; }
      if (state === "connecting") setStatus("ready");
      follow();
      break;
    case "status": setStatus(event.state, event.message); break;
    case "message": {
      if (event.secret) {
        const card = event.secret.requestId ? secretCards.get(event.secret.requestId) : undefined;
        if (card) card.settle(event.secret);
        if (event.secret.requestId && card) pendingQuestion = false;
        // The host's note stands for the card's outcome: no Captain bubble.
        if (!card) add(el("article", "message control-notice secret-notice", event.secret.outcome === "stored" ? `Secret $${event.secret.name} provided · ${scopeText(event.secret.scope)}` : `Secret $${event.secret.name} declined`));
        renderSecretsButton(); updateControls();
        break;
      }
      if ((event.author === "captain" || event.author === "driver") && pendingQuestion) {
        // A written answer settles a waiting secret request: the agent gets the text instead.
        for (const card of secretCards.values()) if (card.state() === "pending") card.settle({ name: card.name, outcome: "declined" });
      }
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
      message(event.author, event.text, event.context, event.selectedSkill, event.source, event.images); break;
    }
    case "secretState": {
      const card = secretCards.get(event.requestId);
      if (card) card.update(event.state, event.secret, event.message);
      else if (event.state === "error") { commandError.textContent = event.message ?? "Secret action failed."; commandError.hidden = false; }
      if (event.state === "revoked" || event.state === "saved") {
        secretsList = secretsList.filter(item => item.name !== event.name);
        if (event.state === "saved" && event.secret) secretsList.push(event.secret);
        for (const other of secretCards.values()) if (other !== card && other.name === event.name) other.reconcile(secretsList);
        if (!secretsPop.hidden) renderSecrets();
      }
      renderSecretsButton(); updateControls();
      break;
    }
    case "secrets": {
      secretsList = event.items;
      for (const card of secretCards.values()) card.reconcile(event.items);
      if (!secretsPop.hidden) renderSecrets(event.error);
      renderSecretsButton();
      break;
    }
    case "imageData": showImageData(event.attachmentId, event.url); break;
    case "questionState": {
      const card = questionCards.get(event.questionCallId);
      if (card && !card.answered) {
        for (const control of card.root.querySelectorAll<HTMLButtonElement | HTMLInputElement>("button,input")) control.disabled = readOnly;
        card.outcome.textContent = event.error;
      }
      break;
    }
    case "sendResult": {
      if (pendingSendText === event.text) { sending = false; pendingSendText = undefined; }
      // Matched on its own record, not on `pendingSendText`: a first message opens its
      // conversation mid-send, and `conversationOpened` clears `pendingSendText` before this
      // result arrives. Only the slots that went out leave; images added since stay.
      const submission = event.accepted ? event.submission : undefined;
      const sent = submission === undefined ? undefined : submittedImages.get(submission);
      if (submission !== undefined && sent) {
        submittedImages.delete(submission);
        pendingImages = pendingImages.filter(image => !sent.includes(image));
        renderPending();
      }
      if (event.accepted && prompt.value === event.text) { lastConsumedDraft = event.text; prompt.value = ""; }
      if (!event.accepted && event.restoreDraft && !prompt.value && lastConsumedDraft === event.text) {
        prompt.value = event.text;
        lastConsumedDraft = undefined;
      }
      updateControls();
      break;
    }
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
      card.outcome.textContent = permissionOutcome(event.resolution, event.message, event.alwaysSaved);
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
    case "favoriteAgents": {
      const keep = agentRows[agentIndex]?.id;
      agentFavorites = new Set(event.items.filter(name => typeof name === "string"));
      if (!agentPop.hidden) {
        renderAgents();
        const moved = agentRows.findIndex(row => row.id === keep);
        if (moved >= 0) { agentIndex = moved; renderAgents(); }
      }
      break;
    }
    case "favorites": {
      favorites = new Set(event.items.filter(name => typeof name === "string"));
      if (!palette.hidden && pickerKind === "model") renderPalette(); else favoriteAnchor = undefined;
      break;
    }
    case "usage": renderUsage(event.snapshot); break;
    case "turnStart": { const item = turn(event.id); item.startedAt = replaying ? undefined : Date.now(); setStatus("busy"); break; }
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
    case "tool": {
      const item = turn(event.id);
      activity(item, event);
      const thumbs = thumbnails(event.images);
      if (thumbs) item.root.append(thumbs);
      break;
    }
    case "question": {
      const item = turn(event.id);
      continueTurn(item);
      finishGroup(item);
      item.last = undefined;
      if (event.secret && event.questionCallId) {
        if (!secretCards.has(event.questionCallId)) {
          const card = createSecretCard(event.questionCallId, event.secret, event.agent ?? agentLabel.dataset.agent ?? "Nav", {
            post: identity.bind(rawPost),
            sessionId: () => sessionId,
            readOnly: () => readOnly,
            changed: () => { updateControls(); env.changed(); },
          });
          secretCards.set(event.questionCallId, card);
          item.root.append(card.root);
        }
        pendingQuestion = true; renderSecretsButton(); setStatus("ready"); follow(); break;
      }
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
      if (item.startedAt !== undefined && item.time) { item.time.textContent = ` · ${durationLabel(Date.now() - item.startedAt)}`; item.startedAt = undefined; }
      if (event.outcome !== "done") item.root.append(el("div", "turn-result", event.error || (event.outcome === "stopped" ? "Stopped" : "The turn failed.")));
      setStatus(event.outcome === "error" ? "error" : "ready", event.error);
      follow();
      break;
    }
  }
}

/** Rebuilds the model picker's items; a locked picker shows the open conversation's model. */
function renderModels(event: Extract<HostToWebview, { type: "models" }>): void {
  modelItems = event.items.filter(model => model.api === "chat" && model.enabled !== false).map(model => ({ name: model.name, description: model.upstream_id, gateway: model.gateway, provider_group: model.provider_group })).sort((a,b) => a.gateway.localeCompare(b.gateway) || a.provider_group.localeCompare(b.provider_group));
  if (event.current) currentModel = event.current;
  const known = event.items.find(model => model.name === currentModel);
  paintModelName();
  modelBtn.title = currentModel
    ? `${currentModel}${known?.contextWindow ? ` · ${tokens(known.contextWindow)} context` : ""}\nSwitch model · running turns finish before the switch`
    : "Switch model · running turns finish before the switch";
  modelBtn.setAttribute("aria-label", `Model: ${currentModel || "default"}`);
  modelBtn.disabled = readOnly || event.locked || event.items.length === 0;
  renderStrip(); renderMeta();
}

/** "model / variant", as the toolbar button shows it. */
function paintModelName(): void {
  modelNameEl.textContent = currentModel ? `${shortName(currentModel)}${currentVariant ? ` / ${currentVariant}` : ""}` : "Default model";
}

/** The next request's estimate, against the usable input budget. */
function tokens(count: number): string {
  return count >= 1000 ? `${Math.round(count / 1000)}k` : String(count);
}

/** The context ring of the header: filled by the share of the window the next request uses. */
function renderUsage(snapshot: import("../contextMeter").ContextSnapshot): void {
  const meter = contextMeter(snapshot);
  ring.hidden = false;
  ringFill.setAttribute("stroke-dasharray", `${(meter.fill / 100) * RING_LENGTH} ${RING_LENGTH}`);
  ring.dataset.level = meter.level;
  ringLabel.textContent = `${Math.round(meter.percent)}%`;
  ring.setAttribute("aria-valuenow", String(meter.fill));
  ring.setAttribute("aria-valuetext", meter.detail);
  ring.title = `${meter.label}\n${meter.detail}`;
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
      parent.append(node.lang === "diff" ? diffBlock(node.value) : node.lang === "mermaid" ? diagramCard(node.value) : codeBlock(node.lang || "Code", node.value, target => { target.textContent = node.value; }));
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

prompt.before(pendingList);
prompt.addEventListener("paste", event => {
  const files = Array.from(event.clipboardData?.files ?? []);
  if (!files.some(file => file.type.startsWith("image/"))) return;
  event.preventDefault();
  addImageFiles(files);
});
form.addEventListener("dragover", event => {
  if (!event.dataTransfer?.types.includes("Files") || prompt.disabled) return;
  event.preventDefault();
  form.classList.add("dropping");
});
form.addEventListener("dragleave", () => form.classList.remove("dropping"));
form.addEventListener("drop", event => {
  form.classList.remove("dropping");
  if (!event.dataTransfer?.files.length || prompt.disabled) return;
  event.preventDefault();
  addImageFiles(Array.from(event.dataTransfer.files));
});

form.addEventListener("submit", event => {
  event.preventDefault();
  const text = prompt.value;
  if ((!text.trim() && !pendingImages.length) || prompt.disabled || sending) return;
  // A message leaves with every image in the composer or not at all.
  if (imagesReading()) { commandError.textContent = "Wait until the images finish loading."; commandError.hidden = false; return; }
  closePalette();
  commandError.hidden = true;
  // Idle compaction holds its HTTP response until completion. It must not own
  // the ordinary message submission lock while the server queues input safely.
  if (!/^\/compact(?:\s|$)/.test(text)) { sending = true; pendingSendText = text; }
  const slots = pendingImages.slice();
  // An entry is spent once its images all left the composer (a later message carried them, or
  // the Captain removed them): no result can take anything away from it any more.
  for (const [id, sent] of submittedImages) if (!sent.some(slot => pendingImages.includes(slot))) submittedImages.delete(id);
  const submission = ++nextSubmission;
  if (slots.length) submittedImages.set(submission, slots);
  post({ type: "send", text, includeContext: contextOn(), submission, ...(slots.length ? { images: slots.map(({ name, data }) => ({ name, data: data ?? "" })) } : {}) });
  // Dropping the chip applies to one message only.
  skipNext = false; renderContext();
  updateControls();
  stickToBottom = true;
  follow();
});
prompt.addEventListener("input", () => { lastConsumedDraft = undefined; paletteDismissed = false; commandError.hidden = true; filterPalette(); updateControls(); });
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
renderContext(); renderStrip(); renderMeta(); agentSeg.render();
return {
  root,
  receive(event) { receive(event); env.changed(); },
  status: computeStatus,
  title: () => currentTitle,
  sessionId: () => sessionId,
  agent: () => String(agentLabel.dataset.agent ?? "build"),
  model: () => currentModel,
  setEditorContext(label) { contextLabel = label; renderContext(); },
  setMessageCount(count) { messageCount = count; renderMeta(); },
  focusComposer() { if (!prompt.disabled) prompt.focus(); },
  dispose() {
    for (const dispose of disposers.splice(0)) dispose();
    for (const card of secretCards.values()) card.dispose();
    document.removeEventListener("click", onSecretsOutside);
  },
};
}
