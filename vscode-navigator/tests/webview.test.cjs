const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const { JSDOM } = require("jsdom");

test("real webview initializes with frozen VS Code API, renders history and posts Allow", () => {
  const { chatHtml } = require("../out/tests/chatHtml.js");
  // jsdom does not enforce CSP; verify the nonce/CSP in the real VS Code panel manually.
  const dom = new JSDOM(chatHtml("test-nonce", "chat.js", "chat.css", "vscode-resource:"), { runScripts: "outside-only" });
  assert.match(dom.window.document.querySelector('meta[http-equiv="Content-Security-Policy"]').content, /script-src 'nonce-test-nonce'/);
  assert.equal(dom.window.document.querySelector("script").nonce, "test-nonce");
  const posts = [];
  const errors = [];
  dom.window.addEventListener("error", event => { errors.push(event.error); event.preventDefault(); });
  dom.window.acquireVsCodeApi = () => Object.freeze({ postMessage: message => posts.push(message), getState: () => undefined, setState() {} });
  dom.window.eval(fs.readFileSync("out/chat.js", "utf8"));
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
