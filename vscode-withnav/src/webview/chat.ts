/// <reference lib="dom" />
import type { ConversationSummary, HostToWebview, TabInfo, WebviewToHost } from "../chatProtocol";
import { createConversation, type ConversationView } from "./conversation";
import { mountSessionList, type OpenTab } from "./conversations";
import { STATUS_LABEL, statusIcon } from "./statusIcon";

// The chat page. Several conversations live in it at once, each in its own `ConversationView`
// (header, transcript, composer); this shell keeps the tab strip, the session list and the routing
// of host messages by tab. In a subagent editor panel (`body.editor-tab`) it is one bare view.

type ViewState = { attach?: boolean; includeContext?: boolean; sidebar?: boolean };
declare function acquireVsCodeApi(): { postMessage(message: WebviewToHost): void; getState(): ViewState | undefined; setState(state: ViewState): void };
const vscode = acquireVsCodeApi();
const editor = document.body.classList.contains("editor-tab");
const MAX_VISIBLE_TABS = 4;
/** From this width (px) the session list stays open as a left column. */
const WIDE_AT = 900;

const tabbar = document.querySelector<HTMLElement>("#tabbar")!;
const viewsHost = document.querySelector<HTMLElement>("#views")!;
const sessionsHost = document.querySelector<HTMLElement>("#sessions")!;
const shellEl = document.querySelector<HTMLElement>(".shell")!;
/** The one sidebar toggle: pinned top-left of the view, in the same place whether the list is open or closed. */
const sidebarToggle = document.createElement("button");
sidebarToggle.type = "button"; sidebarToggle.className = "icon-button sidebar-toggle";
sidebarToggle.innerHTML = '<svg class="icon" viewBox="0 0 16 16" aria-hidden="true"><rect x="2.5" y="3" width="11" height="10" rx="1.5"/><path d="M6.5 3v10"/><path class="pane" d="M3 3.5h3.5v9H3z"/></svg>';
sidebarToggle.hidden = true;
shellEl.append(sidebarToggle);
const viewsEmpty = document.createElement("p");
viewsEmpty.className = "views-empty"; viewsEmpty.textContent = "Pick a session on the left, or start a new conversation.";
viewsHost.append(viewsEmpty);
tabbar.hidden = editor;

interface Entry { view: ConversationView; info: TabInfo }
const entries = new Map<string, Entry>();
let order: TabInfo[] = [];
let active: string | undefined;
let mode: "tab" | "list" = "tab";
let conversations: ConversationSummary[] = [];
let overflowOpen = false;
let wide = false;
let workspace = "";
const saved = (() => { try { return vscode.getState(); } catch { return undefined; } })();
let attach = saved?.attach ?? saved?.includeContext ?? true;
let sidebar = saved?.sidebar ?? true;

const hostPost = (message: WebviewToHost): void => vscode.postMessage(message);

function setAttach(value: boolean): void {
  attach = value;
  try { vscode.setState({ ...vscode.getState(), attach }); } catch { /* A private window keeps no state. */ }
}

function setSidebar(value: boolean): void {
  sidebar = value;
  try { vscode.setState({ ...vscode.getState(), sidebar }); } catch { /* A private window keeps no state. */ }
}
/** The list is a permanent column when wide (unless hidden, or there is no tab to show), else a full view. */
const sidebarShown = (): boolean => !editor && (wide ? sidebar || !order.length : mode === "list");

function makeEntry(info: TabInfo): Entry {
  const id = info.id;
  const view = createConversation({
    editor,
    post: message => hostPost(editor ? message : { ...message, tab: id }),
    getAttach: () => attach,
    setAttach,
    changed: chrome,
    showSessions: () => showList(),
  });
  if (!editor) { view.root.id = `panel-${id}`; view.root.setAttribute("role", "tabpanel"); view.root.setAttribute("aria-labelledby", `tab-${id}`); }
  viewsHost.append(view.root);
  return { view, info };
}

function showList(): void {
  if (wide) { setSidebar(true); render(); list.shown(); return; }
  mode = "list";
  render();
  list.shown();
}

/** `keepFocus`: a keyboard activation shows the panel and leaves focus on the tab, as the Tabs pattern asks. */
function focusTab(id: string, keepFocus = false): void {
  const entry = entries.get(id);
  if (!entry) return;
  const changed = active !== id;
  active = id; mode = "tab"; overflowOpen = false;
  if (changed) hostPost({ type: "focusTab", tab: id });
  if (keepFocus) tabFocusTarget = id;
  render();
  if (!keepFocus) entry.view.focusComposer();
}

