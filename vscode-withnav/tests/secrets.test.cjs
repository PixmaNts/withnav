const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const Module = require("node:module");
const { JSDOM } = require("jsdom");
const originalLoad = Module._load;
Module._load = function (id, ...args) {
  if (id === "vscode") return { workspace: { workspaceFolders: [] } };
  return originalLoad.call(this, id, ...args);
};
const { CognitionBackend } = require("../out/tests/cognition.js");
const { eventView } = require("../out/tests/eventView.js");
Module._load = originalLoad;

const VALUE = "sk-live-7f3a9c2e1d0b4a68";
const yieldPayload = { question_call_id: "sec_1", kind: "secret", question: "Nav asks for a secret", turn_id: "T",
  details: { name: "API_KEY", label: "Deploy key", reason: "the staging deploy", default_ttl: "1h", default_scope: "workspace" } };

test("a secret yield becomes a secret question and its note settles it without a bubble", () => {
  const question = eventView({ seq: 1, kind: "agent.yield", payload: yieldPayload });
  assert.deepEqual(question.secret, { name: "API_KEY", label: "Deploy key", reason: "the staging deploy", defaultTtl: "1h", defaultScope: "workspace" });
  assert.equal(question.questionCallId, "sec_1");
  const ask = eventView({ seq: 1, kind: "agent.yield", payload: { ...yieldPayload, kind: "ask" } });
  assert.equal(ask.secret, undefined, "only the secret kind raises the card");
  const note = eventView({ seq: 2, kind: "message.received", payload: { command_id: "c", source: "user:http", content: "stored as $API_KEY",
    secret: { request_id: "sec_1", name: "API_KEY", outcome: "stored", scope: "session", ttl: "24h", expires_at: "2099-01-01T00:00:00Z" } } });
  assert.deepEqual(note.secret, { requestId: "sec_1", name: "API_KEY", outcome: "stored", scope: "session", ttl: "24h", expiresAt: "2099-01-01T00:00:00Z" });
});

function backendSetup(t, response = { accepted: true, secret: { name: "API_KEY", scope: "session", ttl: "24h", expires_at: "2099-01-01T00:00:00Z", created_at: "2026-01-01T00:00:00Z", use_count: 0, last4: "4a68" } }) {
  const backend = new CognitionBackend();
  backend.endpoint = { port: 1234, token: "tok" };
  backend.state.session = "S";
  const calls = [];
  t.mock.method(globalThis, "fetch", async (url, init) => { calls.push({ url, ...init }); return new Response(JSON.stringify(response), { status: 200 }); });
  const emitted = [];
  backend.subscribe(message => emitted.push(message));
  const message = backend.mapEvent({ seq: 1, kind: "agent.yield", payload: yieldPayload });
  backend.emit(message);
  return { backend, calls, emitted };
}

test("the host posts the value only to the secrets endpoint and keeps it nowhere", async t => {
  const { backend, calls, emitted } = backendSetup(t);
  await backend.submitSecret("S", "sec_1", "API_KEY", VALUE, "1h", "workspace");
  assert.equal(calls.length, 1);
  assert.equal(calls[0].url, "http://127.0.0.1:1234/v1/sessions/S/secrets/sec_1");
  assert.deepEqual(JSON.parse(calls[0].body), { value: VALUE, ttl: "1h", scope: "workspace" });
  assert.equal(calls[0].headers.authorization, "Bearer tok");
  const saved = emitted.find(m => m.type === "secretState");
  assert.equal(saved.state, "saved");
  assert.equal(saved.secret.last4, "4a68");
  assert.ok(!JSON.stringify(backend.history()).includes(VALUE), "the transcript never holds the value");
  assert.ok(!JSON.stringify(emitted).includes(VALUE), "no webview message carries it back");
  // Settled: a later value names its secret.
  backend.emit(backend.mapEvent({ seq: 2, kind: "message.received", payload: { command_id: "c", source: "user:http", content: "stored", secret: { request_id: "sec_1", name: "API_KEY", outcome: "stored" } } }));
  await backend.submitSecret("S", "sec_1", "API_KEY", VALUE, "24h", "session");
  assert.deepEqual(JSON.parse(calls[1].body), { name: "API_KEY", value: VALUE, ttl: "24h", scope: "session" });
});

