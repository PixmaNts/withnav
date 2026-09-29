/// <reference lib="dom" />
/// <reference lib="dom.iterable" />
import type { ConversationSummary, HostToWebview, WebviewToHost } from "../chatProtocol";

// Header (current title, conversation switcher, "New conversation") and the saved
// conversation list. The chat view's static markup belongs to the host, so these
// controls are built here and mounted into the existing top bar.

const SVG = "http://www.w3.org/2000/svg";
const NEW_TITLE = "New conversation";
const DISARM_MS = 4000;

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

function node<K extends keyof HTMLElementTagNameMap>(tag: K, className?: string, text?: string): HTMLElementTagNameMap[K] {
  const element = document.createElement(tag);
  if (className) element.className = className;
  if (text !== undefined) element.textContent = text;
  return element;
}

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

export function mountConversations(post: (message: WebviewToHost) => void, focusComposer: () => void): (event: HostToWebview) => boolean {
  const shell = document.querySelector<HTMLElement>(".shell")!;
  const topbar = document.querySelector<HTMLElement>(".topbar")!;
  const status = document.querySelector<HTMLElement>("#status")!;

  let items: ConversationSummary[] = [];
  let loaded = false;
  let currentId: string | undefined;
  let currentTitle = NEW_TITLE;
  let armed: string | undefined;
  let disarmTimer: ReturnType<typeof setTimeout> | undefined;

  // Header: [chevron + title] ......... [status] [+]
  const trigger = node("button", "conv-trigger");
  trigger.type = "button";
  trigger.setAttribute("aria-haspopup", "true");
  trigger.setAttribute("aria-expanded", "false");
  trigger.setAttribute("aria-controls", "conv-panel");
  const titleText = node("span", "conv-title", NEW_TITLE);
  trigger.append(titleText, icon("M4.5 6.5 8 10l3.5-3.5"));

  const create = node("button", "icon-button conv-new");
  create.type = "button";
  create.title = NEW_TITLE;
  create.setAttribute("aria-label", NEW_TITLE);
  create.append(icon("M8 3.5v9M3.5 8h9"));

  const tail = node("div", "topbar-tail");
  topbar.querySelector(".identity")?.remove();
  topbar.prepend(trigger);
  status.replaceWith(tail);
  tail.append(status, create);

  // Panel, anchored under the top bar, with a scrim over the transcript and composer.
  const panel = node("section", "conv-panel");
  panel.id = "conv-panel";
  panel.tabIndex = -1;
  panel.hidden = true;
  panel.setAttribute("aria-label", "Conversations");
  const heading = node("div", "conv-heading");
  heading.append(node("span", "", "Conversations"), node("span", "conv-keys", "↑↓ Enter · Del · Esc"));
  const list = node("ul", "conv-list");
  const note = node("p", "conv-note");
  panel.append(heading, list, note);
  topbar.append(panel);

  const scrim = node("div", "conv-scrim");
  scrim.hidden = true;
  shell.append(scrim);

  function setTitle(title: string): void {
    currentTitle = title.trim() || (currentId ? "Untitled" : NEW_TITLE);
    titleText.textContent = currentTitle;
    trigger.title = `${currentTitle} · Show conversations`;
    trigger.setAttribute("aria-label", `${currentTitle}. Show conversations`);
  }
  setTitle(NEW_TITLE);

  // While the list is open, the transcript and composer under the scrim leave the tab order.
  function setInert(on: boolean): void {
    for (const element of shell.querySelectorAll<HTMLElement>(":scope > :not(.topbar):not(.conv-scrim)")) element.inert = on;
  }

  const isOpen = (): boolean => !panel.hidden;
  const rows = (): HTMLButtonElement[] => [...list.querySelectorAll<HTMLButtonElement>(".conv-open")];

  function open(): void {
    if (isOpen()) return;
    panel.hidden = false;
    scrim.hidden = false;
    setInert(true);
    trigger.setAttribute("aria-expanded", "true");
    render();
    post({ type: "listConversations" });
    focusInitial();
  }

  function close(returnFocus = true): void {
    if (!isOpen()) return;
    disarm();
    panel.hidden = true;
    scrim.hidden = true;
    setInert(false);
    trigger.setAttribute("aria-expanded", "false");
    if (returnFocus) focusComposer();
  }

  function focusInitial(): void {
    const all = rows();
    (all.find(row => row.dataset.id === currentId) ?? all[0] ?? panel).focus();
  }

  function disarm(): void {
    if (disarmTimer) clearTimeout(disarmTimer);
    disarmTimer = undefined;
    if (armed === undefined) return;
    armed = undefined;
    for (const button of list.querySelectorAll<HTMLButtonElement>(".conv-delete.armed")) paintDelete(button, false);
  }

  function paintDelete(button: HTMLButtonElement, on: boolean): void {
    button.classList.toggle("armed", on);
    button.closest(".conv-row")?.classList.toggle("arming", on);
    const title = button.dataset.title ?? "";
    button.replaceChildren(on ? node("span", "", "Delete") : icon("M3 4.5h10M6.5 4.5V3h3v1.5M4.5 4.5l.6 8.5h5.8l.6-8.5"));
    button.setAttribute("aria-label", on ? `Confirm: delete ${title}` : `Delete ${title}`);
    button.title = on ? "Click again to delete" : "Delete";
  }

  function remove(id: string): void {
    const index = items.findIndex(item => item.id === id);
    disarm();
    post({ type: "deleteConversation", id });
    if (index < 0) return;
    items = items.filter(item => item.id !== id);
    render();
    const all = rows();
    (all[Math.min(index, all.length - 1)] ?? panel).focus();
  }

  function armOrDelete(button: HTMLButtonElement, id: string): void {
    if (armed === id) { remove(id); return; }
    disarm();
    armed = id;
    paintDelete(button, true);
    disarmTimer = setTimeout(disarm, DISARM_MS);
  }

  function render(): void {
    const focused = document.activeElement as HTMLElement | null;
    const focusId = focused?.closest<HTMLElement>(".conv-row")?.dataset.id;
    const focusDelete = focused?.classList.contains("conv-delete") ?? false;
    if (armed && !items.some(item => item.id === armed)) armed = undefined;

    const now = new Date();
    list.replaceChildren();
    for (const item of items) {
      const row = node("li", "conv-row");
      row.dataset.id = item.id;
      const current = item.id === currentId;
      row.classList.toggle("current", current);

      const openButton = node("button", "conv-open");
      openButton.type = "button";
      openButton.dataset.id = item.id;
      if (current) openButton.setAttribute("aria-current", "true");
      const title = item.title.trim() || "Untitled";
      const when = relativeTime(item.updatedAt, now);
      const count = `${item.messageCount} ${item.messageCount === 1 ? "message" : "messages"}`;
      const meta = node("span", "conv-meta");
      const time = node("time", "", when);
      time.dateTime = item.updatedAt;
      const stamp = new Date(item.updatedAt);
      if (!Number.isNaN(stamp.getTime())) time.title = full.format(stamp);
      meta.append(time, node("span", "conv-dot", "·"), node("span", "", count));
      openButton.append(node("span", "conv-row-title", title), meta);
      openButton.setAttribute("aria-label", `${title}, ${when}, ${count}${current ? ", current" : ""}`);
      openButton.addEventListener("click", () => {
        if (item.id !== currentId) post({ type: "openConversation", id: item.id });
        close();
      });

      const deleteButton = node("button", "icon-button conv-delete");
      deleteButton.type = "button";
      deleteButton.dataset.title = title;
      paintDelete(deleteButton, armed === item.id);
      deleteButton.addEventListener("click", event => {
        event.stopPropagation();
        armOrDelete(deleteButton, item.id);
      });
      deleteButton.addEventListener("blur", event => {
        // A re-render detaches the button without the user moving focus: keep the arm then.
        if (armed === item.id && event.relatedTarget) disarm();
      });

      row.append(openButton, deleteButton);
      list.append(row);
    }

    list.hidden = items.length === 0;
    note.hidden = items.length > 0;
    note.textContent = loaded ? "No saved conversations yet." : "Loading…";

    if (!isOpen() || !focusId) return;
    const target = list.querySelector<HTMLElement>(`.conv-row[data-id="${CSS.escape(focusId)}"] ${focusDelete ? ".conv-delete" : ".conv-open"}`);
    target?.focus();
  }

  trigger.addEventListener("click", () => (isOpen() ? close() : open()));
  create.addEventListener("click", () => {
    close(false);
    post({ type: "newConversation" });
    focusComposer();
  });
  scrim.addEventListener("click", () => close());

  panel.addEventListener("keydown", event => {
    const all = rows();
    const active = document.activeElement as HTMLElement | null;
    const row = active?.closest<HTMLElement>(".conv-row");
    const index = row ? all.findIndex(button => button.dataset.id === row.dataset.id) : -1;
    const move = (to: number): void => { if (all.length) all[(to + all.length) % all.length].focus(); };
    switch (event.key) {
      case "ArrowDown": move(index + 1); break;
      case "ArrowUp": move(index < 0 ? -1 : index - 1); break;
      case "Home": move(0); break;
      case "End": move(-1); break;
      case "ArrowRight": row?.querySelector<HTMLElement>(".conv-delete")?.focus(); break;
      case "ArrowLeft": row?.querySelector<HTMLElement>(".conv-open")?.focus(); break;
      case "Delete":
      case "Backspace": {
        const button = row?.querySelector<HTMLButtonElement>(".conv-delete");
        if (!button || !row?.dataset.id) return;
        if (armed !== row.dataset.id) button.focus();
        armOrDelete(button, row.dataset.id);
        break;
      }
      case "Escape": close(); break;
      default: return;
    }
    event.preventDefault();
    event.stopPropagation();
  });
  // Tabbing out of the list closes it; the transcript under the scrim is not reachable while open.
  panel.addEventListener("focusout", event => {
    const next = event.relatedTarget as Node | null;
    if (next && !panel.contains(next) && next !== trigger) close(false);
  });
  document.addEventListener("keydown", event => {
    if (event.key === "Escape" && isOpen()) { event.preventDefault(); close(); }
  });

  return event => {
    switch (event.type) {
      case "conversationOpened":
        currentId = event.id;
        setTitle(event.id === undefined ? NEW_TITLE : event.title);
        if (isOpen()) render();
        return true;
      case "conversations": {
        const hadNone = !loaded || items.length === 0;
        items = [...event.items].sort((a, b) => Date.parse(b.updatedAt) - Date.parse(a.updatedAt));
        loaded = true;
        // The host's currentId is authoritative (undefined = a new, unsaved conversation).
        currentId = event.currentId;
        const current = items.find(item => item.id === currentId);
        if (current) setTitle(current.title);
        render();
        if (isOpen() && hadNone && document.activeElement === panel) focusInitial();
        return true;
      }
      default:
        return false;
    }
  };
}
