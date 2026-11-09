"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const http = require("node:http");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawnSync, execFile } = require("node:child_process");
const Module = require("node:module");

// A vscode stand-in: panels whose message listeners can be counted and driven by the test.
const panels = [];
const vscodeStub = {
  ViewColumn: { Beside: 2, Active: 1 },
  env: { openExternal: async () => true },
  Uri: { parse: (value) => value },
  window: {
    createWebviewPanel() {
      const handlers = new Set();
      const disposals = [];
      const panel = {
        title: "",
        reveal() {},
        handlers,
        posted: [],
        webview: {
          html: "",
          postMessage(message) { panel.posted.push(message); return Promise.resolve(true); },
          onDidReceiveMessage(handler) {
            handlers.add(handler);
            return { dispose: () => handlers.delete(handler) };
          },
        },
        onDidDispose(handler) { disposals.push(handler); return { dispose() {} }; },
        dispose() { for (const d of disposals) d(); },
      };
      panels.push(panel);
      return panel;
    },
  },
};
const originalLoad = Module._load;
Module._load = function (id, ...args) { return id === "vscode" ? vscodeStub : originalLoad.call(this, id, ...args); };
const { openMcpAppPanel, renderMcpAppHtml } = require("../out/tests/mcpAppPanel.js");
Module._load = originalLoad;

const app = { callId: "c1", server: "excalidraw", tool: "draw", resourceUri: "ui://excalidraw/app.html", args: {}, metadata: {} };

test("opening the same app twice keeps one message listener and asks the server for a fresh document each time", async () => {
  let frames = 0;
  let calls = 0;
  const backend = {
    async mcpAppFrame(server, source) {
      frames += 1;
      assert.equal(server, "excalidraw");
      assert.deepEqual(source, { uri: "ui://excalidraw/app.html" });
      return `http://127.0.0.1:4321/mcp-app/token${frames}`;
    },
    async callMcpTool() { calls += 1; return { content: [] }; },
  };
  await openMcpAppPanel(backend, app);
  await openMcpAppPanel(backend, app);
  const panel = panels.at(-1);
  assert.equal(frames, 2);
  assert.match(panel.webview.html, /src="http:\/\/127\.0\.0\.1:4321\/mcp-app\/token2"/);
  assert.equal(panel.handlers.size, 1, "the reload did not add a second listener");
  for (const handler of [...panel.handlers]) await handler({ type: "rpc", id: 7, method: "tools/call", params: { name: "echo", arguments: {} } });
  assert.equal(calls, 1, "one request reaches the backend once");
  panel.dispose();
  assert.equal(panel.handlers.size, 0);
});

test("a server that cannot serve the app shows the reason and no frame", async () => {
  await openMcpAppPanel({ async mcpAppFrame() { throw new Error("MCP server is disconnected"); } }, { ...app, resourceUri: "ui://other/app.html" });
  const html = panels.at(-1).webview.html;
  assert.match(html, /MCP server is disconnected/);
  assert.doesNotMatch(html, /<iframe/);
});

test("the panel policy cannot be broken out of by an unusual app url", () => {
  const html = renderMcpAppHtml("n", { ...app, url: 'https://x".test/' });
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
  const page = renderMcpAppHtml("nonce1", app, frameUrl).replace(
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
