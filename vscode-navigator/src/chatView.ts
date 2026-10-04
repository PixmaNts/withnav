import { chatHtml } from "./chatHtml";
import * as vscode from "vscode";
import { CHAT_VIEW_ID, ChatBackend, HostToWebview, WebviewToHost } from "./chatProtocol";
import { buildEditorContext, presentMessage } from "./editorContext";

const CONTEXT_DEBOUNCE_MS = 200;

export function registerChatView(
  context: vscode.ExtensionContext,
  backend: ChatBackend,
): vscode.Disposable {
  return vscode.window.registerWebviewViewProvider(CHAT_VIEW_ID, {
    resolveWebviewView(view) {
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

      // Driver messages lose their raw editor-context block and gain a chip label.
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
          case "ready":
            if (lastConversations) post(lastConversations);
            post({ type: "conversationOpened", id: backend.currentConversationId(),
              title: lastConversationOpened?.type === "conversationOpened" ? lastConversationOpened.title : "Conversation" });
            post({ type: "history", items: backend.history() });
            if (lastModels) post(lastModels);
            const snapshot = backend.currentContext();
            if (snapshot) post({ type: "usage", snapshot });
            pushContext();
            backend.resumePermissionReads();
            break;
          case "send":
            if (typeof message.text === "string" && message.text.trim()) {
              void (async () => {
                const session = backend.currentConversationId();
                const generation = backend.currentGeneration();
                if (backend.hasOpenPermissions()) {
                  const answer = await vscode.window.showWarningMessage(
                    "Sending cancels the pending permission request", { modal: true }, "Send anyway",
                  );
                  if (answer !== "Send anyway") return false;
                }
                if (generation !== backend.currentGeneration() || session !== backend.currentConversationId()) throw new Error("The open conversation changed. Send again in the current conversation.");
                const context = message.includeContext === false ? undefined : buildEditorContext().block;
                await backend.send(message.text, context);
                return true;
              })().then(accepted => post({ type: "sendResult", text: message.text, accepted, generation: accepted ? backend.currentGeneration() : message.generation }))
                .catch(error => { post({ type: "sendResult", text: message.text, accepted: false, generation: message.generation }); showError(error); });
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
            if (typeof message.alias === "string") backend.selectModel(message.alias);
            break;
        }
      });
      view.onDidDispose(() => {
        backend.cancelPermissionReads();
        unsubscribe();
        messageSubscription.dispose();
        if (contextTimer) clearTimeout(contextTimer);
        for (const subscription of editorSubscriptions) subscription.dispose();
      });
    },
  });
}

function randomNonce(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return Buffer.from(bytes).toString("base64");
}

function showError(error: unknown): void {
  void vscode.window.showErrorMessage(`Navai: ${error instanceof Error ? error.message : String(error)}`);
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
