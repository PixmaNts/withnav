import * as vscode from "vscode";
import type { HostToWebview } from "./chatProtocol";

// The editor-context block rides at the end of the driver's message text so
// the cognition can read it; the panel strips it back out and shows a chip.

const OPEN = "<editor-context>";
const CLOSE = "</editor-context>";
const MAX_OPEN_FILES = 10;
const MAX_SELECTION_CHARS = 2000;
/** Document schemes that are real files the driver works on (not output panes). */
const SCHEMES = new Set(["file", "untitled", "vscode-remote", "vscode-vfs"]);

export interface EditorContext {
  /** Block for the model, or undefined when no editor is open. */
  block?: string;
  /** Short label for the panel chip. */
  label: string;
}

interface Summary {
  path?: string;
  line?: number;
  selection?: [number, number];
  openCount: number;
}

/** Builds the editor-context block and its chip label from the current editor state. */
export function buildEditorContext(): EditorContext {
  const focused = focusedEditor();
  const open = openFiles(focused);
  if (!focused && open.length === 0) return { label: "No editor open" };

  const lines: string[] = [OPEN];
  const summary: Summary = { openCount: open.length };
  if (focused) {
    const document = focused.document;
    const cursor = focused.selection.active;
    const path = displayPath(document.uri);
    const traits = document.isDirty ? `${document.languageId}, unsaved` : document.languageId;
    summary.path = path;
    summary.line = cursor.line + 1;
    lines.push(`focused: ${path} (${traits}) line ${cursor.line + 1}, column ${cursor.character + 1}`);

    const selection = focused.selection;
    if (!selection.isEmpty) {
      const start = selection.start.line + 1;
      // A selection ending at column 1 does not include that last line.
      const end = selection.end.character === 0 && selection.end.line > selection.start.line
        ? selection.end.line
        : selection.end.line + 1;
      summary.selection = [start, end];
      let text = document.getText(selection);
      const truncated = text.length > MAX_SELECTION_CHARS;
      if (truncated) text = text.slice(0, MAX_SELECTION_CHARS);
      const fence = "`".repeat(Math.max(3, longestBacktickRun(text) + 1));
      lines.push(`selection: ${start === end ? `line ${start}` : `lines ${start}-${end}`}${truncated ? ` (first ${MAX_SELECTION_CHARS} characters)` : ""}`);
      lines.push(`${fence}${document.languageId}`, text.replace(/\n$/, ""), fence);
    }
  } else {
    lines.push("focused: none");
  }

  const shown = open.slice(0, MAX_OPEN_FILES).map(file => file.dirty ? `${file.path} (unsaved)` : file.path);
  const more = open.length > MAX_OPEN_FILES ? `, and ${open.length - MAX_OPEN_FILES} more` : "";
  lines.push(`open files (${open.length}): ${shown.join(", ")}${more}`);
  lines.push(CLOSE);
  return { block: lines.join("\n"), label: chipLabel(summary) };
}

/** Splits a message text into the driver's own text and the chip label of its context block. */
export function splitEditorContext(text: string): { text: string; context?: string } {
  const trimmed = text.trimEnd();
  if (!trimmed.endsWith(CLOSE)) return { text };
  // The first block opening on its own line is ours: selected text can only come after it.
  const marker = `\n${OPEN}\n`;
  const at = trimmed.startsWith(`${OPEN}\n`) ? 0 : trimmed.indexOf(marker);
  if (at < 0) return { text };
  const inner = trimmed.slice(at, trimmed.length - CLOSE.length).split("\n").filter(Boolean);
  inner.shift(); // the opening tag
  const summary: Summary = { openCount: 0 };
  const focused = /^focused: (.+) \([^()]*\) line (\d+), column \d+$/.exec(inner[0] ?? "");
  if (focused) {
    summary.path = focused[1];
    summary.line = Number(focused[2]);
  }
  const selection = /^selection: lines? (\d+)(?:-(\d+))?/.exec(inner[1] ?? "");
  if (selection) summary.selection = [Number(selection[1]), Number(selection[2] ?? selection[1])];
  const open = /^open files \((\d+)\):/.exec(inner[inner.length - 1] ?? "");
  if (open) summary.openCount = Number(open[1]);
  return { text: trimmed.slice(0, at).trimEnd(), context: chipLabel(summary) };
}

/** Maps a host message for display: driver messages lose their raw block and gain a chip. */
export function presentMessage(message: HostToWebview): HostToWebview {
  if (message.type === "history") return { ...message, items: message.items.map(presentMessage) };
  if (message.type !== "message" || message.author !== "driver") return message;
  const { text, context } = splitEditorContext(message.text);
  return context === undefined ? message : { ...message, text, context };
}

/** Last file editor the driver used, kept while focus sits in an output or terminal pane. */
let lastFocused: vscode.TextEditor | undefined;

function focusedEditor(): vscode.TextEditor | undefined {
  const active = vscode.window.activeTextEditor;
  if (active && SCHEMES.has(active.document.uri.scheme)) return (lastFocused = active);
  if (lastFocused && !lastFocused.document.isClosed && vscode.window.visibleTextEditors.includes(lastFocused)) return lastFocused;
  return (lastFocused = undefined);
}

function chipLabel(summary: Summary): string {
  const files = summary.openCount === 1 ? "1 file open" : `${summary.openCount} files open`;
  if (!summary.path) return files;
  if (summary.selection) {
    const [start, end] = summary.selection;
    return `${summary.path}:${start === end ? start : `${start}-${end}`} selected`;
  }
  const at = summary.line ? `${summary.path}:${summary.line}` : summary.path;
  return summary.openCount > 1 ? `${at} · ${files}` : at;
}

/** Open text files: the focused one, then other visible editors, then remaining tabs. */
function openFiles(focused: vscode.TextEditor | undefined): { path: string; dirty: boolean }[] {
  const seen = new Map<string, { path: string; dirty: boolean }>();
  const add = (uri: vscode.Uri, dirty: boolean): void => {
    if (!SCHEMES.has(uri.scheme)) return;
    const key = uri.toString();
    const existing = seen.get(key);
    if (existing) existing.dirty ||= dirty;
    else seen.set(key, { path: displayPath(uri), dirty });
  };
  if (focused) add(focused.document.uri, focused.document.isDirty);
  for (const editor of vscode.window.visibleTextEditors) add(editor.document.uri, editor.document.isDirty);
  for (const group of vscode.window.tabGroups.all) {
    for (const tab of group.tabs) {
      const input = tab.input;
      if (input instanceof vscode.TabInputText) add(input.uri, tab.isDirty);
      else if (input instanceof vscode.TabInputTextDiff) add(input.modified, tab.isDirty);
    }
  }
  return [...seen.values()];
}

function displayPath(uri: vscode.Uri): string {
  if (uri.scheme === "untitled") return uri.path;
  return vscode.workspace.asRelativePath(uri, (vscode.workspace.workspaceFolders?.length ?? 0) > 1);
}

function longestBacktickRun(text: string): number {
  let longest = 0;
  for (const run of text.match(/`+/g) ?? []) longest = Math.max(longest, run.length);
  return longest;
}
