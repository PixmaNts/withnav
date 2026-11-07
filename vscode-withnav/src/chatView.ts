import { chatHtml } from "./chatHtml";
import * as vscode from "vscode";
import { CHAT_VIEW_ID, ChatBackend, ChatTabs, HostToWebview, PendingImage, WebviewToHost } from "./chatProtocol";
import { singleTab } from "./chatHub";
import { openMcpAppPanel } from "./mcpAppPanel";
import { readSetting } from "./settings";

export const FONT_SIZE = { min: 11, max: 22, fallback: 14 } as const;
/** The chat text size from `withnav.chat.fontSize`, clamped; the default when unreadable. */
export function chatFontSize(): number {
  try {
    const value = Number(readSetting<number>("chat.fontSize", FONT_SIZE.fallback, false));
    return Number.isFinite(value) ? Math.min(FONT_SIZE.max, Math.max(FONT_SIZE.min, Math.round(value))) : FONT_SIZE.fallback;
  } catch { return FONT_SIZE.fallback; }
}
/** Calls `listener` when the setting changes; a host without configuration events gets a no-op. */
function onFontSizeChange(listener: () => void): { dispose(): void } {
  try { return vscode.workspace.onDidChangeConfiguration(event => { if (event.affectsConfiguration("withnav.chat.fontSize")) listener(); }); }
  catch { return { dispose() {} }; }
}

const workspaceName = (): { workspace?: string } => (vscode.workspace.name ? { workspace: vscode.workspace.name } : {});
import { buildEditorContext, presentMessage } from "./editorContext";
import { routeComposer } from "./commands";
import { openFile } from "./reveal";
import { showImage } from "./imagePanel";


const CONTEXT_DEBOUNCE_MS = 200;
/** `globalState` key of the favorite model names; the terminal client keeps its own list. */
export const FAVORITE_MODELS_KEY = "withnav.favoriteModels";
/** `globalState` key of the favorite agent names. */
export const FAVORITE_AGENTS_KEY = "withnav.favoriteAgents";
const MAX_FAVORITES = 500;
const MAX_FAVORITE_NAME = 300;

/** Favorite names saved in `state`; anything but a list of strings is ignored. */
export function readFavorites(state: vscode.Memento, key: string = FAVORITE_MODELS_KEY): string[] {
  const saved = state.get<unknown>(key);
  return Array.isArray(saved) ? saved.filter((name): name is string => typeof name === "string") : [];
}

interface FavoriteStore {
  get(): string[];
  toggle(name: string): void;
  onChange(listener: (items: string[]) => void): () => void;
}

function favoriteStore(state: vscode.Memento, key: string = FAVORITE_MODELS_KEY): FavoriteStore {
  const listeners = new Set<(items: string[]) => void>();
  return {
    get: () => readFavorites(state, key),
    toggle(name) {
      const current = readFavorites(state, key);
      if (!current.includes(name) && (current.length >= MAX_FAVORITES || !name || name.length > MAX_FAVORITE_NAME)) return;
      const next = current.includes(name) ? current.filter(saved => saved !== name) : [...current, name];
      const saved = state.update(key, next);
      for (const listener of listeners) listener(next);
      void Promise.resolve(saved).catch(showError);
    },
    onChange(listener) { listeners.add(listener); return () => { listeners.delete(listener); }; },
  };
}

function isTabs(source: ChatTabs | ChatBackend): source is ChatTabs {
  return typeof (source as ChatTabs).tabs === "function" && typeof (source as ChatTabs).backend === "function";
}

/** Messages that name no conversation: they carry neither a tab nor a generation. */
const GLOBAL_MESSAGES = new Set(["ready", "openTab", "closeTab", "focusTab", "listConversations", "deleteConversation", "toggleFavorite", "toggleFavoriteAgent"]);

