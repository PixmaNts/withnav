import { displayTitle } from "./sessionTitle";
import type { ChatBackend, ChatTabs, ConversationSummary, HostToWebview, TabInfo } from "./chatProtocol";
import type { ServerInfoParams } from "./protocol";

/** What the hub needs of a backend: the chat contract plus the endpoint announcement. */
export interface TabBackend extends ChatBackend {
  setServerInfo(info: ServerInfoParams): void;
}

export type ConversationBindingSender = (params: { sessionId: string; incarnation?: string; focus: boolean }) => void;

/** What survives a reload of the window: the sessions open in tabs (an empty tab has no id) and the focused one. */
export interface SavedTabs { sessions: (string | null)[]; active: number }
export interface TabStore { load(): SavedTabs | undefined; save(value: SavedTabs): void }

export const MAX_TABS = 12;
const LIST_REFRESH_MS = 800;

interface Tab { id: string; backend: TabBackend; title: string; sessionId?: string; off: () => void }

/**
 * One `ChatBackend` per open conversation tab, all talking to the same session server. Every
 * backend gets each endpoint announcement; messages are re-published tagged with their tab. A
 * `lister` backend (never shown) owns the workspace's session list and the settings requests.
 */
export class ChatHub implements ChatTabs {
  private readonly order: Tab[] = [];
  private activeId: string | undefined;
  private boundSessionId: string | undefined;
  private nextId = 0;
  private endpoint: ServerInfoParams | undefined;
  private list: ConversationSummary[] = [];
  private disposed = false;
  private refreshTimer: ReturnType<typeof setTimeout> | undefined;
  private readonly tabListeners = new Set<() => void>();
  private readonly messageListeners = new Set<(tab: string, message: HostToWebview) => void>();
  private readonly listListeners = new Set<(items: ConversationSummary[]) => void>();
  private readonly listerOff: () => void;

  constructor(
    private readonly create: () => TabBackend,
    private readonly lister: TabBackend,
    private readonly store?: TabStore,
    private readonly notifyError: (error: unknown) => void = () => undefined,
    private readonly notifyBinding?: ConversationBindingSender,
  ) {
    this.listerOff = lister.subscribe(message => this.acceptList(message));
    const saved = store?.load();
    for (const session of saved?.sessions.slice(0, MAX_TABS) ?? []) this.open(typeof session === "string" ? session : undefined, true);
    if (!this.order.length) this.open();
    const index = saved?.active ?? 0;
    if (this.order[index]) this.activeId = this.order[index].id;
    this.bindActive();
  }

  tabs(): TabInfo[] { return this.order.map(tab => ({ id: tab.id, title: tab.title, ...(tab.sessionId ? { sessionId: tab.sessionId } : {}) })); }
  active(): string | undefined { return this.activeId; }
  backend(tab: string): ChatBackend | undefined { return this.order.find(item => item.id === tab)?.backend; }
  activeBackend(): ChatBackend | undefined { return this.activeId ? this.backend(this.activeId) : undefined; }
  conversations(): ConversationSummary[] { return [...this.list]; }

  onTabs(listener: () => void): () => void { this.tabListeners.add(listener); return () => { this.tabListeners.delete(listener); }; }
  subscribe(listener: (tab: string, message: HostToWebview) => void): () => void { this.messageListeners.add(listener); return () => { this.messageListeners.delete(listener); }; }
  onConversations(listener: (items: ConversationSummary[]) => void): () => void { this.listListeners.add(listener); return () => { this.listListeners.delete(listener); }; }

