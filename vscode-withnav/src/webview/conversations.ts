/// <reference lib="dom" />
/// <reference lib="dom.iterable" />
import { displayTitle } from "../sessionTitle";
import type { ConversationSummary } from "../chatProtocol";
import { ranked, PICKER_LIMIT } from "./pickerSearch";
import { STATUS_LABEL, statusIcon, type SessionStatus } from "./statusIcon";

// The session list: every saved conversation of the workspace, with what is known of its state.
// The server's list gives the agent, the message count, whether a runtime is live and the status
// of subagent sessions. A conversation open in a tab also reports its own status and model.

const SVG = "http://www.w3.org/2000/svg";
const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const full = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" });

/** "just now", "3 min ago", "5 h ago", "yesterday", "12 Sep", "12 Sep 2025". */
export function relativeTime(iso: string, now = new Date()): string {
  const then = new Date(iso);
  if (Number.isNaN(then.getTime())) return "";
  const seconds = (now.getTime() - then.getTime()) / 1000;
  if (seconds < 45) return "just now";
  if (seconds < 3600) return `${Math.max(1, Math.round(seconds / 60))} min ago`;
  const startOfToday = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime();
  if (then.getTime() >= startOfToday) return `${Math.floor(seconds / 3600)} h ago`;
  if (then.getTime() >= startOfToday - 86_400_000) return "yesterday";
  const day = `${then.getDate()} ${MONTHS[then.getMonth()]}`;
  return then.getFullYear() === now.getFullYear() ? day : `${day} ${then.getFullYear()}`;
}

/** What the page knows of a conversation that is open in a tab. */
export interface OpenTab { tab: string; status: Exclude<SessionStatus, "active">; agent?: string; model?: string }

export type SessionFilter = "all" | "active" | "waiting" | "errors";

/** The icon of a row: an open tab knows best; otherwise only the server's `live` flag tells. */
export function sessionStatus(item: ConversationSummary, open?: OpenTab): SessionStatus {
  if (open) return open.status === "idle" ? "done" : open.status;
  return item.live ? "active" : "done";
}

/** Whether a row belongs to a filter chip. Failed subagent sessions (from the server's `child_status`) count as errors. */
export function matchesFilter(item: ConversationSummary, open: OpenTab | undefined, filter: SessionFilter): boolean {
  const status = sessionStatus(item, open);
  switch (filter) {
    case "all": return true;
    case "active": return status === "running" || status === "active";
    case "waiting": return status === "waiting";
    case "errors": return status === "error" || (item.subagentsFailed ?? 0) > 0;
  }
}

export function filterCounts(items: ConversationSummary[], tabs: Map<string, OpenTab>): Record<SessionFilter, number> {
  const count = (filter: SessionFilter): number => items.filter(item => matchesFilter(item, tabs.get(item.id), filter)).length;
  return { all: items.length, active: count("active"), waiting: count("waiting"), errors: count("errors") };
}

/** Rows matching the chip and the search text (title or id), newest first; waiting ones are returned apart. */
export function visibleSessions(items: ConversationSummary[], tabs: Map<string, OpenTab>, filter: SessionFilter, query: string): { waiting: ConversationSummary[]; rest: ConversationSummary[]; matches: number } {
  const sorted = [...items].sort((a, b) => Date.parse(b.updatedAt) - Date.parse(a.updatedAt));
  const byFilter = sorted.filter(item => matchesFilter(item, tabs.get(item.id), filter));
  const matches = ranked(byFilter, query, item => item.title, item => item.id);
  const waiting = matches.filter(item => sessionStatus(item, tabs.get(item.id)) === "waiting");
  const rest = matches.filter(item => !waiting.includes(item));
  return { waiting, rest, matches: matches.length };
}

const node = <K extends keyof HTMLElementTagNameMap>(tag: K, className?: string, text?: string): HTMLElementTagNameMap[K] => {
  const element = document.createElement(tag);
  if (className) element.className = className;
  if (text !== undefined) element.textContent = text;
  return element;
};

function icon(d: string): SVGSVGElement {
  const svg = document.createElementNS(SVG, "svg");
  svg.setAttribute("viewBox", "0 0 16 16");
  svg.setAttribute("aria-hidden", "true");
  svg.classList.add("icon");
  const path = document.createElementNS(SVG, "path");
  path.setAttribute("d", d);
  svg.append(path);
  return svg;
}

export interface SessionListApi {
  open(sessionId: string): void;
  create(): void;
  remove(sessionId: string): void;
  refresh(): void;
}
export interface SessionList {
  /** Redraws from the latest list and the tabs that hold sessions. */
  update(items: ConversationSummary[], tabs: Map<string, OpenTab>, options?: { workspace?: string; activeTab?: string }): void;
  /** The list became visible: fetch a fresh one and focus the search. */
  shown(): void;
}

