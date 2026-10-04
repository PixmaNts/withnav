const { test } = require("node:test");
const assert = require("node:assert/strict");
const Module = require("node:module");
const originalLoad = Module._load;
const noopDisposable = () => ({ dispose() {} });
let provider;
const vscode = {
  window: {
    visibleTextEditors: [],
    registerWebviewViewProvider(_id, value) { provider = value; return noopDisposable(); },
    onDidChangeActiveTextEditor: noopDisposable,
    onDidChangeTextEditorSelection: noopDisposable,
    onDidChangeVisibleTextEditors: noopDisposable,
    tabGroups: { onDidChangeTabs: noopDisposable, all: [] },
  },
  workspace: { workspaceFolders: [] },
  Uri: { joinPath(_uri, ...parts) { return parts.join("/"); } },
};
Module._load = function (id, ...args) {
  if (id === "vscode") return vscode;
  return originalLoad.call(this, id, ...args);
};
const { registerChatView } = require("../out/tests/chatView.js");
Module._load = originalLoad;

test("R1 skill confirms permission cancellation before submission without editor context", async () => {
  let receive; let submitted = 0; let confirms = 0;
  vscode.window.showWarningMessage = async () => { confirms++; return "Send anyway"; };
  Object.defineProperty(vscode.window, "activeTextEditor", { configurable: true, get() { throw new Error("No editor context for skills"); } });
  const backend = { currentGeneration: () => 2, currentConversationId: () => "S", subscribe: () => () => {}, hasOpenPermissions: () => true,
    runComposerCommand: async () => { assert.equal(confirms, 1); submitted++; return false; } };
  registerChatView({ extensionUri: "extension" }, backend);
  provider.resolveWebviewView({ webview: { cspSource: "test", asWebviewUri: value => value, postMessage() {}, onDidReceiveMessage(listener) { receive = listener; return noopDisposable(); } }, onDidDispose() {} });
  receive({ type: "send", text: "/skill review check", generation: 2, includeContext: true });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(submitted, 1);
  delete vscode.window.activeTextEditor;
});

test("host routes slash commands before editor context or permission confirmation", async () => {
  let receive;
  let contextReads = 0;
  let command;
  Object.defineProperty(vscode.window, "activeTextEditor", { configurable: true, get() { contextReads++; throw new Error("Editor must not be read"); } });
  const backend = {
    currentGeneration: () => 2,
    subscribe: () => () => {},
    currentConversationId: () => "S",
    runComposerCommand: async (text, generation) => { command = { text, generation }; return false; },
    hasOpenPermissions: () => { throw new Error("Commands do not cancel permissions"); },
  };
  registerChatView({ extensionUri: "extension" }, backend);
  provider.resolveWebviewView({ webview: { cspSource: "test", asWebviewUri: value => value, postMessage() {}, onDidReceiveMessage(listener) { receive = listener; return noopDisposable(); } }, onDidDispose() {} });
  receive({ type: "send", text: "/context", includeContext: true, generation: 2 });
  await Promise.resolve(); await Promise.resolve();
  assert.deepEqual(command, { text: "/context", generation: 2 });
  assert.equal(contextReads, 0);
  delete vscode.window.activeTextEditor;
});

test("late-created and recreated views receive current session identity before replay", () => {
  let cancelled = 0;
  const backend = {
    currentContext: () => ({ incarnation: "I", revision: 3, estimate_total: 42 }),
    currentGeneration: () => 1,
    refreshCommands: async () => {},
    resumePermissionReads: () => {},
    currentConversationId: () => "already-open",
    history: () => [{ type: "pause", id: "P", kind: "budget", text: "Out of steps" }],
    subscribe: () => () => {},
    cancelPermissionReads: () => { cancelled++; },
  };
  registerChatView({ extensionUri: "extension" }, backend);
  for (let i = 0; i < 2; i++) {
    const messages = [];
    let receive;
    let dispose;
    provider.resolveWebviewView({
      webview: {
        cspSource: "test", asWebviewUri: value => value,
        postMessage(message) { messages.push(message); },
        onDidReceiveMessage(listener) { receive = listener; return noopDisposable(); },
      },
      onDidDispose(listener) { dispose = listener; },
    });
    receive({ type: "ready" });
    assert.deepEqual(messages[0], { type: "conversationOpened", id: "already-open", title: "Conversation", generation: 1 });
    assert.equal(messages[1].type, "history");
    assert.deepEqual(messages[2], { type: "usage", snapshot: backend.currentContext(), generation: 1 });
    dispose();
  }
  assert.equal(cancelled, 2);
});
