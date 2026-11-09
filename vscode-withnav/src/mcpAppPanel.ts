import * as vscode from "vscode";
import type { ChatBackend, McpAppDescriptor } from "./chatProtocol";

// One entry per open panel. The panel has a single message receiver, registered
// when the panel is created; it reads `backend` and `app` from this entry, so a
// reopen or reload only updates them. Registering a receiver per open made one
// webview RPC run the MCP tool once per earlier open or reload.
interface PanelEntry {
  panel: vscode.WebviewPanel;
  backend: ChatBackend;
  app: McpAppDescriptor;
  // Incremented by every open; a render whose resource read was overtaken by a
  // newer open drops its HTML instead of replacing the newer one.
  render: number;
}

const activePanels = new Map<string, PanelEntry>();

function escapeHtml(text: string): string {
  return text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function randomNonce(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return Buffer.from(bytes).toString("base64");
}

export async function openMcpAppPanel(backend: ChatBackend, app: McpAppDescriptor): Promise<void> {
  const key = `${app.server}:${app.resourceUri ?? app.url ?? app.tool}`;
  const title = `MCP App · ${app.server} (${app.tool})`;
  let entry = activePanels.get(key);
  if (entry) {
    entry.panel.title = title;
    entry.panel.reveal(vscode.ViewColumn.Beside);
  } else {
    const panel = vscode.window.createWebviewPanel(
      "withnav.mcpApp",
      title,
      vscode.ViewColumn.Beside ?? vscode.ViewColumn.Active,
      { enableScripts: true, retainContextWhenHidden: true },
    );
    const created: PanelEntry = { panel, backend, app, render: 0 };
    entry = created;
    activePanels.set(key, created);
    const sub = panel.webview.onDidReceiveMessage((msg: unknown) => handlePanelMessage(created, msg));
    panel.onDidDispose(() => {
      sub.dispose();
      if (activePanels.get(key) === created) activePanels.delete(key);
    });
  }
  const render = ++entry.render;

  // The app runs in its own document, served by WithNav with a Content-Security-Policy of its
  // own, never in a srcdoc that would inherit this panel's.
  let frameUrl: string | undefined;
  let loadError: string | undefined;
  const source = app.html ? { html: app.html } : app.resourceUri ? { uri: app.resourceUri } : undefined;
  if (source) {
    try {
      if (!backend.mcpAppFrame) throw new Error("MCP apps are not supported by this backend");
      frameUrl = await backend.mcpAppFrame(app.server, source);
    } catch (error) {
      loadError = error instanceof Error ? error.message : String(error);
    }
  }

  // Closed while the resource was read, or overtaken by a newer open.
  if (activePanels.get(key) !== entry || entry.render !== render) return;
  // Switch the context together with the HTML: until then the displayed app's
  // RPCs keep going to the backend that opened it.
  entry.backend = backend;
  entry.app = app;
  entry.panel.webview.html = renderMcpAppHtml(randomNonce(), app, frameUrl, loadError);
}

async function handlePanelMessage(entry: PanelEntry, msg: any): Promise<void> {
  if (!msg || typeof msg !== "object") return;
  // Snapshot the context: a reopen during an await must not mix backends.
  const { panel, backend, app } = entry;
  if (msg.type === "openExternal" && typeof msg.url === "string") {
    if (msg.url.startsWith("http://") || msg.url.startsWith("https://")) {
      try {
        await vscode.env.openExternal(vscode.Uri.parse(msg.url));
      } catch (error) {
        const reason = error instanceof Error ? error.message : String(error);
        void panel.webview.postMessage({ type: "hostError", message: `Could not open the browser: ${reason}` });
      }
    }
    return;
  }
  if (msg.type === "reload") {
    await openMcpAppPanel(backend, app);
    return;
  }
  if (msg.type === "rpc") {
    const { id, method, params } = msg;
    try {
      if (method === "tools/call") {
        if (!backend.callMcpTool) throw new Error("MCP tool calls are not supported by this backend");
        const name = typeof params?.name === "string" ? params.name : "";
        const args = params?.arguments && typeof params.arguments === "object" ? params.arguments : {};
        const result = await backend.callMcpTool(app.server, name, args);
        void panel.webview.postMessage({ type: "rpcReply", id, result });
      } else if (method === "resources/read") {
        if (!backend.readMcpResource) throw new Error("MCP resource reads are not supported by this backend");
        const uri = typeof params?.uri === "string" ? params.uri : "";
        const res = await backend.readMcpResource(app.server, uri);
        void panel.webview.postMessage({
          type: "rpcReply",
          id,
          result: {
            contents: [
              {
                uri: res.uri,
                mimeType: res.mime_type,
                ...(res.text ? { text: res.text } : {}),
                ...(res.blob ? { blob: res.blob } : {}),
              },
            ],
          },
        });
      }
    } catch (error) {
      void panel.webview.postMessage({
        type: "rpcReply",
        id,
        error: { code: -32000, message: error instanceof Error ? error.message : String(error) },
      });
    }
  }
}

/**
 * The origin of a URL as a CSP source, or undefined when it is not a plain http(s) origin of
 * letters, digits, dots, dashes and a port: anything else would be pasted into the policy.
 */
function originOf(url: string | undefined): string | undefined {
  if (!url) return undefined;
  try {
    const parsed = new URL(url);
    if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return undefined;
    return /^https?:\/\/[A-Za-z0-9.-]+(:\d{1,5})?$/.test(parsed.origin) ? parsed.origin : undefined;
  } catch {
    return undefined;
  }
}

/** What an external `app.url` page has always been allowed to embed, redirects included. */
const EXTERNAL_FRAME_SOURCES = "http://127.0.0.1:* http://localhost:* https:";

export function renderMcpAppHtml(
  nonce: string,
  app: McpAppDescriptor,
  frameUrl?: string,
  loadError?: string,
): string {
  const payloadJson = JSON.stringify({
    callId: app.callId,
    server: app.server,
    tool: app.tool,
    url: app.url,
    args: app.args ?? {},
    output: app.output,
    metadata: app.metadata ?? {},
  }).replace(/</g, "\\u003c");
  // A document served by WithNav is a one-time URL under its own CSP, in an opaque-origin
  // sandbox: it runs, but cannot reach this panel, VS Code's API or local files. A bare
  // `app.url` is an external page and keeps the sandbox it had.
  const showUrl = !frameUrl && !!app.url;
  const frameSrc = frameUrl ?? (showUrl ? app.url : undefined);
  const srcAttr = frameSrc ? `src="${escapeHtml(frameSrc)}"` : "";
  const sandbox = showUrl
    ? "allow-scripts allow-same-origin allow-forms allow-downloads"
    : "allow-scripts allow-forms";
  // The panel may embed only the one origin of a document WithNav served, so a frame that
  // navigates itself cannot land on another page that the bridge would still answer. An
  // external `app.url` keeps the wider list it had, since such pages redirect.
  const frameSources = showUrl ? EXTERNAL_FRAME_SOURCES : (originOf(frameSrc) ?? "'none'");
  const policy = `default-src 'none'; frame-src ${frameSources}; img-src data:; style-src 'unsafe-inline'; script-src 'nonce-${nonce}';`;

  return `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta http-equiv="Content-Security-Policy" content="${escapeHtml(policy)}">
  <title>${escapeHtml(app.server)} · ${escapeHtml(app.tool)}</title>
  <style>
    html, body { height: 100%; margin: 0; padding: 0; background: var(--vscode-editor-background); color: var(--vscode-editor-foreground); font-family: var(--vscode-font-family, sans-serif); font-size: 12px; display: flex; flex-direction: column; overflow: hidden; }
    .toolbar { display: flex; align-items: center; justify-content: space-between; padding: 6px 10px; border-bottom: 1px solid var(--vscode-panel-border, rgba(128,128,128,0.25)); gap: 8px; }
    .title { font-family: var(--vscode-editor-font-family, monospace); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .actions { display: flex; gap: 6px; }
    button { background: var(--vscode-button-secondaryBackground, rgba(128,128,128,0.2)); color: var(--vscode-button-secondaryForeground, inherit); border: 1px solid var(--vscode-panel-border, rgba(128,128,128,0.3)); border-radius: 4px; padding: 3px 8px; font: inherit; cursor: pointer; }
    .error { padding: 14px; color: var(--vscode-errorForeground, #f44); }
    iframe { flex: 1; width: 100%; border: 0; background: #fff; min-height: 0; }
  </style>
</head>
<body>
  <div class="toolbar">
    <div class="title">${escapeHtml(app.server)} · ${escapeHtml(app.tool)}</div>
    <div class="actions">
      ${app.url ? `<button type="button" id="open-ext">Open in Browser</button>` : ""}
      <button type="button" id="reload">Reload</button>
    </div>
  </div>
  ${loadError && !showUrl ? `<div class="error">${escapeHtml(loadError)}</div>` : ""}
  <div class="error" id="host-error" role="alert" hidden></div>
  ${frameSrc ? `<iframe id="mcp-frame" sandbox="${sandbox}" ${srcAttr}></iframe>` : ""}
  <script nonce="${nonce}">
    const vscode = acquireVsCodeApi();
    const app = ${payloadJson};
    const frame = document.getElementById("mcp-frame");
    document.getElementById("reload")?.addEventListener("click", () => vscode.postMessage({ type: "reload" }));
    document.getElementById("open-ext")?.addEventListener("click", () => app.url && vscode.postMessage({ type: "openExternal", url: app.url }));
    window.addEventListener("message", (event) => {
      const data = event.data;
      if (!data || typeof data !== "object") return;
      if (data.type === "hostError" && event.source !== frame?.contentWindow) {
        const box = document.getElementById("host-error");
        box.textContent = String(data.message || "");
        box.hidden = false;
        return;
      }
      if (data.type === "rpcReply" && frame?.contentWindow) {
        const msg = data.error
          ? { jsonrpc: "2.0", id: data.id, error: data.error }
          : { jsonrpc: "2.0", id: data.id, result: data.result };
        frame.contentWindow.postMessage(msg, "*");
        return;
      }
      if (!frame || event.source !== frame.contentWindow || data.jsonrpc !== "2.0") return;
      const sendToFrame = (payload) => frame.contentWindow.postMessage(payload, "*");
      const structuredContent = (app.metadata && app.metadata.mcp_structured) || app.metadata || {};
      if (data.method === "ui/initialize") {
        sendToFrame({
          jsonrpc: "2.0",
          id: data.id,
          result: {
            protocolVersion: "2025-11-25",
            hostCapabilities: { openLinks: {}, serverTools: {}, serverResources: {} },
            hostInfo: { name: "WithNav VS Code", version: "0.8.2" },
            hostContext: { toolInfo: { id: app.callId, tool: { name: app.tool } } }
          }
        });
        sendToFrame({ jsonrpc: "2.0", method: "ui/notifications/tool-input", params: { arguments: app.args } });
        if (app.output !== undefined || structuredContent) {
          sendToFrame({
            jsonrpc: "2.0",
            method: "ui/notifications/tool-result",
            params: { content: [{ type: "text", text: app.output || "" }], structuredContent }
          });
        }
        return;
      }
      if (data.method === "ui/notifications/initialized") {
        sendToFrame({ jsonrpc: "2.0", method: "ui/notifications/tool-input", params: { arguments: app.args } });
        if (app.output !== undefined || structuredContent) {
          sendToFrame({
            jsonrpc: "2.0",
            method: "ui/notifications/tool-result",
            params: { content: [{ type: "text", text: app.output || "" }], structuredContent }
          });
        }
        return;
      }
      if (data.method === "ui/open-link" || data.method === "ui/openLink") {
        const url = data.params && typeof data.params.url === "string" ? data.params.url : "";
        if (url.startsWith("http://") || url.startsWith("https://")) {
          vscode.postMessage({ type: "openExternal", url });
          sendToFrame({ jsonrpc: "2.0", id: data.id, result: {} });
        } else {
          sendToFrame({ jsonrpc: "2.0", id: data.id, error: { code: -32602, message: "Invalid link URL" } });
        }
        return;
      }
      if (data.method === "tools/call" || data.method === "resources/read") {
        vscode.postMessage({ type: "rpc", id: data.id, method: data.method, params: data.params });
      }
    });
  </script>
</body>
</html>`;
}
