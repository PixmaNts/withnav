// One reusable editor panel that shows a conversation image at full size, beside the chat.
// The image arrives as a `data:` URL: the panel does no networking and runs no script.
import * as vscode from "vscode";
import { imageHtml } from "./imageHtml";

let panel: vscode.WebviewPanel | undefined;

export function showImage(name: string, url: string): void {
  if (!panel) {
    panel = vscode.window.createWebviewPanel("withnav.image", name, { viewColumn: vscode.ViewColumn.Beside, preserveFocus: true }, { enableScripts: false });
    panel.onDidDispose(() => { panel = undefined; });
  }
  panel.title = name;
  panel.webview.html = imageHtml(name, url);
  panel.reveal(vscode.ViewColumn.Beside, true);
}
