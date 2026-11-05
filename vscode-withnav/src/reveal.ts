import { realpath } from "node:fs/promises";
import { relative, sep, isAbsolute } from "node:path";
import * as vscode from "vscode";

/** Where to put the cursor: a 1-based line, a symbol name, or both (the occurrence nearest the line). */
export interface RevealTarget { line?: number; symbol?: string }

/** Opens a workspace file (reusing its tab) and moves the cursor. `agent`: the
 *  cognition's `reveal` tool, which centers the target and keeps focus in the chat. */
export async function openFile(path: string, line?: number, symbol?: string, agent = false): Promise<void> {
  const uri = await workspaceFile(path);
  if (!uri) { void vscode.window.showInformationMessage(`WithNav: file missing or outside the workspace: ${path}`); return; }
  const document = await vscode.workspace.openTextDocument(uri);
  const editor = await vscode.window.showTextDocument(document, { preserveFocus: agent, preview: !agent });
  const validLine = typeof line === "number" && Number.isInteger(line) && line > 0 ? line : undefined;
  const range = (symbol && await symbolRange(document, symbol, validLine)) ||
    (validLine ? new vscode.Range(Math.min(validLine - 1, document.lineCount - 1), 0, Math.min(validLine - 1, document.lineCount - 1), 0) : undefined);
  if (!range) return;
  editor.selection = new vscode.Selection(range.start, range.start);
  editor.revealRange(range, agent ? vscode.TextEditorRevealType.InCenter : vscode.TextEditorRevealType.InCenterIfOutsideViewport);
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
      return candidate;
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
