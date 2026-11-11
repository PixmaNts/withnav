"use strict";
const { test, beforeEach } = require("node:test");
const assert = require("node:assert/strict");
const http = require("node:http");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawnSync, execFile } = require("node:child_process");
const Module = require("node:module");

// Stand-in for VS Code webview panel: each panel records its message receivers,
// posted messages and dispose handlers.
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
    handlers: messages.listeners,
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
      for (const fn of disposed.listeners) fn();
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

const { openMcpAppPanel, renderMcpAppHtml } = require("../out/tests/mcpAppPanel.js");
Module._load = originalLoad;

let nextServer = 0;
function freshApp(overrides = {}) {
  return { callId: "c1", server: `srv${nextServer++}`, tool: "draw", html: "<p>app</p>", ...overrides };
}

function backend(name = "b") {
  const calls = [];
  return {
    name,
    calls,
    mcpAppFrame: async (server, source) => {
      calls.push({ kind: "frame", server, source });
      return `http://127.0.0.1:4321/mcp-app/${name}`;
    },
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
  assert.equal(first.calls.filter(c => c.kind === "tool").length, 0);
  assert.equal(second.calls.filter(c => c.kind === "tool").length, 1);
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
  assert.equal(b.calls.filter(c => c.kind === "tool").length, 1);
});

function deferredBackend() {
  const pending = [];
  return {
    pending,
    mcpAppFrame: (server, source) =>
      new Promise(resolve => pending.push(url => resolve(url))),
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
  b.pending[1]("http://127.0.0.1:4321/NEWER");
  await newer;
  b.pending[0]("http://127.0.0.1:4321/OLDER");
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
  second.pending[0]("http://127.0.0.1:4321/SECOND");
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
  b.pending[0]("http://127.0.0.1:4321/LATE");
  await opening;
  assert.doesNotMatch(created[0].html, /LATE/);
});

const defaultApp = { callId: "c1", server: "excalidraw", tool: "draw", resourceUri: "ui://excalidraw/app.html", args: {}, metadata: {} };

test("opening the same app twice keeps one message listener and asks the server for a fresh document each time", async () => {
  let frames = 0;
  let calls = 0;
  const b = {
    async mcpAppFrame(server, source) {
      frames += 1;
      assert.equal(server, "excalidraw");
      assert.deepEqual(source, { uri: "ui://excalidraw/app.html" });
      return `http://127.0.0.1:4321/mcp-app/token${frames}`;
    },
    async callMcpTool() { calls += 1; return { content: [] }; },
  };
  await openMcpAppPanel(b, defaultApp);
  await openMcpAppPanel(b, defaultApp);
  const panel = created.at(-1);
  assert.equal(frames, 2);
  assert.match(panel.webview.html, /src="http:\/\/127\.0\.0\.1:4321\/mcp-app\/token2"/);
  assert.equal(panel.handlers.size, 1, "the reload did not add a second listener");
  for (const handler of [...panel.handlers]) await handler({ type: "rpc", id: 7, method: "tools/call", params: { name: "echo", arguments: {} } });
  assert.equal(calls, 1, "one request reaches the backend once");
  panel.dispose();
  assert.equal(panel.handlers.size, 0);
});

test("a server that cannot serve the app shows the reason and no frame", async () => {
  await openMcpAppPanel({ async mcpAppFrame() { throw new Error("MCP server is disconnected"); } }, { ...defaultApp, resourceUri: "ui://other/app.html" });
  const html = created.at(-1).webview.html;
  assert.match(html, /MCP server is disconnected/);
  assert.doesNotMatch(html, /<iframe/);
});

test("the panel policy cannot be broken out of by an unusual app url", () => {
  const html = renderMcpAppHtml("n", { ...defaultApp, url: 'https://x".test/' });
  const csp = html.match(/Content-Security-Policy" content="([^"]*)"/)[1];
  assert.match(csp, /script-src 'nonce-n';/, `the policy survived intact: ${csp}`);
  assert.match(html, /<script nonce="n">/);
});

function browser() {
  if (process.env.WITHNAV_TEST_BROWSER) return process.env.WITHNAV_TEST_BROWSER;
  for (const name of ["chromium", "chromium-browser", "google-chrome", "google-chrome-stable", "chrome"]) {
    if (spawnSync("sh", ["-c", `command -v ${name}`]).status === 0) return name;
  }
  return undefined;
}

// The shipped panel page in a real browser, with an app document served the way the WithNav
// server serves one: its own CSP header, no `allow-same-origin`. Only the VS Code API is a stand-in.
test("the shipped panel page runs an app in its sandbox: ui/initialize reaches the bridge and the parent stays unreadable", async (t) => {
  const exe = browser();
  if (!exe) { t.skip("no Chromium or Chrome on PATH (set WITHNAV_TEST_BROWSER)"); return; }
  const appPage = `<!doctype html><script>
    let parentAccess;
    try { parent.document.title; parentAccess = "readable"; } catch (error) { parentAccess = "blocked"; }
    window.addEventListener("message", (event) => {
      if (!event.data || event.data.id !== 1 || !event.data.result) return;
      parent.postMessage({ jsonrpc: "2.0", id: 2, method: "ui/open-link",
        params: { url: "https://example.com/?init=" + event.data.result.protocolVersion + "&parent=" + parentAccess } }, "*");
    });
    parent.postMessage({ jsonrpc: "2.0", id: 1, method: "ui/initialize", params: {} }, "*");
  </script>`;
  const server = http.createServer((request, response) => {
    response.writeHead(200, {
      "content-type": "text/html; charset=utf-8",
      "content-security-policy": "default-src 'none'; script-src 'unsafe-inline'; connect-src 'none'; sandbox allow-scripts allow-forms",
    });
    response.end(appPage);
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(() => server.close());
  const frameUrl = `http://127.0.0.1:${server.address().port}/mcp-app/token`;
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "withnav-panel-"));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  // Only acquireVsCodeApi is replaced: what the panel posts to VS Code lands on the root element.
  const page = renderMcpAppHtml("nonce1", defaultApp, frameUrl).replace(
    "acquireVsCodeApi()",
    `{ postMessage: (m) => { if (m.type === "openExternal") document.documentElement.setAttribute("data-opened", m.url); } }`,
  );
  fs.writeFileSync(path.join(dir, "panel.html"), page);
  // Asynchronous: the app server above must keep answering while the browser runs.
  const run = await new Promise((resolve) => execFile(exe, [
    "--headless", "--no-sandbox", "--disable-gpu", "--no-first-run", "--no-proxy-server",
    `--user-data-dir=${path.join(dir, "profile")}`, "--dump-dom", "--virtual-time-budget=10000",
    `file://${path.join(dir, "panel.html")}`,
  ], { encoding: "utf8", timeout: 60000 }, (error, stdout, stderr) => resolve({ stdout, stderr })));
  const opened = /data-opened="([^"]*)"/.exec(run.stdout ?? "");
  assert.ok(opened, `the app never reached the bridge: ${run.stdout}\n${run.stderr}`);
  const url = new URL(opened[1].replace(/&amp;/g, "&"));
  assert.equal(url.searchParams.get("init"), "2025-11-25", "ui/initialize was answered by the panel");
  assert.equal(url.searchParams.get("parent"), "blocked", "the sandboxed app cannot read the panel");
});
