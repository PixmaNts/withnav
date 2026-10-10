import * as vscode from "vscode";
import {
  DidChangeConfigurationNotification,
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  State,
  TransportKind,
} from "vscode-languageclient/node";
import {
  ClearNotification,
  DiagnosticsNotification,
  DiagnosticSeverityName,
  DiagnosticItem,
  FeedbackNotification,
  IntentNotification,
  WithNavConfig,
  RemarkNotification,
  RemarkParams,
  ServerInfoNotification,
  StatusNotification,
  Verdict,
} from "./protocol";
import { registerReflex } from "./reflex";
import { affectsSettings, readServerPath, readSetting } from "./settings";
import { CognitionBackend } from "./cognition";
import { registerChatView } from "./chatView";
import { registerSettingsView } from "./settingsView";

const MARKER = "◆ ";
const MAX_INLINE = 120;
const DIAGNOSTICS_DEBOUNCE_MS = 500;
const CLIENT_NAME = "withnav";
const CONTEXT_REMARK_VISIBLE = "withnav.remarkVisible";

interface ActiveRemark {
  id: string;
  uri: string;
  line: number;
  text: string;
}

let client: LanguageClient | undefined;
let clientServerPath: string | undefined;
let clientStartFailed = false;

let decorationType: vscode.TextEditorDecorationType;
let statusItem: vscode.StatusBarItem;
let state: "idle" | "thinking" | "error" = "idle";
let stateMessage: string | undefined;
let remarks = new Map<string, ActiveRemark>();
let lastRemark: ActiveRemark | undefined;
let diagnosticsTimers = new Map<string, ReturnType<typeof setTimeout>>();
let reflexContext: vscode.ExtensionContext | undefined;
let reflexDisposables: vscode.Disposable[] = [];
const cognition = new CognitionBackend();

export function activate(_context: vscode.ExtensionContext) {
  reflexContext = _context;
  decorationType = vscode.window.createTextEditorDecorationType({
    after: {
      margin: "0 0 0 2em",
      color: new vscode.ThemeColor("editorCodeLens.foreground"),
      fontStyle: "italic",
    },
  });

  statusItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
  statusItem.name = "WithNav";
  statusItem.command = "withnav.setIntent";
  statusItem.show();

  registerCommands();
  _context.subscriptions.push(vscode.commands.registerCommand("withnav.toggleBypassPermissions", () => cognition.toggleBypassPermissions().catch(error => vscode.window.showErrorMessage(String(error)))));
  _context.subscriptions.push(registerChatView(_context, cognition));
  _context.subscriptions.push(registerSettingsView(_context, cognition));
  _context.subscriptions.push(cognition);
  _context.subscriptions.push(vscode.commands.registerCommand("withnav.focusChat", () => vscode.commands.executeCommand("workbench.view.extension.withnav")));

  vscode.window.onDidChangeVisibleTextEditors((editors) => {
    for (const editor of editors) {
      applyDecorations(editor);
    }
  });

  // A remark is anchored to a line number: edits above it shift it, and an
  // edit next to it means the Captain is acting on it, so it is removed
  // (observed 2026-09-30: a remark stayed on screen after the Captain applied
  // it, and drifted to unrelated lines as code moved).
  vscode.workspace.onDidChangeTextDocument((event) => followEdits(event));

  vscode.languages.onDidChangeDiagnostics((event) => {
    for (const uri of event.uris) {
      if (uri.scheme !== "file") {
        continue;
      }
      scheduleDiagnosticsForward(uri);
    }
  });

  vscode.workspace.onDidChangeConfiguration((event) => {
    if (!affectsSettings(event)) {
      return;
    }
    handleConfigurationChange();
  });

  refreshStatusBar();
  if (readConfig().enabled) {
    void startClient();
  }
}

export function deactivate(): Thenable<void> | undefined {
  // Stop inline completions before the language client shuts down.
  clearAllRemarks();
  statusItem.dispose();
  decorationType.dispose();
  return client ? client.stop() : undefined;
}

function registerCommands() {
  vscode.commands.registerCommand("withnav.markUseful", () => sendFeedback("useful"));
  vscode.commands.registerCommand("withnav.markNoise", () => sendFeedback("noise"));
  vscode.commands.registerCommand("withnav.dismiss", () => sendFeedback("dismissed"));
  vscode.commands.registerCommand("withnav.setIntent", promptForIntent);
}

function readConfig(): WithNavConfig {
  return {
    enabled: readSetting<boolean>("enabled", true),
    idleMs: readSetting<number>("idleMs", 3000),
    minChangedLines: readSetting<number>("minChangedLines", 3),
    cooldownMs: readSetting<number>("cooldownMs", 20000),
  };
}

