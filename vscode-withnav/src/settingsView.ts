import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import * as vscode from "vscode";
import { settingsHtml } from "./settingsHtml";
import { SettingsHost } from "./settingsHost";
import { parseSettingsMessage, type HostToSettings } from "./settingsProtocol";

export interface SettingsConnection {
  settingsRequest(path: string, init?: RequestInit, signal?: AbortSignal): Promise<Response>;
}

/** The `.md` defining an agent: the last directory wins, as the server loads user then project agents. */
export function findAgentDefinition(dirs: string[], id: string): string | undefined {
  if (!/^[\w.-]+$/.test(id)) return undefined;
  for (const dir of [...dirs].reverse()) {
    const file = path.join(dir, `${id}.md`);
    if (fs.existsSync(file)) return file;
  }
  return undefined;
}

/** "Navai: Open Settings": one editor tab, revealed when it is already open. */
export function registerSettingsView(context: vscode.ExtensionContext, backend: SettingsConnection): vscode.Disposable {
  let panel: vscode.WebviewPanel | undefined;
  const command = vscode.commands.registerCommand("navai.openSettings", () => {
    if (panel) {
      panel.reveal();
      return;
    }
    const current = vscode.window.createWebviewPanel("navai.settings", "Navai Settings", vscode.ViewColumn.Active, {
      enableScripts: true,
      retainContextWhenHidden: true,
      localResourceRoots: [vscode.Uri.joinPath(context.extensionUri, "out")],
    });
    panel = current;
    current.iconPath = vscode.Uri.joinPath(context.extensionUri, "media", "navai.svg");
    const webview = current.webview;
    const nonce = Buffer.from(crypto.getRandomValues(new Uint8Array(16))).toString("base64");
    const asset = (name: string) => String(webview.asWebviewUri(vscode.Uri.joinPath(context.extensionUri, "out", name)));
    webview.html = settingsHtml(nonce, asset("settings.js"), asset("settings.css"), webview.cspSource);

    const pending = new AbortController();
    const host = new SettingsHost(
      { request: (p, init) => backend.settingsRequest(p, init, pending.signal) },
      (message: HostToSettings) => void webview.postMessage(message),
      {
        workspace: vscode.workspace.workspaceFolders?.[0]?.uri.fsPath,
        home: os.homedir(),
        findDefinition: findAgentDefinition,
        onSaved: () => vscode.window.setStatusBarMessage("$(check) Navai · settings saved", 3000),
      },
    );

    const subscription = webview.onDidReceiveMessage((raw: unknown) => {
      const message = parseSettingsMessage(raw);
      if (!message) return;
      const snapshot = host.current();
      switch (message.type) {
        case "openFolder":
          void vscode.commands.executeCommand("workbench.action.files.openFolder");
          break;
        case "open": {
          const file = message.target === "config" ? snapshot?.settings.paths.config : snapshot?.settings.paths.models;
          if (file) void openFile(file);
          break;
        }
        case "openAgent": {
          const agent = snapshot?.agents.find((a) => a.id === message.id);
          if (agent?.definition) void openFile(agent.definition);
          break;
        }
        case "openExtensionSettings":
          void vscode.commands.executeCommand("workbench.action.openSettings", `@ext:${context.extension.id}`);
          break;
        default:
          void host.handle(message);
      }
    });
    current.onDidDispose(() => {
      pending.abort();
      host.dispose();
      subscription.dispose();
      if (panel === current) panel = undefined;
    });
  });
  return new vscode.Disposable(() => {
    command.dispose();
    panel?.dispose();
  });
}

/** Opens a file in an editor; a config.json that does not exist yet opens as a new file at that path. */
async function openFile(file: string): Promise<void> {
  try {
    const uri = fs.existsSync(file) ? vscode.Uri.file(file) : vscode.Uri.file(file).with({ scheme: "untitled" });
    await vscode.window.showTextDocument(await vscode.workspace.openTextDocument(uri), { preview: false });
  } catch (error) {
    void vscode.window.showErrorMessage(`Navai: cannot open ${file}: ${error instanceof Error ? error.message : String(error)}`);
  }
}