  open(sessionId?: string, restoring = false): string {
    if (sessionId) {
      const existing = this.order.find(tab => tab.sessionId === sessionId);
      if (existing) { this.focus(existing.id); return existing.id; }
    }
    if (this.order.length >= MAX_TABS) {
      this.notifyError(new Error(`At most ${MAX_TABS} conversations can be open; close one first.`));
      return this.activeId ?? this.order[0].id;
    }
    const backend = this.create();
    const id = `tab${++this.nextId}`;
    const title = sessionId ? displayTitle(this.list.find(item => item.id === sessionId)?.title ?? "", sessionId) : "New conversation";
    const tab: Tab = { id, backend, title, sessionId, off: () => undefined };
    tab.off = backend.subscribe(message => this.fromBackend(tab, message));
    this.order.push(tab);
    this.activeId = id;
    if (this.endpoint) backend.setServerInfo(this.endpoint);
    if (sessionId) void backend.openConversation(sessionId).catch(this.notifyError);
    if (!restoring) {
      this.changed();
      this.bindActive();
    }
    return id;
  }

  close(id: string): void {
    const index = this.order.findIndex(tab => tab.id === id);
    if (index < 0) return;
    const [tab] = this.order.splice(index, 1);
    tab.off();
    tab.backend.dispose();
    // Only the conversation this editor is bound to is unbound: a background tab was never
    // bound, and an unfocus for it would cancel the active tab's bind still in progress.
    if (tab.sessionId && this.boundSessionId === tab.sessionId) {
      this.notifyBinding?.({ sessionId: tab.sessionId, focus: false });
      this.boundSessionId = undefined;
    }
    if (this.activeId === id) {
      this.activeId = (this.order[index] ?? this.order[index - 1])?.id;
      this.bindActive();
    }
    this.changed();
  }

  focus(id: string): void {
    if (this.activeId === id || !this.order.some(tab => tab.id === id)) return;
    this.activeId = id;
    this.changed();
    this.bindActive();
  }

  /** Forwards an endpoint announcement to every backend; tabs that lost their server reopen their session. */
  setServerInfo(info: ServerInfoParams): void {
    const restarted = !!this.endpoint && (this.endpoint.port !== info.port || this.endpoint.token !== info.token);
    this.endpoint = info;
    this.lister.setServerInfo(info);
    for (const tab of this.order) {
      tab.backend.setServerInfo(info);
      if (restarted && tab.sessionId) void tab.backend.openConversation(tab.sessionId).catch(this.notifyError);
    }
    this.bindActive();
  }

  private bindActive(): void {
    const active = this.order.find(tab => tab.id === this.activeId);
    const targetSessionId = active?.sessionId;
    if (this.boundSessionId && this.boundSessionId !== targetSessionId) {
      this.notifyBinding?.({ sessionId: this.boundSessionId, focus: false });
      this.boundSessionId = undefined;
    }
    if (!targetSessionId) return;
    const incarnation = active?.backend.currentIncarnation?.() ?? undefined;
    this.notifyBinding?.({
      sessionId: targetSessionId,
      ...(incarnation ? { incarnation } : {}),
      focus: true,
    });
    this.boundSessionId = targetSessionId;
  }

  settingsRequest(path: string, init?: RequestInit, signal?: AbortSignal): Promise<Response> {
    return (this.lister as unknown as { settingsRequest(path: string, init?: RequestInit, signal?: AbortSignal): Promise<Response> }).settingsRequest(path, init, signal);
  }

  async listConversations(): Promise<ConversationSummary[]> {
    const items = await this.lister.listConversations();
    this.acceptItems(items);
    return [...this.list];
  }

  async deleteConversation(id: string): Promise<void> {
    // Close the tabs only once the server has dropped the conversation: a refused delete must
    // leave it open where the Captain had it. The lister treats "already gone" as deleted.
    await this.lister.deleteConversation(id);
    for (const tab of [...this.order]) if (tab.sessionId === id) this.close(tab.id);
    this.list = this.list.filter(item => item.id !== id);
    this.publishList();
  }

