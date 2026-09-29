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
  NavigatorConfig,
  RemarkNotification,
  RemarkParams,
  StatusNotification,
  Verdict,
} from "./protocol";
import { registerReflex } from "./reflex";

const MARKER = "◆ ";
const MAX_INLINE = 120;
const DIAGNOSTICS_DEBOUNCE_MS = 500;
const CLIENT_NAME = "navigator";
const CONTEXT_REMARK_VISIBLE = "navigator.remarkVisible";

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
  statusItem.name = "Navigator";
  statusItem.command = "navigator.setIntent";
  statusItem.show();

  registerCommands();

  vscode.window.onDidChangeVisibleTextEditors((editors) => {
    for (const editor of editors) {
      applyDecorations(editor);
    }
  });

  vscode.languages.onDidChangeDiagnostics((event) => {
    for (const uri of event.uris) {
      if (uri.scheme !== "file") {
        continue;
      }
      scheduleDiagnosticsForward(uri);
    }
  });

  vscode.workspace.onDidChangeConfiguration((event) => {
    if (!event.affectsConfiguration("navigator")) {
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
  vscode.commands.registerCommand("navigator.markUseful", () => sendFeedback("useful"));
  vscode.commands.registerCommand("navigator.markNoise", () => sendFeedback("noise"));
  vscode.commands.registerCommand("navigator.dismiss", () => sendFeedback("dismissed"));
  vscode.commands.registerCommand("navigator.setIntent", promptForIntent);
}

function readConfig(): NavigatorConfig {
  const config = vscode.workspace.getConfiguration("navigator");
  return {
    enabled: config.get<boolean>("enabled", true),
    idleMs: config.get<number>("idleMs", 3000),
    minChangedLines: config.get<number>("minChangedLines", 3),
    cooldownMs: config.get<number>("cooldownMs", 20000),
    modelAlias: config.get<string>("modelAlias", "metacognition"),
  };
}

function readServerPath(): string {
  return vscode.workspace.getConfiguration("navigator").get<string>("serverPath", "rust-agent-lsp");
}

async function startClient(): Promise<void> {
  if (client) {
    return;
  }
  const serverPath = readServerPath();
  const serverOptions: ServerOptions = {
    command: serverPath,
    transport: TransportKind.stdio,
  };
  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file" }],
    initializationOptions: readConfig(),
    outputChannelName: "Navigator",
  };
  const newClient = new LanguageClient(CLIENT_NAME, "Navigator", serverOptions, clientOptions);
  newClient.onNotification(RemarkNotification.type, handleRemark);
  newClient.onNotification(ClearNotification.type, (params) => removeRemark(params.uri));
  newClient.onNotification(StatusNotification.type, (params) => {
    state = params.state;
    stateMessage = params.message;
    refreshStatusBar();
  });

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
    stateMessage = `cannot start server: ${serverPath} (${errorText(err)})`;
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
    void vscode.window.setStatusBarMessage("Navigator: no remark to rate", 3000);
    return;
  }
  if (client && client.state === State.Running) {
    void client.sendNotification(FeedbackNotification.type, { id: remark.id, verdict });
  }
  removeRemark(remark.uri);
}

async function promptForIntent() {
  const text = await vscode.window.showInputBox({
    prompt: "Tell the navigator what you are working on",
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
    hoverMessage: new vscode.MarkdownString(escapeMarkdown(remark.text)),
  };
  editor.setDecorations(decorationType, [decoration]);
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
    text = "$(error) Navigator";
    tooltip = stateMessage ?? "Navigator error";
  } else if (hasRemark) {
    text = "$(comment) Navigator";
    tooltip = "Remark available — rate it: useful (Ctrl+Alt+Y), noise (Ctrl+Alt+N), dismiss (Ctrl+Alt+D)";
  } else if (state === "thinking") {
    text = "$(sync~spin) Navigator";
    tooltip = "Navigator is thinking…";
  } else {
    text = "$(eye) Navigator";
    tooltip = "Navigator idle — click to set your current intent";
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