async function startClient(): Promise<void> {
  if (client) {
    return;
  }
  const serverPath = readServerPath();
  const serverOptions: ServerOptions = {
    command: serverPath,
    args: ["lsp"],
    transport: TransportKind.stdio,
  };
  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file" }],
    initializationOptions: readConfig(),
    outputChannelName: "WithNav",
  };
  const newClient = new LanguageClient(CLIENT_NAME, "WithNav", serverOptions, clientOptions);
  newClient.onNotification(RemarkNotification.type, handleRemark);
  newClient.onNotification(ClearNotification.type, (params) => removeRemark(params.uri));
  newClient.onNotification(StatusNotification.type, (params) => {
    state = params.state;
    stateMessage = params.message;
    refreshStatusBar();
  });
  newClient.onNotification(ServerInfoNotification.type, (params) => cognition.setServerInfo(params));

  try {
    await newClient.start();
    client = newClient;
    clientServerPath = serverPath;
    clientStartFailed = false;
    state = "idle";
    stateMessage = undefined;
    refreshStatusBar();
    if (reflexContext) {
      reflexDisposables = registerReflex(newClient);
      reflexContext.subscriptions.push(...reflexDisposables);
    }
  } catch (err) {
    clientStartFailed = true;
    state = "error";
    stateMessage = `cannot start server: ${serverPath} lsp (${errorText(err)})`;
    refreshStatusBar();
  }
}

async function stopClient(): Promise<void> {
  const current = client;
  client = undefined;
  clientServerPath = undefined;
  for (const disposable of reflexDisposables) {
    disposable.dispose();
  }
  reflexDisposables = [];
  if (current) {
    await current.stop(2000).catch(() => undefined);
  }
  state = "idle";
  stateMessage = undefined;
  clearAllRemarks();
  refreshStatusBar();
}

async function handleConfigurationChange(): Promise<void> {
  const config = readConfig();
  if (!config.enabled) {
    await stopClient();
    return;
  }
  if (client) {
    if (readServerPath() !== clientServerPath) {
      await stopClient();
      await startClient();
      return;
    }
    await client
      .sendNotification(DidChangeConfigurationNotification.type, { settings: config })
      .catch(() => undefined);
    return;
  }
  if (clientStartFailed) {
    // Retry in case the server binary is now available.
    await startClient();
  }
}

function handleRemark(params: RemarkParams) {
  const remark: ActiveRemark = {
    id: params.id,
    uri: params.uri,
    line: params.line,
    text: params.text,
  };
  remarks.set(params.uri, remark);
  lastRemark = remark;
  for (const editor of vscode.window.visibleTextEditors) {
    if (editor.document.uri.toString() === params.uri) {
      applyDecorations(editor);
    }
  }
  updateContextKey();
  refreshStatusBar();
}

function removeRemark(uri: string) {
  remarks.delete(uri);
  for (const editor of vscode.window.visibleTextEditors) {
    if (editor.document.uri.toString() === uri) {
      editor.setDecorations(decorationType, []);
    }
  }
  updateContextKey();
  refreshStatusBar();
}

/** Lines around the remarked one within which an edit clears the remark. */
const REMARK_NEIGHBOURHOOD = 2;

function followEdits(event: vscode.TextDocumentChangeEvent) {
  const uri = event.document.uri.toString();
  const remark = remarks.get(uri);
  if (!remark || event.contentChanges.length === 0) {
    return;
  }
  let line = remark.line;
  for (const change of event.contentChanges) {
    const start = change.range.start.line;
    const end = change.range.end.line;
    if (end < line - REMARK_NEIGHBOURHOOD) {
      const added = change.text.split("\n").length - 1;
      line += added - (end - start);
    } else if (start <= line + REMARK_NEIGHBOURHOOD) {
      // Editing right next to a remark is the strongest hint it was acted
      // on: record it, so a replay can count it, instead of losing it as an
      // unrated remark.
      if (client && client.state === State.Running) {
        void client
          .sendNotification(FeedbackNotification.type, { id: remark.id, verdict: "followed" })
          .catch(() => undefined);
      }
      removeRemark(uri);
      return;
    }
  }
  if (line !== remark.line) {
    remark.line = Math.max(0, line);
    for (const editor of vscode.window.visibleTextEditors) {
      if (editor.document.uri.toString() === uri) {
        applyDecorations(editor);
      }
    }
  }
}

function clearAllRemarks() {
  remarks.clear();
  for (const editor of vscode.window.visibleTextEditors) {
    editor.setDecorations(decorationType, []);
  }
  updateContextKey();
  refreshStatusBar();
}

function currentRemark(): ActiveRemark | undefined {
  const active = vscode.window.activeTextEditor;
  if (active) {
    const uri = active.document.uri.toString();
    const remark = remarks.get(uri);
    if (remark) {
      return remark;
    }
  }
  return lastRemark;
}

function sendFeedback(verdict: Verdict) {
  const remark = currentRemark();
  if (!remark) {
    void vscode.window.setStatusBarMessage("WithNav: no remark to rate", 3000);
    return;
  }
  if (client && client.state === State.Running) {
    void client.sendNotification(FeedbackNotification.type, { id: remark.id, verdict });
  }
  removeRemark(remark.uri);
}