export function registerChatView(
  context: vscode.ExtensionContext,
  source: ChatTabs | ChatBackend,
): vscode.Disposable {
  const tabs: ChatTabs = isTabs(source) ? source : singleTab(source);
  const panels = new Map<string, { panel: vscode.WebviewPanel; tab: string }>();
  const openings = new Map<string, Promise<void>>();
  const favorites = favoriteStore(context.globalState);
  const favoriteAgents = favoriteStore(context.globalState, FAVORITE_AGENTS_KEY);
  let currentWebview: vscode.Webview | undefined;
  const showSessions = vscode.commands.registerCommand("withnav.showSessions", () => { void currentWebview?.postMessage({ type: "showSessions" }); });
  const newConversation = vscode.commands.registerCommand("withnav.newConversation", () => { tabs.open(); });
  const deletion = vscode.commands.registerCommand("withnav.deleteConversation", async () => {
    try {
      const items = await tabs.listConversations();
      const picked = await vscode.window.showQuickPick(items.map(conversation => ({
        label: conversation.title, description: conversation.updatedAt, conversation,
      })), { placeHolder: "Choose a conversation to delete" });
      if (picked) await confirmDeleteConversation(tabs, picked.conversation.id);
    } catch (error) { showError(error); }
  });
  const registration = vscode.window.registerWebviewViewProvider(CHAT_VIEW_ID, {
    resolveWebviewView(view) {
      let disposed = false;
      let ready = false;
      const webview = view.webview;
      currentWebview = webview;
      webview.options = {
        enableScripts: true,
        localResourceRoots: [
          vscode.Uri.joinPath(context.extensionUri, "out"),
          vscode.Uri.joinPath(context.extensionUri, "media"),
        ],
      };

      const nonce = randomNonce();
      const script = webview.asWebviewUri(vscode.Uri.joinPath(context.extensionUri, "out", "chat.js"));
      const style = webview.asWebviewUri(vscode.Uri.joinPath(context.extensionUri, "out", "chat.css"));
      webview.html = chatHtml(nonce, String(script), String(style), webview.cspSource);

      // The webview (re)loads with amnesia: remember each tab's last header pushes so
      // `ready` can restore them before the transcript.
      const lastOpened = new Map<string, HostToWebview>();
      const lastModels = new Map<string, HostToWebview>();
      const known = new Set<string>();
      const postGlobal = (message: HostToWebview): void => { void webview.postMessage(message); };
      // Tab messages carry their tab and the generation of that tab's backend; Captain messages
      // lose their raw editor-context block and gain a chip label.
      const postTab = (tab: string, message: HostToWebview): void => {
        const backend = tabs.backend(tab);
        if (!backend) return;
        if (message.generation !== undefined && message.generation !== backend.currentGeneration()) return;
        message = { ...message, tab, generation: backend.currentGeneration() };
        if (message.type === "conversationOpened") lastOpened.set(tab, message);
        else if (message.type === "models") lastModels.set(tab, message);
        void webview.postMessage(presentMessage(message));
      };
      const initTab = (tab: string): void => {
        const backend = tabs.backend(tab);
        if (!backend) return;
        const opened = lastOpened.get(tab);
        postTab(tab, { type: "conversationOpened", id: backend.currentConversationId(),
          title: opened?.type === "conversationOpened" ? opened.title : "Conversation" });
        backend.publishSubagents();
        backend.publishBypass?.();
        postTab(tab, { type: "history", items: backend.history() });
        backend.publishCompaction();
        const models = lastModels.get(tab);
        if (models) postTab(tab, models);
        const snapshot = backend.currentContext();
        if (snapshot) postTab(tab, { type: "usage", snapshot });
        backend.resumePermissionReads();
        void backend.refreshCommands().catch(showError);
        void backend.refreshAssignments().catch(showError);
      };
      const syncTabs = (): void => {
        if (!ready) return;
        const current = tabs.tabs();
        postGlobal({ type: "tabs", tabs: current, active: tabs.active() });
        const ids = new Set(current.map(tab => tab.id));
        for (const id of [...known]) if (!ids.has(id)) known.delete(id);
        for (const id of ids) if (!known.has(id)) { known.add(id); initTab(id); }
        for (const [childId, entry] of panels) if (!ids.has(entry.tab)) { panels.delete(childId); entry.panel.dispose(); }
      };
      const unsubscribe = tabs.subscribe(postTab);
      const unsubscribeTabs = tabs.onTabs(syncTabs);
      const unsubscribeList = tabs.onConversations(items => postGlobal({ type: "conversations", items, ...workspaceName() }));
      const unsubscribeFavorites = favorites.onChange(items => postGlobal({ type: "favorites", items }));
      const unsubscribeFavoriteAgents = favoriteAgents.onChange(items => postGlobal({ type: "favoriteAgents", items }));
      const fontWatcher = onFontSizeChange(() => postGlobal({ type: "appearance", fontSize: chatFontSize() }));

      // Live preview of what the next message would carry.
      let contextTimer: ReturnType<typeof setTimeout> | undefined;
      const pushContext = (): void => postGlobal({ type: "editorContext", label: buildEditorContext().label });
      const scheduleContext = (): void => {
        if (contextTimer) clearTimeout(contextTimer);
        contextTimer = setTimeout(pushContext, CONTEXT_DEBOUNCE_MS);
      };
      const editorSubscriptions = [
        vscode.window.onDidChangeActiveTextEditor(scheduleContext),
        vscode.window.onDidChangeTextEditorSelection(scheduleContext),
        vscode.window.onDidChangeVisibleTextEditors(scheduleContext),
        vscode.window.tabGroups.onDidChangeTabs(scheduleContext),
      ];
      const messageSubscription = webview.onDidReceiveMessage((message: WebviewToHost) => {
        try {
        // Messages that name no conversation.
        if (message?.type && GLOBAL_MESSAGES.has(message.type)) {
          switch (message.type) {
            case "ready":
              ready = true;
              known.clear();
              postGlobal({ type: "conversations", items: tabs.conversations(), ...workspaceName() });
              postGlobal({ type: "appearance", fontSize: chatFontSize() });
              postGlobal({ type: "favorites", items: favorites.get() });
              postGlobal({ type: "favoriteAgents", items: favoriteAgents.get() });
              syncTabs();
              pushContext();
              void Promise.resolve().then(() => tabs.listConversations()).catch(() => undefined);
              break;
            case "openTab":
              if (message.sessionId === undefined || typeof message.sessionId === "string") tabs.open(message.sessionId);
              break;
            case "closeTab":
              if (typeof message.tab === "string") tabs.close(message.tab);
              break;
            case "focusTab":
              if (typeof message.tab === "string") tabs.focus(message.tab);
              break;
            case "listConversations":
              void tabs.listConversations().catch(showError);
              break;
            case "deleteConversation":
              if (typeof message.id === "string") void confirmDeleteConversation(tabs, message.id).catch(showError);
              break;
            case "toggleFavoriteAgent":
              if (typeof message.name === "string") favoriteAgents.toggle(message.name);
              break;
            case "toggleFavorite":
              if (typeof message.name === "string") favorites.toggle(message.name);
              break;
          }
          return;
        }
        // Everything else belongs to one tab and its current generation.
        const tab = typeof message?.tab === "string" ? message.tab : undefined;
        const backend = tab === undefined ? undefined : tabs.backend(tab);
        if (!tab || !backend || message.generation !== backend.currentGeneration()) return;
        const post = (reply: HostToWebview): void => postTab(tab, reply);
        switch (message.type) {
          case "loadImage":
            if (typeof message.attachmentId === "string") {
              const attachmentId = message.attachmentId;
              void backend.imageDataUrl(attachmentId).catch(() => undefined).then(url => post({ type: "imageData", attachmentId, url }));
            }
            break;
          case "openImage":
            if (typeof message.attachmentId === "string") {
              const { attachmentId, name } = message;
              void backend.imageDataUrl(attachmentId).then(url => url ? showImage(String(name || "image"), url) : void vscode.window.showWarningMessage("This image is no longer available."), showError);
            }
            break;
          case "toggleBypass":
            void backend.toggleBypassPermissions().catch(showError);
            break;
          case "send":
            if (backend.isReadOnly()) break;
            if (typeof message.text === "string" && (message.text.trim() || message.images?.length)) {
              const operation = backend.captureOperation();
              void (async () => {
                const images = pendingImages(message.images);
                const session = backend.currentConversationId();
                 const generation = backend.currentGeneration();
                 // Route before confirmation and, crucially, before reading editor content.
                 const route = routeComposer(message.text);
                 if (images.length && route.kind === "command") throw new Error("Images go with a message, not with a command.");
                 if (route.kind === "command" && route.name !== "skill") {
                   return await backend.runComposerCommand(message.text, generation, operation);
                 }
                if (backend.hasOpenPermissions()) {
                  const answer = await vscode.window.showWarningMessage(
                    "Sending cancels the pending permission request", { modal: true }, "Send anyway",
                  );
                  if (answer !== "Send anyway") return false;
                }
                 if (generation !== backend.currentGeneration() || session !== backend.currentConversationId()) throw new Error("The open conversation changed. Send again in the current conversation.");
                 if (route.kind === "command") return await backend.runComposerCommand(message.text, generation, operation);
                const context = message.includeContext === false ? undefined : buildEditorContext().block;
                 const routed = routeComposer(message.text);
                 if (routed.kind === "message") await backend.send(routed.text, context, operation, images);
                return true;
               })().then(accepted => {
                 // /new deliberately changes identity. Its reset already released the composer;
                 // never relabel an acknowledgement from the previous conversation.
                 if (routeComposer(message.text).kind === "command" && message.text.match(/^\/new(?:\s|$)/)) return;
                 if (backend.ownsOperation(operation)) post({ type: "sendResult", text: message.text, accepted, generation: operation.generation });
               })
                .catch(error => { if (!backend.ownsOperation(operation)) return; post({ type: "sendResult", text: message.text, accepted: false, generation: operation.generation }); post({ type: "commandError", text: error instanceof Error ? error.message : String(error), generation: operation.generation }); });
            }
            break;
          case "answerPermission":
            if (typeof message.sessionId === "string" && typeof message.requestId === "string" &&
                ["allow_once", "allow_always", "reject"].includes(message.decision) &&
                (message.message === undefined || typeof message.message === "string")) {
              void backend.answerPermission(message.sessionId, message.requestId, message.decision, message.message, message.generation).catch(showError);
            }
            break;
          case "answerQuestion":
            if (typeof message.sessionId === "string" && message.answer && typeof message.answer.question_call_id === "string") {
              void backend.answerQuestion(message.sessionId, message.answer, message.generation).catch(showError);
            }
            break;
          // The value travels only from here to the backend's dedicated request: never logged,
          // never put in a notice or an error (an error names the secret, not its value).
          case "submitSecret":
            if (typeof message.sessionId === "string" && typeof message.requestId === "string" && typeof message.name === "string" &&
                typeof message.value === "string" && ["1h", "24h", "session"].includes(message.ttl) && ["session", "workspace"].includes(message.scope)) {
              void backend.submitSecret(message.sessionId, message.requestId, message.name, message.value, message.ttl, message.scope, message.generation)
                .catch(() => showError(new Error(`Could not save the secret ${message.name}.`)));
            }
            break;
          case "declineSecret":
            if (typeof message.sessionId === "string" && typeof message.requestId === "string") {
              void backend.declineSecret(message.sessionId, message.requestId, message.generation).catch(showError);
            }
            break;
          case "listSecrets":
            if (typeof message.sessionId === "string") void backend.listSecrets(message.sessionId, message.generation).catch(showError);
            break;
          case "revokeSecret":
            if (typeof message.sessionId === "string" && typeof message.name === "string") {
              void backend.revokeSecret(message.sessionId, message.name, message.requestId, message.generation).catch(showError);
            }
            break;
          case "controlPause":
            if (typeof message.sessionId === "string" && typeof message.id === "string" &&
                ["continue", "stop"].includes(message.action)) {
              void backend.controlPause(message.sessionId, message.id, message.action, message.generation).catch(showError);
            }
            break;
          case "stop":
            void backend.stop().catch(showError);
            break;
          case "openFile":
            if (typeof message.path === "string") {
              void openFile(message.path, message.line).catch(showError);
            }
            break;
          case "openMcpApp":
            if (message.app && typeof message.app.server === "string") {
              void openMcpAppPanel(backend, message.app).catch(showError);
            }
            break;
          case "selectModel":
            if (typeof message.name === "string") backend.selectModel(message.name);
            break;
          case "selectAgent":
            if (typeof message.id === "string") void backend.selectAgent(message.id).catch(showError);
            break;
          case "openChild":
            if (typeof message.parentId === "string" && typeof message.childId === "string") {
              const existing = panels.get(message.childId);
              if (existing) { existing.panel.reveal(); break; }
              const opening = openings.get(message.childId);
              if (opening) { void opening.then(() => panels.get(message.childId)?.panel.reveal()); break; }
              const pending = backend.openChild(message.parentId, message.childId, message.generation).then(childBackend => {
                if (disposed || message.generation !== backend.currentGeneration()) { childBackend.dispose(); return; }
                const panel = vscode.window.createWebviewPanel("withnav.subagent", "Subagent", vscode.ViewColumn.Active, { enableScripts: true });
                panels.set(message.childId, { panel, tab });
                mountChildPanel(context, panel, childBackend, backend, message.parentId, message.childId, favorites);
                panel.onDidDispose(() => { if (panels.get(message.childId)?.panel === panel) panels.delete(message.childId); childBackend.dispose(); });
                const off = backend.subscribe(update => { if (update.generation !== message.generation) panel.dispose(); });
                panel.onDidDispose(off);
              }).catch(showError).finally(() => { if (openings.get(message.childId) === pending) openings.delete(message.childId); });
              openings.set(message.childId, pending);
            }
            break;
          case "controlChild":
            if (typeof message.parentId === "string" && typeof message.childId === "string" && ["stop", "continue"].includes(message.action)) {
              const generation = message.generation;
              let descriptor: object;
              try { descriptor = backend.captureChildControl(message.parentId, message.childId, message.action, generation); } catch (error) { showError(error); break; }
              void (async () => {
                const choice = await vscode.window.showWarningMessage(`${message.action === "stop" ? "Stop" : "Continue"} this subagent?`, { modal: true }, "Confirm");
                if (disposed || choice !== "Confirm" || generation !== backend.currentGeneration()) return;
                await backend.controlChild(message.parentId, message.childId, message.action, generation, descriptor);
              })().catch(showError);
            }
            break;
        }
        } catch (error) { showError(error); }
      });
      view.onDidDispose(() => {
        disposed = true;
        if (currentWebview === webview) currentWebview = undefined;
        for (const tab of tabs.tabs()) tabs.backend(tab.id)?.cancelPermissionReads();
        unsubscribe();
        unsubscribeTabs();
        unsubscribeList();
        unsubscribeFavorites();
        unsubscribeFavoriteAgents();
        fontWatcher.dispose();
        messageSubscription.dispose();
        if (contextTimer) clearTimeout(contextTimer);
        for (const subscription of editorSubscriptions) subscription.dispose();
      });
    },
  }, { webviewOptions: { retainContextWhenHidden: true } });
  return vscode.Disposable.from(registration, showSessions, newConversation, deletion);
}