function closeTab(id: string): void { hostPost({ type: "closeTab", tab: id }); }

const list = mountSessionList(sessionsHost, {
  open: sessionId => {
    const existing = order.find(tab => sessionOf(tab) === sessionId);
    if (existing) focusTab(existing.id); else hostPost({ type: "openTab", sessionId });
  },
  create: () => hostPost({ type: "openTab" }),
  remove: sessionId => hostPost({ type: "deleteConversation", id: sessionId }),
  refresh: () => hostPost({ type: "listConversations" }),
});

/** Tabs that hold a saved session, by session id: what the list overlays on the server's rows. */
const sessionOf = (tab: TabInfo): string => entries.get(tab.id)?.view.sessionId() || tab.sessionId || "";

function openTabs(): Map<string, OpenTab> {
  const open = new Map<string, OpenTab>();
  for (const tab of order) {
    const view = entries.get(tab.id)?.view;
    const sessionId = sessionOf(tab);
    if (view && sessionId) open.set(sessionId, { tab: tab.id, status: view.status(), agent: view.agent(), model: view.model() });
  }
  return open;
}

// ---- Rendering --------------------------------------------------------------------------------
let tabSignature = "";
let listSignature = "";
/** The tab that takes focus once the strip is rebuilt: set by a keyboard action, consumed by `renderTabbar`. */
let tabFocusTarget: string | undefined;

function tabLabel(tab: TabInfo): string { return entries.get(tab.id)?.view.title() || tab.title || "New conversation"; }

/** Up to four tabs are shown; the focused one always is, the rest go into the overflow menu. */
function splitTabs(): { shown: TabInfo[]; hidden: TabInfo[] } {
  if (order.length <= MAX_VISIBLE_TABS) return { shown: order, hidden: [] };
  const shownIds = new Set(order.slice(0, MAX_VISIBLE_TABS).map(tab => tab.id));
  if (active && !shownIds.has(active)) {
    shownIds.delete([...shownIds].at(-1)!);
    shownIds.add(active);
  }
  return { shown: order.filter(tab => shownIds.has(tab.id)), hidden: order.filter(tab => !shownIds.has(tab.id)) };
}

function tabElement(tab: TabInfo, inMenu: boolean): HTMLElement {
  const entry = entries.get(tab.id);
  const status = entry?.view.status() ?? "idle";
  const root = document.createElement("div");
  root.className = inMenu ? "tab tab-menu-item" : "tab";
  root.dataset.tab = tab.id; root.dataset.status = status;
  const selected = (wide || mode === "tab") && tab.id === active;
  // The tab (or menu item) is the focusable button itself; the wrapper only lays out the close button.
  root.setAttribute("role", "presentation");
  root.classList.toggle("active", selected);
  const main = document.createElement("button");
  main.type = "button"; main.className = "tab-main";
  main.setAttribute("role", inMenu ? "menuitem" : "tab");
  if (!inMenu) {
    main.id = `tab-${tab.id}`; main.setAttribute("aria-controls", `panel-${tab.id}`);
    main.setAttribute("aria-selected", String(selected));
  }
  const label = tabLabel(tab);
  const title = document.createElement("span");
  title.className = "tab-title"; title.textContent = label;
  main.append(statusIcon(status), title);
  main.title = `${label} · ${STATUS_LABEL[status]}`;
  main.addEventListener("click", event => focusTab(tab.id, !inMenu && event.detail === 0));
  const close = document.createElement("button");
  // Mouse only: the keyboard closes the focused tab with Delete.
  close.type = "button"; close.className = "tab-close"; close.tabIndex = -1; close.setAttribute("aria-hidden", "true");
  close.title = "Close";
  close.innerHTML = '<svg class="icon" viewBox="0 0 16 16" aria-hidden="true"><path d="m4.5 4.5 7 7m0-7-7 7"/></svg>';
  close.addEventListener("click", event => { event.stopPropagation(); closeTab(tab.id); });
  root.addEventListener("auxclick", event => { if (event.button === 1) { event.preventDefault(); closeTab(tab.id); } });
  root.append(main, close);
  return root;
}

