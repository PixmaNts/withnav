import { chatHtml } from "./chatHtml";
import * as vscode from "vscode";
import { CHAT_VIEW_ID, ChatBackend, HostToWebview, WebviewToHost } from "./chatProtocol";
import { buildEditorContext, presentMessage } from "./editorContext";
import { routeComposer } from "./commands";

const CONTEXT_DEBOUNCE_MS = 200;
/** `globalState` key of the favorite model names; the terminal client keeps its own list. */
export const FAVORITE_MODELS_KEY = "withnav.favoriteModels";
const MAX_FAVORITES = 500;
const MAX_FAVORITE_NAME = 300;

/** Favorite names saved in `state`; anything but a list of strings is ignored. */
export function readFavorites(state: vscode.Memento): string[] {
  const saved = state.get<unknown>(FAVORITE_MODELS_KEY);
  return Array.isArray(saved) ? saved.filter((name): name is string => typeof name === "string") : [];
}

interface FavoriteStore {
  get(): string[];
  toggle(name: string): void;
  onChange(listener: (items: string[]) => void): () => void;
}

function favoriteStore(state: vscode.Memento): FavoriteStore {
  const listeners = new Set<(items: string[]) => void>();
  return {
    get: () => readFavorites(state),
    toggle(name) {
      const current = readFavorites(state);
      if (!current.includes(name) && (current.length >= MAX_FAVORITES || !name || name.length > MAX_FAVORITE_NAME)) return;
      const next = current.includes(name) ? current.filter(saved => saved !== name) : [...current, name];
      const saved = state.update(FAVORITE_MODELS_KEY, next);
      for (const listener of listeners) listener(next);
      void Promise.resolve(saved).catch(showError);
    },
    onChange(listener) { listeners.add(listener); return () => { listeners.delete(listener); }; },
  };
}

