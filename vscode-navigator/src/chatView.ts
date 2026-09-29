import * as vscode from "vscode";
import { CHAT_VIEW_ID, ChatBackend, WebviewToHost } from "./chatProtocol";

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
      webview.html = `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src ${webview.cspSource}; style-src ${webview.cspSource}; script-src 'nonce-${nonce}'; connect-src 'none'; font-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'">
  <link rel="stylesheet" href="${style}">
  <title>Navigator</title>
</head>
<body>
  <div class="shell">
    <header class="topbar"><span class="identity">Navigator</span><span id="status" class="status" role="status" aria-live="polite"><span class="status-dot"></span><span id="status-label">Connecting</span></span></header>
    <main id="transcript" class="transcript" role="log" aria-label="Conversation" aria-live="polite" aria-relevant="additions text"><div id="empty" class="empty">Ask about the code, or describe what you want to do.</div></main>
    <form id="composer" class="composer">
      <label for="prompt" class="sr-only">Message Navigator</label>
      <textarea id="prompt" rows="2" placeholder="Message Navigator…" disabled></textarea>
      <div class="composer-footer"><span class="hint">Enter to send · Shift+Enter for a line</span><div class="actions"><button type="button" id="stop" class="stop" hidden>Stop</button><button type="submit" id="send" disabled>Send <span aria-hidden="true">↗</span></button></div></div>
    </form>
  </div>
  <script nonce="${nonce}" src="${script}"></script>
</body>
</html>`;

      const unsubscribe = backend.subscribe(message => { void webview.postMessage(message); });
      const messageSubscription = webview.onDidReceiveMessage((message: WebviewToHost) => {
        switch (message?.type) {
          case "ready":
            void webview.postMessage({ type: "history", items: backend.history() });
            break;
          case "send":
            if (typeof message.text === "string" && message.text.trim()) {
              void backend.send(message.text).catch(showError);
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
        }
      });
      view.onDidDispose(() => {
        unsubscribe();
        messageSubscription.dispose();
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
  void vscode.window.showErrorMessage(`Navigator: ${error instanceof Error ? error.message : String(error)}`);
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