/** Tabs pattern keys: arrows wrap, Home/End jump, Delete closes. Moving focus does not activate; Enter or Space does. */
function tabKeys(event: KeyboardEvent): void {
  if (event.isComposing || event.altKey || event.ctrlKey || event.metaKey) return;
  const buttons = [...tabbar.querySelectorAll<HTMLButtonElement>(".tab-strip .tab-main")];
  const index = buttons.indexOf(event.target as HTMLButtonElement);
  if (index < 0) return;
  const idOf = (button: HTMLButtonElement | undefined): string | undefined => button?.closest<HTMLElement>(".tab")?.dataset.tab;
  const moveTo = (to: number): void => {
    for (const [i, button] of buttons.entries()) button.tabIndex = i === to ? 0 : -1;
    buttons[to].focus();
  };
  switch (event.key) {
    case "ArrowRight": moveTo((index + 1) % buttons.length); break;
    case "ArrowLeft": moveTo((index - 1 + buttons.length) % buttons.length); break;
    case "Home": moveTo(0); break;
    case "End": moveTo(buttons.length - 1); break;
    case "Delete": {
      const id = idOf(buttons[index]);
      // Focus follows to the next tab (the previous one for the last), not to the composer.
      tabFocusTarget = idOf(buttons[index + 1]) ?? idOf(buttons[index - 1]);
      if (id !== undefined) closeTab(id);
      break;
    }
    default: return;
  }
  event.preventDefault();
}

function renderTabbar(): void {
  if (editor) return;
  const { shown, hidden } = splitTabs();
  const signature = JSON.stringify([mode, wide, active, overflowOpen, order.map(tab => [tab.id, tabLabel(tab), entries.get(tab.id)?.view.status()]), shown.map(tab => tab.id)]);
  const pending = tabFocusTarget;
  tabFocusTarget = undefined;
  if (signature === tabSignature) {
    // Nothing to rebuild (activating the selected tab again), but the keyboard intent still lands.
    if (pending !== undefined) [...tabbar.querySelectorAll<HTMLElement>(".tab-strip .tab")].find(tab => tab.dataset.tab === pending)?.querySelector<HTMLElement>(".tab-main")?.focus();
    return;
  }
  tabSignature = signature;
  const focused = document.activeElement?.closest<HTMLElement>(".tab-strip .tab")?.dataset.tab;
  const refocus = pending ?? focused;
  tabbar.replaceChildren();
  const strip = document.createElement("div");
  strip.className = "tab-strip"; strip.setAttribute("role", "tablist"); strip.setAttribute("aria-label", "Open conversations");
  for (const tab of shown) strip.append(tabElement(tab, false));
  // Roving tabindex: one tab is in the Tab order, the selected one unless focus is being kept on another.
  const tabButtons = [...strip.querySelectorAll<HTMLButtonElement>(".tab-main")];
  const stop = tabButtons.find(button => button.closest<HTMLElement>(".tab")?.dataset.tab === refocus) ?? tabButtons.find(button => button.getAttribute("aria-selected") === "true") ?? tabButtons[0];
  for (const button of tabButtons) button.tabIndex = button === stop ? 0 : -1;
  strip.addEventListener("keydown", tabKeys);
  tabbar.append(strip);
  if (hidden.length) {
    const more = document.createElement("button");
    more.type = "button"; more.className = "tab-overflow";
    more.setAttribute("aria-haspopup", "menu"); more.setAttribute("aria-expanded", String(overflowOpen));
    more.title = `${hidden.length} more conversations`;
    more.textContent = `» ${hidden.length}`;
    more.addEventListener("click", event => { event.stopPropagation(); overflowOpen = !overflowOpen; tabSignature = ""; renderTabbar(); });
    tabbar.append(more);
    if (overflowOpen) {
      const menu = document.createElement("div");
      menu.className = "tab-menu"; menu.setAttribute("role", "menu");
      for (const tab of hidden) menu.append(tabElement(tab, true));
      tabbar.append(menu);
    }
  }
  const add = document.createElement("button");
  add.type = "button"; add.className = "icon-button tab-new";
  add.title = "New conversation"; add.setAttribute("aria-label", "New conversation");
  add.innerHTML = '<svg class="icon" viewBox="0 0 16 16" aria-hidden="true"><path d="M8 3.5v9M3.5 8h9"/></svg>';
  add.addEventListener("click", () => hostPost({ type: "openTab" }));
  tabbar.append(add);
  if (refocus !== undefined) stop?.focus();
}

function renderList(): void {
  if (!sidebarShown()) return;
  const tabs = openTabs();
  const activeTab = wide || mode === "tab" ? active : undefined;
  const signature = JSON.stringify([conversations, [...tabs], workspace, activeTab]);
  if (signature === listSignature) return;
  listSignature = signature;
  list.update(conversations, tabs, { workspace, activeTab });
}

