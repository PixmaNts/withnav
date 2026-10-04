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
    dispose();
  }
  assert.equal(cancelled, 2);
});
