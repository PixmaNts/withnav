const { test, beforeEach } = require("node:test");
const assert = require("node:assert/strict");
const Module = require("node:module");

// Minimal VS Code stand-in: each panel records its message receivers, posted
// messages and dispose handlers so a test can count how many receivers handle
// one webview message.
const created = [];
function makeEmitter() {
  const listeners = new Set();
  return {
    listeners,
    register(fn) {
      listeners.add(fn);
      return { dispose: () => listeners.delete(fn) };
    },
    async fire(value) {
      await Promise.all([...listeners].map(fn => fn(value)));
    },
  };
}
function makePanel() {
  const messages = makeEmitter();
  const disposed = makeEmitter();
  const panel = {
    title: "",
    html: "",
    posted: [],
    messages,
    webview: {
      set html(value) {
        if (panel.isDisposed) throw new Error("Webview is disposed");
        panel.html = value;
      },
      get html() {
        return panel.html;
      },
      onDidReceiveMessage: fn => messages.register(fn),
      postMessage: msg => {
        panel.posted.push(msg);
        return Promise.resolve(true);
      },
    },
    isDisposed: false,
    reveal() {},
    onDidDispose: fn => disposed.register(fn),
    dispose() {
      panel.isDisposed = true;
      void disposed.fire();
    },
  };
  return panel;
}
const vscode = {
  ViewColumn: { Active: -1, Beside: -2 },
  window: {
    createWebviewPanel: () => {
      const panel = makePanel();
      created.push(panel);
      return panel;
    },
  },
  env: { openExternal: async () => true },
  Uri: { parse: url => url },
};
const originalLoad = Module._load;
Module._load = function (id, ...args) {
  if (id === "vscode") return vscode;
  return originalLoad.call(this, id, ...args);
};
const { openMcpAppPanel } = require("../out/tests/mcpAppPanel.js");

let nextServer = 0;
function freshApp(overrides = {}) {
  return { callId: "c1", server: `srv${nextServer++}`, tool: "draw", html: "<p>app</p>", ...overrides };
}
function backend(name = "b") {
  const calls = [];
  return {
    name,
    calls,
    callMcpTool: async (server, tool, args) => {
      calls.push({ kind: "tool", server, tool, args });
      return { ok: name };
    },
    readMcpResource: async (server, uri) => {
      calls.push({ kind: "read", server, uri });
      return { uri, mime_type: "text/html", text: `<p>${name}</p>` };
    },
  };
}
const rpcTool = { type: "rpc", id: 1, method: "tools/call", params: { name: "save", arguments: { x: 1 } } };
const rpcRead = { type: "rpc", id: 2, method: "resources/read", params: { uri: "ui://x" } };

beforeEach(() => {
  created.length = 0;
});

test("reload and reopen keep one receiver and one backend call per RPC", async () => {
  const b = backend();
  const app = freshApp();
  await openMcpAppPanel(b, app);
  assert.equal(created.length, 1);
  const panel = created[0];
  for (const step of ["reload", "reload", "reopen", "reload"]) {
    if (step === "reload") await panel.messages.fire({ type: "reload" });
    else await openMcpAppPanel(b, app);
    assert.equal(created.length, 1, `${step} reuses the panel`);
    assert.equal(panel.messages.listeners.size, 1, `${step} keeps one receiver`);
    b.calls.length = 0;
    panel.posted.length = 0;
    await panel.messages.fire(rpcTool);
    await panel.messages.fire(rpcRead);
    assert.deepEqual(
      b.calls.map(c => c.kind),
      ["tool", "read"],
      `${step}: one backend call per RPC`,
    );
    assert.equal(panel.posted.length, 2, `${step}: one reply per RPC`);
  }
});

test("a reopen from another backend routes RPCs to that backend only", async () => {
  const first = backend("first");
  const second = backend("second");
  const app = freshApp();
  await openMcpAppPanel(first, app);
  await openMcpAppPanel(second, { ...app, callId: "c2" });
  const panel = created[0];
  await panel.messages.fire({ type: "reload" });
  await panel.messages.fire(rpcTool);
  assert.equal(first.calls.length, 0);
  assert.equal(second.calls.length, 1);
  assert.match(panel.html, /"callId":"c2"/, "reload renders the latest descriptor");
});

test("a failing backend call yields one call and one error reply", async () => {
  const b = backend();
  let calls = 0;
  b.callMcpTool = async () => {
    calls += 1;
    throw new Error("boom");
  };
  const app = freshApp();
  await openMcpAppPanel(b, app);
  await openMcpAppPanel(b, app);
  const panel = created[0];
  await panel.messages.fire(rpcTool);
  assert.equal(calls, 1);
  assert.equal(panel.posted.length, 1);
  assert.equal(panel.posted[0].error.message, "boom");
});

test("closing the panel disposes its receiver; reopening starts with one", async () => {
  const b = backend();
  const app = freshApp();
  await openMcpAppPanel(b, app);
  const old = created[0];
  old.dispose();
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(old.messages.listeners.size, 0);
  await openMcpAppPanel(b, app);
  assert.equal(created.length, 2);
  const panel = created[1];
  assert.equal(panel.messages.listeners.size, 1);
  await panel.messages.fire(rpcTool);
  assert.equal(b.calls.length, 1);
});

function deferredBackend() {
  const pending = [];
  return {
    pending,
    callMcpTool: async () => ({}),
    readMcpResource: (server, uri) =>
      new Promise(resolve => pending.push(text => resolve({ uri, mime_type: "text/html", text }))),
  };
}

test("the latest render wins when resource reads finish out of order", async () => {
  const b = deferredBackend();
  const app = freshApp({ html: undefined, resourceUri: "ui://app" });
  const older = openMcpAppPanel(b, app);
  const newer = openMcpAppPanel(b, app);
  assert.equal(b.pending.length, 2);
  b.pending[1]("NEWER");
  await newer;
  b.pending[0]("OLDER");
  await older;
  assert.match(created[0].html, /NEWER/);
  assert.doesNotMatch(created[0].html, /OLDER/);
});

test("the displayed app keeps its backend while a reopen is loading", async () => {
  const first = backend("first");
  const second = deferredBackend();
  let secondCalls = 0;
  second.callMcpTool = async () => {
    secondCalls += 1;
    return {};
  };
  const app = freshApp({ html: undefined, resourceUri: "ui://app" });
  await openMcpAppPanel(first, app);
  const panel = created[0];
  const reopening = openMcpAppPanel(second, { ...app, callId: "c2" });
  await panel.messages.fire(rpcTool);
  assert.equal(first.calls.filter(c => c.kind === "tool").length, 1);
  assert.equal(secondCalls, 0);
  second.pending[0]("SECOND");
  await reopening;
  await panel.messages.fire(rpcTool);
  assert.equal(first.calls.filter(c => c.kind === "tool").length, 1);
  assert.equal(secondCalls, 1);
});

test("a panel closed during a pending read is not written to", async () => {
  const b = deferredBackend();
  const app = freshApp({ html: undefined, resourceUri: "ui://app" });
  const opening = openMcpAppPanel(b, app);
  created[0].dispose();
  b.pending[0]("LATE");
  await opening;
  assert.doesNotMatch(created[0].html, /LATE/);
});