async function promptForIntent() {
  const text = await vscode.window.showInputBox({
    prompt: "Set the Course — what are you working on?",
    placeHolder: "e.g. refactor the trigger policy to skip test files",
    ignoreFocusOut: false,
  });
  if (text === undefined) {
    return;
  }
  if (client && client.state === State.Running) {
    await client.sendNotification(IntentNotification.type, { text }).catch(() => undefined);
  }
}

function applyDecorations(editor: vscode.TextEditor) {
  const uri = editor.document.uri.toString();
  const remark = remarks.get(uri);
  if (!remark) {
    editor.setDecorations(decorationType, []);
    return;
  }
  const document = editor.document;
  const line = clampLine(remark.line, document.lineCount);
  const endOfLine = document.lineAt(line).range.end;
  const decoration: vscode.DecorationOptions = {
    range: new vscode.Range(endOfLine, endOfLine),
    renderOptions: {
      after: {
        contentText: MARKER + inlineText(remark.text),
      },
    },
    hoverMessage: remarkHover(remark.text),
  };
  editor.setDecorations(decorationType, [decoration]);
}

/** The remark's full text plus clickable verdicts, so rating does not depend on remembering shortcuts. */
function remarkHover(text: string): vscode.MarkdownString {
  const hover = new vscode.MarkdownString(
    `${escapeMarkdown(text)}\n\n` +
      "[Useful](command:withnav.markUseful) · " +
      "[Noise](command:withnav.markNoise) · " +
      "[Dismiss](command:withnav.dismiss)",
  );
  hover.isTrusted = {
    enabledCommands: ["withnav.markUseful", "withnav.markNoise", "withnav.dismiss"],
  };
  return hover;
}

function clampLine(line: number, lineCount: number): number {
  return Math.max(0, Math.min(line, lineCount - 1));
}

function inlineText(text: string): string {
  const singleLine = text.replace(/\s+/g, " ").trim();
  if (singleLine.length > MAX_INLINE) {
    return singleLine.slice(0, MAX_INLINE - 1) + "…";
  }
  return singleLine;
}

function escapeMarkdown(text: string): string {
  return text.replace(/([\\`*_{}[\]()#+!~|<>-])/g, "\\$1");
}

function updateContextKey() {
  void vscode.commands.executeCommand("setContext", CONTEXT_REMARK_VISIBLE, remarks.size > 0);
}

function refreshStatusBar() {
  const hasRemark = remarks.size > 0;
  let text: string;
  let tooltip: string;
  if (state === "error") {
    text = "$(error) WithNav";
    tooltip = stateMessage ?? "WithNav error";
  } else if (hasRemark) {
    text = "$(comment) WithNav";
    tooltip = "Remark available — rate it: useful (Ctrl+Alt+Y), noise (Ctrl+Alt+N), dismiss (Ctrl+Alt+D)";
  } else if (state === "thinking") {
    text = "$(sync~spin) WithNav";
    tooltip = "WithNav is thinking…";
  } else {
    text = "Nav · drift —";
    tooltip = "Nav idle — click to set your current intent";
  }
  statusItem.text = text;
  statusItem.tooltip = tooltip;
  statusItem.backgroundColor =
    state === "error" ? new vscode.ThemeColor("statusBarItem.errorBackground") : undefined;
}

function scheduleDiagnosticsForward(uri: vscode.Uri) {
  if (!client || client.state !== State.Running) {
    return;
  }
  const key = uri.toString();
  const existing = diagnosticsTimers.get(key);
  if (existing) {
    clearTimeout(existing);
  }
  const timer = setTimeout(() => {
    diagnosticsTimers.delete(key);
    forwardDiagnostics(uri);
  }, DIAGNOSTICS_DEBOUNCE_MS);
  diagnosticsTimers.set(key, timer);
}

function forwardDiagnostics(uri: vscode.Uri) {
  if (!client || client.state !== State.Running) {
    return;
  }
  const items: DiagnosticItem[] = [];
  for (const diagnostic of vscode.languages.getDiagnostics(uri)) {
    if (diagnostic.source === CLIENT_NAME) {
      continue;
    }
    items.push({
      line: Math.max(0, diagnostic.range.start.line),
      severity: severityName(diagnostic.severity),
      message: diagnostic.message,
      source: diagnostic.source,
    });
  }
  void client
    .sendNotification(DiagnosticsNotification.type, { uri: uri.toString(), items })
    .catch(() => undefined);
}

function severityName(severity: vscode.DiagnosticSeverity | undefined): DiagnosticSeverityName {
  switch (severity) {
    case vscode.DiagnosticSeverity.Error:
      return "error";
    case vscode.DiagnosticSeverity.Warning:
      return "warning";
    case vscode.DiagnosticSeverity.Information:
      return "info";
    case vscode.DiagnosticSeverity.Hint:
      return "hint";
    default:
      return "info";
  }
}

function errorText(err: unknown): string {
  if (err instanceof Error) {
    return err.message;
  }
  return String(err);
}
