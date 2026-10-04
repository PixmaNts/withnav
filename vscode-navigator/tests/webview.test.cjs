const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const { JSDOM } = require("jsdom");

test("real webview initializes with frozen VS Code API, renders history and posts Allow", () => {
  const dom = new JSDOM(`<div class="shell"><header class="topbar"><span class="identity">Navai</span><span id="status"><span id="status-label"></span></span></header>
    <main id="transcript"><div id="empty"></div></main><form id="composer"><textarea id="prompt"></textarea>
    <button id="send"></button><button id="stop"></button><input id="include-context" type="checkbox"><span id="context-preview"></span>
    <select id="model"></select><span id="usage"><span class="usage-bar"><span id="usage-fill"></span></span><span id="usage-label"></span></span></form></div>`, { runScripts: "outside-only" });
  const posts = [];
  const errors = [];
  dom.window.addEventListener("error", event => { errors.push(event.error); event.preventDefault(); });
  dom.window.acquireVsCodeApi = () => Object.freeze({ postMessage: message => posts.push(message), getState: () => undefined, setState() {} });
  dom.window.eval(fs.readFileSync("out/tests/chat-browser.js", "utf8"));
  assert.equal(posts[0].type, "ready");
  const receive = data => dom.window.dispatchEvent(new dom.window.MessageEvent("message", { data }));
  receive({ type: "conversationOpened", id: "A", title: "Conversation A", generation: 2 });
  receive({ type: "history", generation: 2, items: [{ type: "message", author: "driver", text: "Hello from history", id: "M", generation: 2 }] });
  assert.match(dom.window.document.querySelector("#transcript").textContent, /Hello from history/);
  receive({ type: "permission", generation: 2, card: { requestId: "RA", callId: "C", tool: "bash", agent: "build", scope: { kind: "folder", value: "/a" }, reason: "Review", call: "pwd", allowFolder: false } });
  [...dom.window.document.querySelectorAll("button")].find(button => button.textContent === "Allow once").click();
  assert.deepEqual(JSON.parse(JSON.stringify(posts.at(-1))), { type: "answerPermission", sessionId: "A", requestId: "RA", decision: "allow_once", generation: 2 });
  assert.deepEqual(errors, []);
  dom.window.close();
});