  dispose(): void {
    this.disposed = true;
    if (this.refreshTimer) clearTimeout(this.refreshTimer);
    this.listerOff();
    if (this.boundSessionId) {
      this.notifyBinding?.({ sessionId: this.boundSessionId, focus: false });
      this.boundSessionId = undefined;
    }
    for (const tab of this.order.splice(0)) { tab.off(); tab.backend.dispose(); }
    this.lister.dispose();
    this.tabListeners.clear(); this.messageListeners.clear(); this.listListeners.clear();
  }

  private fromBackend(tab: Tab, message: HostToWebview): void {
    if (this.disposed) return;
    if (message.type === "conversations") { this.acceptList(message); return; }
    if (message.type === "conversationOpened") {
      const sessionId = message.id ?? tab.backend.currentConversationId();
      const title = message.id === undefined ? "New conversation" : displayTitle(message.title, sessionId ?? "");
      if (sessionId !== tab.sessionId || title !== tab.title) {
        tab.sessionId = sessionId;
        tab.title = title;
        this.changed();
      }
      // Also for a new, still empty conversation: the server must drop the binding of the
      // conversation this tab showed before.
      if (tab.id === this.activeId) {
        this.bindActive();
      }
    }
    // A finished turn changes the list (message counts, ordering): refresh it, coalesced.
    if (message.type === "turnEnd" || message.type === "status") this.scheduleListRefresh();
    for (const listener of [...this.messageListeners]) listener(tab.id, message);
  }

  private acceptList(message: HostToWebview): void {
    if (message.type === "conversations") this.acceptItems(message.items);
  }

  private acceptItems(items: ConversationSummary[]): void {
    this.list = [...items];
    for (const tab of this.order) {
      const row = tab.sessionId ? this.list.find(item => item.id === tab.sessionId) : undefined;
      const next = row ? displayTitle(row.title, row.id) : "";
      if (row && next !== tab.title && tab.title !== "New conversation") { tab.title = next; this.changed(); }
    }
    this.publishList();
  }

  private publishList(): void { for (const listener of [...this.listListeners]) listener([...this.list]); }

  private scheduleListRefresh(): void {
    if (this.refreshTimer || !this.endpoint) return;
    this.refreshTimer = setTimeout(() => {
      this.refreshTimer = undefined;
      if (!this.disposed) void this.lister.listConversations().catch(error => { if (!this.disposed) this.notifyError(error); });
    }, LIST_REFRESH_MS);
  }

  private changed(): void {
    this.store?.save({ sessions: this.order.map(tab => tab.sessionId ?? null), active: Math.max(0, this.order.findIndex(tab => tab.id === this.activeId)) });
    for (const listener of [...this.tabListeners]) listener();
  }
}

/** A fixed single tab around one backend: how the chat view runs when it is given a bare `ChatBackend`. */
export function singleTab(backend: ChatBackend): ChatTabs {
  const tabListeners = new Set<() => void>();
  let items: ConversationSummary[] = [];
  const listeners = new Set<(items: ConversationSummary[]) => void>();
  const tabId = "main";
  const subscribeBackend = (listener: (tab: string, message: HostToWebview) => void): (() => void) => backend.subscribe(message => {
    if (message.type === "conversations") { items = message.items; for (const l of [...listeners]) l(items); return; }
    listener(tabId, message);
  });
  return {
    tabs: () => [{ id: tabId, title: "Conversation", ...(backend.currentConversationId?.() ? { sessionId: backend.currentConversationId() } : {}) }],
    active: () => tabId,
    backend: tab => (tab === tabId ? backend : undefined),
    open: () => tabId,
    close: () => undefined,
    focus: () => undefined,
    onTabs: listener => { tabListeners.add(listener); return () => { tabListeners.delete(listener); }; },
    subscribe: subscribeBackend,
    listConversations: () => backend.listConversations(),
    deleteConversation: id => backend.deleteConversation(id),
    conversations: () => [...items],
    onConversations: listener => { listeners.add(listener); return () => { listeners.delete(listener); }; },
  };
}