test("decline, list and revoke use their endpoints; errors never quote the value", async t => {
  const { backend, calls, emitted } = backendSetup(t, { secrets: [{ name: "API_KEY", scope: "workspace", ttl: "1h", created_at: "x", use_count: 2, last4: "4a68" }] });
  await backend.declineSecret("S", "sec_1");
  assert.deepEqual(JSON.parse(calls[0].body), { decline: true });
  await backend.listSecrets("S");
  assert.deepEqual(emitted.find(m => m.type === "secrets").items, [{ name: "API_KEY", scope: "workspace", ttl: "1h", createdAt: "x", useCount: 2, last4: "4a68" }]);
  await backend.revokeSecret("S", "API_KEY", "sec_1");
  assert.equal(calls.find(c => c.method === "DELETE").url, "http://127.0.0.1:1234/v1/sessions/S/secrets/API_KEY");
  t.mock.method(globalThis, "fetch", async () => new Response(JSON.stringify({ error: "malformed request body: invalid" }), { status: 400 }));
  await backend.submitSecret("S", "sec_1", "API_KEY", VALUE, "1h", "session");
  const error = emitted.filter(m => m.type === "secretState").at(-1);
  assert.equal(error.state, "error");
  assert.ok(!error.message.includes(VALUE));
});

function page() {
  const { chatHtml } = require("../out/tests/chatHtml.js");
  const dom = new JSDOM(chatHtml("n", "chat.js", "chat.css", "resource:"), { runScripts: "outside-only" });
  const posts = []; const states = [];
  dom.window.acquireVsCodeApi = () => ({ postMessage: m => { const { tab, ...rest } = m; posts.push(rest); }, getState() { return states.at(-1); }, setState(s) { states.push(s); } });
  dom.window.eval(fs.readFileSync("out/chat.js", "utf8"));
  dom.window.dispatchEvent(new dom.window.MessageEvent("message", { data: { type: "tabs", tabs: [{ id: "t1", title: "S" }], active: "t1" } }));
  const d = dom.window.document;
  const receive = data => dom.window.dispatchEvent(new dom.window.MessageEvent("message", { data: { tab: "t1", ...data, generation: 1 } }));
  receive({ type: "conversationOpened", id: "S", title: "S" }); receive({ type: "status", state: "ready" });
  const typeInto = (input, text) => { input.value = text; input.dispatchEvent(new dom.window.Event("input", { bubbles: true })); };
  return { dom, d, posts, states, receive, typeInto };
}

