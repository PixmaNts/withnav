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

test("late-created and recreated views receive current session identity before replay", () => {
  let cancelled = 0;
  const backend = {
    currentGeneration: () => 1,
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