export function mountSessionList(host: HTMLElement, api: SessionListApi): SessionList {
  let items: ConversationSummary[] = [];
  let tabs = new Map<string, OpenTab>();
  let loaded = false;
  let filter: SessionFilter = "all";
  let selected = 0;
  let workspace = "";
  let activeTab: string | undefined;
  let collapsed = false;

  host.replaceChildren();
  const head = node("div", "sl-head");
  const search = node("input", "picker-search sl-search"); search.type = "search"; search.placeholder = "Search sessions…";
  search.setAttribute("aria-label", "Search sessions"); search.setAttribute("role", "combobox"); search.setAttribute("aria-autocomplete", "list"); search.setAttribute("aria-controls", "session-waiting-options session-options"); search.setAttribute("aria-expanded", "true");
  const create = node("button", "icon-button sl-new"); create.type = "button"; create.title = "New conversation"; create.setAttribute("aria-label", "New conversation");
  create.append(icon("M8 3.5v9M3.5 8h9"));
  head.append(search);
  const group = node("div", "sl-group");
  const groupToggle = node("button", "sl-group-toggle"); groupToggle.type = "button"; groupToggle.setAttribute("aria-expanded", "true");
  const groupName = node("span", "sl-group-name", "Sessions");
  groupToggle.append(icon("M4 6l4 4 4-4"), groupName);
  create.classList.add("sl-group-new");
  group.append(groupToggle, create);
  const chipRow = node("div", "sl-chips"); chipRow.setAttribute("role", "group"); chipRow.setAttribute("aria-label", "Filter sessions");
  const scroll = node("div", "sl-scroll");
  const waitingSection = node("section", "sl-section sl-waiting");
  const waitingHeading = node("h3", "sl-heading", "Waiting for you");
  const waitingList = node("ul", "sl-list"); waitingList.id = "session-waiting-options"; waitingList.setAttribute("role", "listbox"); waitingList.setAttribute("aria-label", "Sessions waiting for you");
  waitingSection.append(waitingHeading, waitingList);
  const restSection = node("section", "sl-section");
  const restHeading = node("h3", "sl-heading", "Sessions");
  const restList = node("ul", "sl-list"); restList.id = "session-options"; restList.setAttribute("role", "listbox"); restList.setAttribute("aria-label", "Sessions");
  restSection.append(restHeading, restList);
  const count = node("div", "picker-count"); count.setAttribute("role", "status"); count.setAttribute("aria-live", "polite");
  const note = node("p", "sl-note");
  scroll.append(waitingSection, group, restSection, count, note);
  host.append(head, chipRow, scroll);

  /** The rows on screen: a collapsed section renders none, so arrows and Enter never reach a hidden one. */
  const rows = (): HTMLLIElement[] => [...host.querySelectorAll<HTMLLIElement>(".sl-row")];

  function row(item: ConversationSummary, index: number, now: Date): HTMLLIElement {
    const open = tabs.get(item.id);
    const status = sessionStatus(item, open);
    const li = node("li", "sl-row");
    li.id = `session-option-${index}`; li.setAttribute("role", "option"); li.setAttribute("aria-selected", String(index === selected));
    li.dataset.id = item.id; li.dataset.status = status;
    li.classList.toggle("is-open", !!open);
    li.classList.toggle("is-active", !!open && open.tab === activeTab);
    // The option holds no focusable control: the search field keeps focus (aria-activedescendant) and the
    // keys act on the selected row. The delete button below is for the mouse only.
    const button = node("div", "sl-open"); button.dataset.id = item.id;
    const title = displayTitle(item.title, item.id);
    const when = relativeTime(item.updatedAt, now);
    const time = node("time", "sl-time", when); time.dateTime = item.updatedAt;
    const stamp = new Date(item.updatedAt);
    if (!Number.isNaN(stamp.getTime())) time.title = full.format(stamp);
    const line1 = node("span", "sl-line1");
    line1.append(node("span", "sl-title", title), time);
    const parts: string[] = [];
    const agent = open?.agent ?? item.agent;
    if (agent) parts.push(agent);
    if (open?.model) parts.push(open.model.split("/").at(-1) || open.model);
    parts.push(`${item.messageCount} ${item.messageCount === 1 ? "msg" : "msgs"}`);
    const subagents = item.subagentCount ?? 0;
    if (subagents) parts.push(`${subagents} ${subagents === 1 ? "subagent" : "subagents"}`);
    if (item.subagentsRunning) parts.push(`${item.subagentsRunning} running`);
    const line2 = node("span", "sl-line2", parts.join(" · "));
    if (item.subagentsFailed) line2.append(node("span", "sl-failed", ` · ${item.subagentsFailed} failed`));
    const main = node("span", "sl-main");
    main.append(line1, line2);
    button.append(statusIcon(status), main);
    if (open) {
      const mark = node("span", "sl-openmark", "open");
      mark.title = "Open in a tab";
      button.append(mark);
    }
    li.setAttribute("aria-label", `${title}, ${STATUS_LABEL[status]}, ${when}, ${parts.join(", ")}${open ? ", open in a tab" : ""}`);
    li.addEventListener("click", () => api.open(item.id));
    // Read by the host's `webview/context` menu, which offers Delete on a right click.
    li.dataset.vscodeContext = JSON.stringify({ webviewSection: "session", sessionId: item.id, preventDefaultContextMenuItems: true });
    const remove = node("button", "icon-button sl-delete"); remove.type = "button";
    remove.tabIndex = -1; remove.setAttribute("aria-hidden", "true");
    remove.title = "Delete";
    remove.append(icon("M3 4.5h10M6.5 4.5V3h3v1.5M4.5 4.5l.6 8.5h5.8l.6-8.5"));
    // The host asks for confirmation before anything is deleted.
    remove.addEventListener("click", event => { event.stopPropagation(); api.remove(item.id); });
    li.append(button, remove);
    return li;
  }

  function render(): void {
    const now = new Date();
    const counts = filterCounts(items, tabs);
    chipRow.replaceChildren();
    for (const [value, label] of [["all", "All"], ["active", "Active"], ["waiting", "Waiting for you"], ["errors", "Errors"]] as const) {
      const chip = node("button", "pill", `${label} ${counts[value]}`); chip.type = "button";
      chip.dataset.filter = value;
      chip.setAttribute("aria-pressed", String(filter === value));
      chip.addEventListener("click", () => { filter = value; selected = 0; render(); });
      chipRow.append(chip);
    }
    const view = visibleSessions(items, tabs, filter, search.value);
    const shownWaiting = view.waiting.slice(0, PICKER_LIMIT);
    const shownRest = collapsed ? [] : view.rest.slice(0, Math.max(0, PICKER_LIMIT - shownWaiting.length));
    const total = shownWaiting.length + shownRest.length;
    selected = Math.max(0, Math.min(selected, total - 1));
    waitingList.replaceChildren(...shownWaiting.map((item, index) => row(item, index, now)));
    restList.replaceChildren(...shownRest.map((item, index) => row(item, shownWaiting.length + index, now)));
    waitingSection.hidden = shownWaiting.length === 0;
    restSection.hidden = shownRest.length === 0;
    restHeading.hidden = true;
    groupName.textContent = workspace || "Sessions";
    groupToggle.setAttribute("aria-expanded", String(!collapsed));
    group.classList.toggle("collapsed", collapsed);
    count.textContent = `${view.matches} of ${items.length}${view.matches > PICKER_LIMIT ? " · Type to narrow (first 50 shown)" : ""}`;
    if (total) search.setAttribute("aria-activedescendant", `session-option-${selected}`); else search.removeAttribute("aria-activedescendant");
    note.hidden = total > 0 || (collapsed && view.matches > 0);
    note.textContent = search.value || filter !== "all" ? "No matching sessions." : loaded ? "No saved sessions yet." : "Loading…";
    host.querySelector(`#session-option-${selected}`)?.scrollIntoView?.({ block: "nearest" });
  }

  search.addEventListener("input", () => { selected = 0; render(); });
  search.addEventListener("keydown", event => {
    if (event.isComposing) { event.stopPropagation(); return; }
    if (["ArrowUp", "ArrowDown"].includes(event.key)) {
      event.preventDefault(); event.stopPropagation();
      const all = rows();
      if (all.length) { selected = (selected + (event.key === "ArrowDown" ? 1 : -1) + all.length) % all.length; render(); }
    } else if (event.key === "Enter") { event.preventDefault(); event.stopPropagation(); rows()[selected]?.click(); }
    else if (event.key === "Delete" && search.selectionStart === search.value.length && search.selectionEnd === search.value.length && rows()[selected]) {
      // Delete at the end of the text has nothing to edit, so it acts on the selected row; the host asks first.
      event.preventDefault(); event.stopPropagation(); api.remove(rows()[selected].dataset.id!);
    }
    else if (event.key === "Escape") { if (search.value) { event.preventDefault(); event.stopPropagation(); search.value = ""; selected = 0; render(); } }
    else event.stopPropagation(); // Home/End and editing keys belong to the search field.
  });
  create.addEventListener("click", () => api.create());
  groupToggle.addEventListener("click", () => { collapsed = !collapsed; render(); });
  render();
  return {
    update(nextItems, nextTabs, options) { items = nextItems; tabs = nextTabs; workspace = options?.workspace ?? workspace; activeTab = options?.activeTab; loaded = true; render(); },
    shown() { api.refresh(); render(); search.focus(); },
  };
}