test("the card masks the value, sends it once, clears it and settles from the server's note", () => {
  const { dom, d, posts, states, receive, typeInto } = page();
  receive({ type: "turnStart", id: "T" });
  receive({ type: "question", id: "T", text: "Nav asks", questionCallId: "sec_1", agent: "build", secret: { name: "API_KEY", label: "Deploy key", reason: "the staging deploy", defaultTtl: "24h", defaultScope: "session" } });
  const card = d.querySelector(".secret-card");
  assert.equal(card.dataset.state, "pending");
  assert.match(card.querySelector(".secret-status").textContent, /waiting for your answer/);
  assert.equal(card.querySelector(".secret-reason").textContent, "For the staging deploy. Requested by build.");
  assert.match(card.querySelector(".secret-note").textContent, /Kept out of the chat\. The agent receives \$API_KEY, not the value\./);
  assert.equal(d.querySelector(".prompt").placeholder, "Nav is waiting for the key above… (or write to answer otherwise)");
  assert.equal(d.querySelector('.tab[data-tab="t1"]').dataset.status, "waiting", "the tab shows it waits for the Captain");
  const input = card.querySelector(".secret-input");
  const save = card.querySelector(".secret-save");
  assert.equal(input.type, "password");
  assert.equal(save.disabled, true, "Save waits for a value");
  card.querySelector(".secret-eye").click();
  assert.equal(input.type, "text");
  card.querySelector(".secret-eye").click();
  assert.equal(input.type, "password");
  typeInto(input, VALUE);
  assert.equal(save.disabled, false);
  card.querySelectorAll('.secret-seg-btn')[0].click(); // 1 h
  card.querySelectorAll('.secret-seg')[1].querySelectorAll('button')[1].click(); // This workspace
  save.click();
  const sent = posts.filter(p => p.type === "submitSecret");
  assert.equal(sent.length, 1);
  assert.deepEqual(JSON.parse(JSON.stringify(sent[0])), { type: "submitSecret", sessionId: "S", requestId: "sec_1", name: "API_KEY", value: VALUE, ttl: "1h", scope: "workspace", generation: 1 });
  assert.equal(input.value, "", "the field is emptied once sent");
  assert.ok(!JSON.stringify(states).includes(VALUE), "webview state never holds it");
  receive({ type: "secretState", requestId: "sec_1", name: "API_KEY", state: "saved", secret: { name: "API_KEY", scope: "workspace", ttl: "1h", expiresAt: new Date(Date.now() + 3_600_000).toISOString(), createdAt: "", useCount: 0, last4: "4a68" } });
  receive({ type: "message", id: "c", author: "captain", text: "stored as $API_KEY", secret: { requestId: "sec_1", name: "API_KEY", outcome: "stored", scope: "workspace", ttl: "1h", expiresAt: new Date(Date.now() + 3_600_000).toISOString() } });
  assert.equal(card.dataset.state, "saved");
  assert.equal(card.querySelector(".secret-masked").textContent, "$API_KEY ••••4a68");
  assert.match(card.querySelector(".secret-expiry").textContent, /expires in (60 min|1 h), this workspace/);
  assert.equal(d.querySelectorAll(".message.captain").length, 0, "no Captain bubble for the note");
  assert.equal(d.querySelector(".prompt").placeholder, "Message WithNav…");
  [...card.querySelectorAll("button")].find(b => b.textContent === "Revoke").click();
  assert.deepEqual(JSON.parse(JSON.stringify(posts.at(-1))), { type: "revokeSecret", sessionId: "S", name: "API_KEY", requestId: "sec_1", generation: 1 });
  receive({ type: "secretState", requestId: "sec_1", name: "API_KEY", state: "revoked" });
  assert.equal(card.dataset.state, "revoked");
  assert.ok(!d.body.innerHTML.includes(VALUE));
  dom.window.close();
});

test("decline, provide anyway, and the header list with revoke", () => {
  const { dom, d, posts, receive, typeInto } = page();
  receive({ type: "turnStart", id: "T" });
  receive({ type: "question", id: "T", text: "Nav asks", questionCallId: "sec_1", secret: { name: "API_KEY", label: "Deploy key", reason: "tests", defaultTtl: "24h", defaultScope: "session" } });
  const card = d.querySelector(".secret-card");
  card.querySelector(".secret-decline").click();
  assert.deepEqual(JSON.parse(JSON.stringify(posts.at(-1))), { type: "declineSecret", sessionId: "S", requestId: "sec_1", generation: 1 });
  receive({ type: "message", id: "c", author: "captain", text: "declined", secret: { requestId: "sec_1", name: "API_KEY", outcome: "declined" } });
  assert.equal(card.dataset.state, "declined");
  assert.match(card.textContent, /Nav was told and continues without it/);
  [...card.querySelectorAll("button")].find(b => b.textContent === "Provide anyway").click();
  typeInto(card.querySelector(".secret-input"), VALUE);
  card.querySelector(".secret-save").click();
  assert.equal(posts.at(-1).type, "submitSecret");
  assert.equal(card.querySelector(".secret-input").value, "");
  const button = d.querySelector(".sh-secrets");
  assert.equal(button.hidden, false);
  button.click();
  assert.deepEqual(JSON.parse(JSON.stringify(posts.at(-1))), { type: "listSecrets", sessionId: "S", generation: 1 });
  receive({ type: "secrets", items: [{ name: "API_KEY", scope: "session", ttl: "24h", expiresAt: new Date(Date.now() + 7_200_000).toISOString(), createdAt: "", useCount: 3, last4: "4a68" }] });
  const row = d.querySelector(".secrets-row");
  assert.match(row.textContent, /\$API_KEY ••••4a68/);
  assert.match(row.textContent, /expires in 2 h · this session · used 3×/);
  row.querySelector("button").click();
  assert.deepEqual(JSON.parse(JSON.stringify(posts.at(-1))), { type: "revokeSecret", sessionId: "S", name: "API_KEY", generation: 1 });
  dom.window.close();
});
