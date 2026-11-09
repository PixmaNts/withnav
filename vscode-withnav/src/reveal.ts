import { realpath } from "node:fs/promises";
import { relative, sep, isAbsolute } from "node:path";
import * as vscode from "vscode";

import type { RevealParams, RevealPlacement, RevealResult } from "./protocol";

/** Opens a workspace file from a chat link (reusing its tab) and moves the cursor. */
export async function openFile(path: string, line?: number): Promise<void> {
  const uri = await workspaceFile(path);
  if (!uri) { void vscode.window.showInformationMessage(`WithNav: file missing or outside the workspace: ${path}`); return; }
  const document = await vscode.workspace.openTextDocument(uri);
  const editor = await vscode.window.showTextDocument(document);
  const range = lineRange(document, line);
  if (!range) return;
  editor.selection = new vscode.Selection(range.start, range.start);
  editor.revealRange(range, vscode.TextEditorRevealType.InCenterIfOutsideViewport);
}

/** The cognition's `reveal`: opens the file in the requested editor group,
 *  centers the symbol or line, keeps focus in the chat. Never throws: an
 *  impossible placement or file comes back as `error`. */
export async function reveal(params: RevealParams): Promise<RevealResult> {
  const uri = await workspaceFile(params.path);
  if (!uri) return { error: `file missing or outside the workspace: ${params.path}` };
  const column = targetColumn(params.placement ?? "active");
  if (typeof column === "string") return { error: column };
  const document = await vscode.workspace.openTextDocument(uri);
  const symbol = params.symbol ? await symbolRange(document, params.symbol, params.line) : undefined;
  // Without the symbol the line places the cursor: refuse one past the end
  // rather than land elsewhere, and fall back to line 1 for a missing symbol
  // so the server can say where the cursor is.
  if (!symbol && typeof params.line === "number" && params.line > document.lineCount) {
    return { error: `line ${params.line} is past the end of ${params.path} (${document.lineCount} lines)` };
  }
  const range = symbol ?? lineRange(document, params.line) ?? (params.symbol ? new vscode.Range(0, 0, 0, 0) : undefined);
  const editor = await vscode.window.showTextDocument(document, { viewColumn: column, preserveFocus: true, preview: false });
  if (range) {
    editor.selection = new vscode.Selection(range.start, range.start);
    editor.revealRange(range, vscode.TextEditorRevealType.InCenter);
  }
  return { group: editor.viewColumn, ...(params.symbol ? { symbol_found: !!symbol } : {}) };
}

/** The view column for a placement, or why it is impossible. A neighbor is
 *  the next group after the active one, else the one before it. */
export function targetColumn(placement: RevealPlacement): vscode.ViewColumn | string {
  const active = vscode.window.tabGroups.activeTabGroup?.viewColumn ?? vscode.ViewColumn.One;
  if (placement === "active") return vscode.ViewColumn.Active;
  const columns = vscode.window.tabGroups.all.map(group => group.viewColumn).sort((a, b) => a - b);
  const neighbor = columns.find(column => column > active) ?? [...columns].reverse().find(column => column < active);
  if (placement === "neighbor") return neighbor ?? "no editor group next to the active one; use placement \"beside\" to create one";
  if (placement === "beside") return neighbor ?? vscode.ViewColumn.Beside;
  return `unknown placement: ${String(placement)}`;
}

function lineRange(document: vscode.TextDocument, line?: number): vscode.Range | undefined {
  if (typeof line !== "number" || !Number.isInteger(line) || line < 1) return undefined;
  const index = Math.min(line - 1, document.lineCount - 1);
  return new vscode.Range(index, 0, index, 0);
}

async function workspaceFile(path: string): Promise<vscode.Uri | undefined> {
  const folders = vscode.workspace.workspaceFolders;
  if (!path.trim() || !folders?.length) return undefined;
  const absolute = /^(\/|[A-Za-z]:[\\/])/.test(path);
  const candidates = absolute ? [vscode.Uri.file(path)] : folders.map(folder => vscode.Uri.joinPath(folder.uri, path));
  for (const candidate of candidates) {
    if (!vscode.workspace.getWorkspaceFolder(candidate)) continue;
    try {
      const stat = await vscode.workspace.fs.stat(candidate);
      if (!(stat.type & vscode.FileType.File)) continue;
      const realPath = await realpath(candidate.fsPath);
      const roots = await Promise.all(folders.map(folder => realpath(folder.uri.fsPath)));
      if (!roots.some(root => { const rel = relative(root, realPath); return rel === "" || (!rel.startsWith(`..${sep}`) && rel !== ".." && !isAbsolute(rel)); })) continue;
      // Open the path that was checked: the candidate may be a link retargeted since.
      return vscode.Uri.file(realPath);
    } catch { /* Try the next workspace root for missing or inaccessible paths. */ }
  }
  return undefined;
}

/** The symbol's name range: the language server's document symbols first, then a whole-word text search. */
async function symbolRange(document: vscode.TextDocument, symbol: string, line?: number): Promise<vscode.Range | undefined> {
  const name = symbol.replace(/\(\)$/, "").trim();
  if (!name) return undefined;
  const nearest = (ranges: vscode.Range[]): vscode.Range | undefined =>
    line === undefined ? ranges[0] : ranges.sort((a, b) => Math.abs(a.start.line + 1 - line) - Math.abs(b.start.line + 1 - line))[0];
  try {
    const symbols = await vscode.commands.executeCommand<(vscode.DocumentSymbol | vscode.SymbolInformation)[]>(
      "vscode.executeDocumentSymbolProvider", document.uri) ?? [];
    const found: vscode.Range[] = [];
    const visit = (items: (vscode.DocumentSymbol | vscode.SymbolInformation)[]): void => {
      for (const item of items) {
        // Some servers append a signature ("send_frame(int)") or qualify the name ("Foo::send_frame").
        const bare = item.name.split("(")[0].trim();
        if (bare === name || bare.endsWith(`::${name}`) || bare.endsWith(`.${name}`)) {
          found.push("selectionRange" in item ? item.selectionRange : item.location.range);
        }
        if ("children" in item) visit(item.children);
      }
    };
    visit(symbols);
    if (found.length) return nearest(found);
  } catch { /* No symbol provider: fall back to the text. */ }
  const escaped = name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const pattern = new RegExp(`(?<![\\w$])${escaped}(?![\\w$])`, "g");
  const found: vscode.Range[] = [];
  const text = document.getText();
  for (const match of text.matchAll(pattern)) {
    const start = document.positionAt(match.index ?? 0);
    found.push(new vscode.Range(start, start.translate(0, name.length)));
    if (found.length >= 200) break;
  }
  return nearest(found);
}