/** Cheap enough to run on every message: each part redraws only when its inputs changed. */
function chrome(): void { renderTabbar(); renderList(); }

function render(): void {
  for (const [id, entry] of entries) entry.view.root.hidden = !((wide || mode === "tab") && id === active);
  const side = sidebarShown();
  sessionsHost.hidden = !side;
  viewsHost.hidden = !wide && mode === "list";
  viewsEmpty.hidden = order.length > 0;
  sidebarToggle.hidden = editor || !wide;
  sidebarToggle.disabled = !order.length;
  sidebarToggle.setAttribute("aria-pressed", String(side));
  sidebarToggle.setAttribute("aria-label", side ? "Hide sessions" : "Show sessions");
  sidebarToggle.title = side ? "Hide sessions" : "Show sessions";
  shellEl.dataset.wide = String(wide); shellEl.dataset.sidebar = side ? "on" : "off";
  document.body.dataset.mode = mode;
  listSignature = "";
  chrome();
}

sidebarToggle.addEventListener("click", () => {
  if (sidebarShown() && order.length) { setSidebar(false); render(); } else showList();
});

/** Container width, not the window's: the view can be a narrow side bar or a maximized one. */
function measure(): void {
  const width = shellEl.clientWidth || document.documentElement.clientWidth;
  const next = !editor && width >= WIDE_AT;
  if (next === wide) return;
  wide = next;
  tabSignature = "";
  render();
}
window.addEventListener("resize", measure);
if (typeof ResizeObserver !== "undefined") new ResizeObserver(measure).observe(shellEl);

document.addEventListener("click", () => { if (overflowOpen) { overflowOpen = false; tabSignature = ""; renderTabbar(); } });

// ---- Host messages ----------------------------------------------------------------------------
function applyTabs(tabs: TabInfo[], nextActive: string | undefined): void {
  // A keyboard close in the strip keeps focus there; any other change of the active tab moves it to the composer.
  const keepStrip = tabFocusTarget !== undefined;
  const ids = new Set(tabs.map(tab => tab.id));
  for (const [id, entry] of entries) {
    if (ids.has(id)) continue;
    entry.view.dispose(); entry.view.root.remove(); entries.delete(id);
  }
  let created = false;
  for (const info of tabs) {
    const existing = entries.get(info.id);
    if (existing) existing.info = info; else { entries.set(info.id, makeEntry(info)); created = true; }
  }
  order = tabs;
  const previous = active;
  active = nextActive && ids.has(nextActive) ? nextActive : tabs.find(tab => tab.id === active)?.id ?? tabs[0]?.id;
  if (!tabs.length) mode = "list";
  else if (created || active !== previous) mode = "tab";
  tabSignature = "";
  render();
  if (!tabs.length) list.shown();
  else if (mode === "tab" && (created || active !== previous) && !keepStrip) entries.get(active!)?.view.focusComposer();
  tabFocusTarget = undefined;
}

function applyConversations(items: ConversationSummary[], name?: string): void {
  conversations = items;
  if (name) workspace = name;
  for (const entry of entries.values()) {
    const row = items.find(item => item.id === (entry.view.sessionId() || entry.info.sessionId));
    entry.view.setMessageCount(row?.messageCount);
  }
  listSignature = "";
  chrome();
}

function route(data: HostToWebview): void {
  switch (data.type) {
    case "tabs": applyTabs(data.tabs, data.active); return;
    case "conversations": applyConversations(data.items, data.workspace); return;
    case "appearance": if (Number.isFinite(data.fontSize) && data.fontSize >= 11 && data.fontSize <= 22) document.documentElement.style.setProperty("--nav-font-size", `${data.fontSize}px`); return;
    case "showSessions": if (wide && sidebarShown() && order.length) { setSidebar(false); render(); } else showList(); return;
    case "editorContext": for (const entry of entries.values()) entry.view.setEditorContext(data.label); return;
    case "favorites": case "favoriteAgents": for (const entry of entries.values()) entry.view.receive(data); return;
  }
  const target = editor ? entries.get("") : data.tab !== undefined ? entries.get(data.tab) : undefined;
  target?.view.receive(data);
}

window.addEventListener("message", event => {
  if (event.data && typeof event.data.type === "string") route(event.data as HostToWebview);
});

if (editor) {
  entries.set("", makeEntry({ id: "", title: "Subagent" }));
  order = [{ id: "", title: "Subagent" }];
  active = "";
  render();
}
measure();
hostPost({ type: "ready" });