export function registerChatView(
  context: vscode.ExtensionContext,
  backend: ChatBackend,
): vscode.Disposable {
  const panels = new Map<string, vscode.WebviewPanel>();
  const openings = new Map<string, Promise<void>>();
  const favorites = favoriteStore(context.globalState);
  const registration = vscode.window.registerWebviewViewProvider(CHAT_VIEW_ID, {
    resolveWebviewView(view) {
      let disposed = false;
      const webview = view.webview;
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

      // Captain messages lose their raw editor-context block and gain a chip label.
      // The webview (re)loads with amnesia: remember the last conversation
      // header pushes so `ready` can restore them before the transcript.
      let lastConversationOpened: HostToWebview | undefined;
      let lastConversations: HostToWebview | undefined;
      let lastModels: HostToWebview | undefined;
      const post = (message: HostToWebview): void => {
        if (message.generation !== undefined && message.generation !== backend.currentGeneration()) return;
        message = { ...message, generation: backend.currentGeneration() };
        if (message.type === "conversationOpened") {
          lastConversationOpened = message;
        } else if (message.type === "conversations") lastConversations = message;
        else if (message.type === "models") lastModels = message;
        void webview.postMessage(presentMessage(message));
      };
      const unsubscribe = backend.subscribe(post);
      const unsubscribeFavorites = favorites.onChange(items => post({ type: "favorites", items }));

      // Live preview of what the next message would carry.
      let contextTimer: ReturnType<typeof setTimeout> | undefined;
      const pushContext = (): void => post({ type: "editorContext", label: buildEditorContext().label });
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
        if (message?.type !== "ready" && message?.generation !== backend.currentGeneration()) return;
        switch (message?.type) {
          case "toggleBypass":
            void backend.toggleBypassPermissions().catch(showError);
            break;
          case "ready":
            if (lastConversations) post(lastConversations);
            post({ type: "conversationOpened", id: backend.currentConversationId(),
              title: lastConversationOpened?.type === "conversationOpened" ? lastConversationOpened.title : "Conversation" });
            backend.publishSubagents();
            backend.publishBypass?.();
            post({ type: "history", items: backend.history() });
            backend.publishCompaction();
            if (lastModels) post(lastModels);
            const snapshot = backend.currentContext();
            if (snapshot) post({ type: "usage", snapshot });
            post({ type: "favorites", items: favorites.get() });
            pushContext();
            backend.resumePermissionReads();
            void backend.refreshCommands().catch(showError);
            void backend.refreshAssignments().catch(showError);
            break;
          case "send":
            if (backend.isReadOnly()) break;
            if (typeof message.text === "string" && message.text.trim()) {
              const operation = backend.captureOperation();
              void (async () => {
                const session = backend.currentConversationId();
                 const generation = backend.currentGeneration();
                 // Route before confirmation and, crucially, before reading editor content.
                 const route = routeComposer(message.text);
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
                 if (routed.kind === "message") await backend.send(routed.text, context, operation);
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
                ["allow_once", "allow_folder", "deny"].includes(message.decision) &&
                (message.message === undefined || typeof message.message === "string")) {
              void backend.answerPermission(message.sessionId, message.requestId, message.decision, message.message, message.generation).catch(showError);
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
          case "listConversations":
            void backend.listConversations().catch(showError);
            break;
          case "openConversation":
            if (typeof message.id === "string") {
              void backend.openConversation(message.id).catch(showError);
            }
            break;
          case "newConversation":
            void backend.newConversation().catch(showError);
            break;
          case "deleteConversation":
            if (typeof message.id === "string") {
              void backend.deleteConversation(message.id).catch(showError);
            }
            break;
          case "selectModel":
            if (typeof message.name === "string") backend.selectModel(message.name);
            break;
          case "toggleFavorite":
            if (typeof message.name === "string") favorites.toggle(message.name);
            break;
          case "selectAgent":
            if (typeof message.id === "string") void backend.selectAgent(message.id).catch(showError);
            break;
          case "openChild":
            if (typeof message.parentId === "string" && typeof message.childId === "string") {
              const existing = panels.get(message.childId);
              if (existing) { existing.reveal(); break; }
              const opening = openings.get(message.childId);
              if (opening) { void opening.then(() => panels.get(message.childId)?.reveal()); break; }
              const pending = backend.openChild(message.parentId, message.childId, message.generation).then(childBackend => {
                if (disposed || message.generation !== backend.currentGeneration()) { childBackend.dispose(); return; }
                const panel = vscode.window.createWebviewPanel("withnav.subagent", "Subagent", vscode.ViewColumn.Active, { enableScripts: true });
                panels.set(message.childId, panel);
                mountChildPanel(context, panel, childBackend, backend, message.parentId, message.childId, favorites);
                panel.onDidDispose(() => { if (panels.get(message.childId) === panel) panels.delete(message.childId); childBackend.dispose(); });
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
      });
      view.onDidDispose(() => {
        disposed = true;
        backend.cancelPermissionReads();
        unsubscribe();
        unsubscribeFavorites();
        messageSubscription.dispose();
        if (contextTimer) clearTimeout(contextTimer);
        for (const subscription of editorSubscriptions) subscription.dispose();
      });
    },
  });
  return registration;
}

function mountChildPanel(context: vscode.ExtensionContext, panel: vscode.WebviewPanel, child: ChatBackend, parent: ChatBackend, parentId: string, childId: string, favorites: FavoriteStore): void {
  let disposed = false;
  const webview = panel.webview;
  webview.options = { enableScripts: true, localResourceRoots: [vscode.Uri.joinPath(context.extensionUri, "out")] };
  webview.html = chatHtml(randomNonce(), String(webview.asWebviewUri(vscode.Uri.joinPath(context.extensionUri, "out", "chat.js"))), String(webview.asWebviewUri(vscode.Uri.joinPath(context.extensionUri, "out", "chat.css"))), webview.cspSource);
  const post = (m: HostToWebview): void => { if (!disposed) void webview.postMessage(m); };
  const off = child.subscribe(post);
  const favoritesOff = favorites.onChange(items => post({ type: "favorites", items, generation: child.currentGeneration() }));
  const parentOff = parent.subscribe(m => { if (m.type === "subagents") {
    const row = m.items.find(c => c.id === childId);
    if (row) { panel.title = `Subagent · ${row.agent} · ${row.goal.slice(0,80)}`; child.publishSubagents(); }
  } });
  webview.onDidReceiveMessage(async (m: WebviewToHost) => {
    if (disposed || (m.type !== "ready" && m.generation !== child.currentGeneration())) return;
    if (m.type === "ready") {
      post({ type: "conversationOpened", id: childId, title: "Read-only subagent", generation: child.currentGeneration() });
      post({ type: "favorites", items: favorites.get(), generation: child.currentGeneration() });
      child.publishSubagents();
      post({ type: "history", items: child.history(), generation: child.currentGeneration() });
      child.resumePermissionReads();
    } else if (m.type === "controlChild" && m.parentId === parentId && m.childId === childId && ["stop", "continue"].includes(m.action)) {
      try {
        const descriptor = parent.captureChildControl(parentId, childId, m.action, m.generation);
        const choice = await vscode.window.showWarningMessage(`${m.action === "stop" ? "Stop" : "Continue"} this subagent?`, { modal: true }, "Confirm");
        if (!disposed && choice === "Confirm") await parent.controlChild(parentId, childId, m.action, m.generation, descriptor);
      } catch (error) { if (!disposed) showError(error); }
    }
    // No send, resume, commands or permission answers in this panel.
  });
  panel.onDidDispose(() => { disposed = true; off(); favoritesOff(); parentOff(); child.dispose(); });
}

function randomNonce(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return Buffer.from(bytes).toString("base64");
}

function showError(error: unknown): void {
  void vscode.window.showErrorMessage(`WithNav: ${error instanceof Error ? error.message : String(error)}`);
}

async function openFile(path: string, line?: number): Promise<void> {
  const folders = vscode.workspace.workspaceFolders;
  if (!path.trim() || !folders?.length) return;
  const absolute = /^(\/|[A-Za-z]:[\\/])/.test(path);
  const uri = absolute ? vscode.Uri.file(path) : vscode.Uri.joinPath(folders[0].uri, path);
  if (!vscode.workspace.getWorkspaceFolder(uri)) return;
  const document = await vscode.workspace.openTextDocument(uri);
  const editor = await vscode.window.showTextDocument(document);
  if (typeof line === "number" && Number.isInteger(line) && line > 0) {
    const position = new vscode.Position(Math.min(line - 1, document.lineCount - 1), 0);
    editor.selection = new vscode.Selection(position, position);
    editor.revealRange(new vscode.Range(position, position), vscode.TextEditorRevealType.InCenterIfOutsideViewport);
  }
}