export async function confirmDeleteConversation(backend: Pick<ChatBackend, "listConversations" | "deleteConversation">, id: string): Promise<void> {
  const conversation = (await backend.listConversations()).find(item => item.id === id);
  const count = conversation?.subagentCount ?? 0;
  const answer = await vscode.window.showWarningMessage(
    count ? `Delete this conversation and its ${count} subagent sessions? This cannot be undone.`
      : "Delete this conversation? This cannot be undone.",
    { modal: true }, "Delete",
  );
  if (answer === "Delete") await backend.deleteConversation(id);
}

function mountChildPanel(context: vscode.ExtensionContext, panel: vscode.WebviewPanel, child: ChatBackend, parent: ChatBackend, parentId: string, childId: string, favorites: FavoriteStore): void {
  let disposed = false;
  const webview = panel.webview;
  webview.options = { enableScripts: true, localResourceRoots: [vscode.Uri.joinPath(context.extensionUri, "out")] };
  webview.html = chatHtml(randomNonce(), String(webview.asWebviewUri(vscode.Uri.joinPath(context.extensionUri, "out", "chat.js"))), String(webview.asWebviewUri(vscode.Uri.joinPath(context.extensionUri, "out", "chat.css"))), webview.cspSource).replace("<body>", '<body class="editor-tab">');
  const post = (m: HostToWebview): void => { if (!disposed) void webview.postMessage(m); };
  const off = child.subscribe(post);
  const favoritesOff = favorites.onChange(items => post({ type: "favorites", items, generation: child.currentGeneration() }));
  const fontWatcher = onFontSizeChange(() => post({ type: "appearance", fontSize: chatFontSize() }));
  const parentOff = parent.subscribe(m => { if (m.type === "subagents") {
    const row = m.items.find(c => c.id === childId);
    if (m.parentId === parentId && !row) { panel.dispose(); return; }
    if (row) { panel.title = `Subagent · ${row.agent} · ${row.goal.slice(0,80)}`; child.publishSubagents(); }
  } });
  webview.onDidReceiveMessage(async (m: WebviewToHost) => {
    if (disposed || (m.type !== "ready" && m.generation !== child.currentGeneration())) return;
    if (m.type === "ready") {
      post({ type: "conversationOpened", id: childId, title: "Read-only subagent", generation: child.currentGeneration() });
      post({ type: "appearance", fontSize: chatFontSize() });
      post({ type: "favorites", items: favorites.get(), generation: child.currentGeneration() });
      child.publishSubagents();
      post({ type: "history", items: child.history(), generation: child.currentGeneration() });
      child.resumePermissionReads();
    } else if (m.type === "openMcpApp" && m.app && typeof m.app.server === "string") {
      void openMcpAppPanel(child, m.app).catch(showError);
    } else if (m.type === "controlChild" && m.parentId === parentId && m.childId === childId && ["stop", "continue"].includes(m.action)) {
      try {
        const descriptor = parent.captureChildControl(parentId, childId, m.action, m.generation);
        const choice = await vscode.window.showWarningMessage(`${m.action === "stop" ? "Stop" : "Continue"} this subagent?`, { modal: true }, "Confirm");
        if (!disposed && choice === "Confirm") await parent.controlChild(parentId, childId, m.action, m.generation, descriptor);
      } catch (error) { if (!disposed) showError(error); }
    } else if (m.type === "loadImage" && typeof m.attachmentId === "string") {
      const attachmentId = m.attachmentId;
      const url = await child.imageDataUrl(attachmentId).catch(() => undefined);
      post({ type: "imageData", attachmentId, url });
    } else if (m.type === "openImage" && typeof m.attachmentId === "string") {
      const url = await child.imageDataUrl(m.attachmentId).catch(() => undefined);
      if (disposed) return;
      if (url) showImage(String(m.name || "image"), url);
      else void vscode.window.showWarningMessage("This image is no longer available.");
    }
    // No send, resume, commands or permission answers in this panel.
  });
  panel.onDidDispose(() => { disposed = true; off(); favoritesOff(); fontWatcher.dispose(); parentOff(); child.dispose(); });
}

function randomNonce(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return Buffer.from(bytes).toString("base64");
}

function showError(error: unknown): void {
  void vscode.window.showErrorMessage(`WithNav: ${error instanceof Error ? error.message : String(error)}`);
}

/** Most images a message carries, and the largest one in base64 (the server's 3.75 MiB raw cap). */
const MAX_PENDING_IMAGES = 8;
const MAX_PENDING_BASE64 = Math.ceil((3_932_160 * 4) / 3) + 4;

/**
 * The images of a `send` from the webview, checked before anything is decoded or uploaded: the
 * webview is not a trust boundary. Anything malformed refuses the whole message.
 */
export function pendingImages(value: unknown): PendingImage[] {
  if (value === undefined) return [];
  if (!Array.isArray(value) || value.length > MAX_PENDING_IMAGES) throw new Error(`A message carries at most ${MAX_PENDING_IMAGES} images.`);
  return value.map((item: unknown) => {
    const { name, data } = (item ?? {}) as { name?: unknown; data?: unknown };
    if (typeof name !== "string" || name.length > 255 || typeof data !== "string" || !data) throw new Error("An attached image could not be read.");
    if (data.length > MAX_PENDING_BASE64) throw new Error(`${name} is too large to attach.`);
    if (!/^[A-Za-z0-9+/]*={0,2}$/.test(data)) throw new Error(`${name} could not be read.`);
    return { name, data };
  });
}
