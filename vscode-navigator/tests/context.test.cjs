const { test } = require("node:test");
const assert = require("node:assert/strict");
const Module = require("node:module");
const load = Module._load;
Module._load = function(id, ...args) { return id === "vscode" ? { workspace: { workspaceFolders: [] } } : load.call(this, id, ...args); };
const { CognitionBackend } = require("../out/tests/cognition.js");
Module._load = load;
const { contextMeter, tokens, parseContext } = require("../out/tests/contextMeter.js");
const snapshot = (revision = 1, extra = {}) => ({ model: "gw/model", gateway:"gw", upstream_id:"model", window: 1000000, window_source: "catalog", estimate_total: 42000,
  breakdown: { system: 842, rules: 1000, skills_catalog: 2000, tools: 3000, messages: 35158 },
  output_reserve: 100000, margin: 100000, input_budget: 800000, percent: 4, revision, incarnation: "I", ...extra });

test("next request mapping uses the server percent over the window, formats millions and labels last request", () => {
  for (const [n, text] of [[842,"842"],[42000,"42k"],[999500,"1M"],[1000000,"1M"],[1500000,"1.5M"]]) assert.equal(tokens(n), text);
  const meter = contextMeter(snapshot(1, { estimate_total: 700000, percent: 70, last_usage: { prompt_tokens: 42000, completion_tokens: 842, step: "01ARZ3NDEKTSV4RRFFQ69G5FAV" } }));
  assert.equal(meter.percent, 70);
  assert.equal(meter.level, "mid");
  assert.equal(contextMeter(snapshot(2, { estimate_total: 900000, percent: 90 })).level, "high");
  assert.match(meter.label, /^~700k \/ 1M · 70%$/);
  assert.equal(contextMeter(snapshot()).level, "low");
  assert.match(meter.detail, /Window: 1M \(catalog\)/);
  assert.match(meter.detail, /Last request · provider count: 42k prompt \+ 842 completion/);
  for (const label of ["System", "Rules", "Skills catalog", "Tools", "Messages", "Output reserve", "Safety margin"]) assert.ok(meter.detail.includes(label));
  assert.equal(parseContext(snapshot(1, { estimate_total: -1 })), undefined);
});

test("context revisions and retired incarnations cannot move the estimate backwards", () => {
  const backend = new CognitionBackend(); backend.state.session = "A";
  const seen = []; backend.subscribe(m => seen.push(m));
  backend.acceptContext(snapshot(3)); backend.acceptContext(snapshot(2)); backend.acceptContext(snapshot(3));
  assert.equal(seen.length, 1);
  backend.acceptContext(snapshot(0, { incarnation: "J" }), true);
  assert.equal(backend.state.context.incarnation, "J");
  assert.equal(seen.at(-1).generation, backend.currentGeneration());
});

test("GET and context.updated accept provider usage with a ULID request identifier", async t => {
  const backend = new CognitionBackend(); backend.endpoint = { port: 1234, token: "secret" }; backend.state.session = "A";
  const last_usage = { prompt_tokens: 42000, completion_tokens: 842, step: "01ARZ3NDEKTSV4RRFFQ69G5FAV" };
  t.mock.method(globalThis, "fetch", async () => new Response(JSON.stringify(snapshot(1, { last_usage }))));
  await backend.fetchContext(); assert.deepEqual(backend.currentContext().last_usage, last_usage);
  backend.mapEvent({ seq: 2, kind: "context.updated", payload: snapshot(2, { last_usage }) });
  assert.equal(backend.currentContext().revision, 2);
  assert.doesNotMatch(contextMeter(backend.currentContext()).detail, /step|01ARZ/);
  assert.ok(parseContext(snapshot(3, { last_usage: null })));
  assert.equal(parseContext(snapshot(3, { last_usage: { ...last_usage, step: 2 } })), undefined);
  backend.dispose();
});

test("authoritative GET J survives queued unseen incarnation I and follows later J revisions", async t => {
  const backend = new CognitionBackend(); backend.endpoint = { port: 1234, token: "secret" }; backend.state.session = "A";
  let calls = 0;
  t.mock.method(globalThis, "fetch", async () => { calls++; return new Response(JSON.stringify(snapshot(1, { incarnation: "J" }))); });
  await backend.fetchContext();
  backend.mapEvent({ seq: 2, kind: "context.updated", payload: snapshot(50) });
  assert.equal(backend.currentContext().incarnation, "J");
  await new Promise(resolve => setImmediate(resolve)); assert.equal(calls, 2);
  backend.mapEvent({ seq: 3, kind: "context.updated", payload: snapshot(2, { incarnation: "J" }) });
  backend.mapEvent({ seq: 4, kind: "context.updated", payload: snapshot(3, { incarnation: "J" }) });
  assert.equal(backend.currentContext().revision, 3);
  backend.state.retiredIncarnations.push("I");
  t.mock.method(globalThis, "fetch", async () => new Response(JSON.stringify(snapshot(0))));
  await backend.fetchContext(); assert.equal(backend.currentContext().incarnation, "I");
  assert.ok(!backend.state.retiredIncarnations.includes("I")); backend.dispose();
});

test("open fetches context; events update it; model change and compaction refresh", async t => {
  const backend = new CognitionBackend(); backend.endpoint = { port: 1234, token: "secret" };
  const paths = [];
  t.mock.method(globalThis, "fetch", async (url, init) => {
    paths.push(url);
    assert.equal(init.headers.authorization, "Bearer secret");
    const body = url.endsWith("/context") ? snapshot() : init.method === "POST" ? { session_id: "A" } : { current_seq: 0, actor_state: "idle" };
    return new Response(JSON.stringify(body));
  });
  backend.connect = async () => {}; backend.listConversations = async () => [];
  await backend.openConversation("A"); await new Promise(resolve => setImmediate(resolve));
  assert.ok(paths.some(p => p.endsWith("/A/context")));
  backend.handleFrame({ id: "2", envelope: { seq: 2, session_id: "A", kind: "context.updated", payload: snapshot(2) } });
  assert.equal(backend.state.context.revision, 2);
  backend.handleFrame({ id: "3", envelope: { seq: 3, session_id: "B", kind: "context.updated", payload: snapshot(99) } });
  assert.equal(backend.state.context.revision, 2);
  for (const kind of ["context.compacted", "model.changed"]) {
    const before = paths.length; backend.mapEvent({ seq: 4, kind, payload: {} });
    await new Promise(resolve => setImmediate(resolve)); assert.equal(paths.length, before + 1);
  }
  backend.dispose();
});

test("late HTTP snapshot and snapshot overtaken by live event are ignored", async t => {
  const backend = new CognitionBackend(); backend.endpoint = { port: 1234, token: "secret" }; backend.state.session = "A";
  let finish; t.mock.method(globalThis, "fetch", () => new Promise(resolve => { finish = resolve; }));
  const first = backend.fetchContext();
  backend.mapEvent({ seq: 1, kind: "context.updated", payload: snapshot(3) });
  finish(new Response(JSON.stringify(snapshot(99)))); await first;
  assert.equal(backend.state.context.revision, 3);
  const second = backend.fetchContext(); await backend.newConversation();
  finish(new Response(JSON.stringify(snapshot(99)))); await second;
  assert.equal(backend.state.context, undefined);
  backend.dispose();
});

test("each stream connection refreshes the next-request snapshot", async t => {
  const backend = new CognitionBackend(); backend.endpoint = { port: 1234, token: "secret" }; backend.state.session = "A";
  let reads = 0;
  t.mock.method(globalThis, "fetch", async url => {
    if (url.endsWith("/context")) { reads++; return new Response(JSON.stringify(snapshot(reads))); }
    return new Response(new ReadableStream({ start(controller) { controller.close(); } }));
  });
  // Avoid automatic timers; exercise the same connection entry point twice.
  t.mock.method(globalThis, "setTimeout", () => 0);
  await backend.connect(); await new Promise(resolve => setImmediate(resolve));
  await backend.connect(); await new Promise(resolve => setImmediate(resolve));
  assert.equal(reads, 2); assert.equal(backend.state.context.revision, 2);
  backend.dispose();
});

test("foreign-incarnation bursts coalesce into one in-flight GET and one trailing reconciliation", async t => {
  const backend = new CognitionBackend(); backend.endpoint = { port: 1234, token: "secret" }; backend.state.session = "A";
  backend.acceptContext(snapshot(1, { incarnation: "J" }), true);
  const replies = []; let concurrent = 0; let maximum = 0;
  t.mock.method(globalThis, "fetch", () => {
    concurrent++; maximum = Math.max(maximum, concurrent);
    return new Promise(resolve => replies.push(body => { concurrent--; resolve(new Response(JSON.stringify(body))); }));
  });
  const refresh = backend.fetchContext();
  for (let seq = 1; seq <= 100; seq++) backend.mapEvent({ seq, kind: "context.updated", payload: snapshot(seq, { incarnation: "I" }) });
  assert.equal(replies.length, 1); assert.equal(maximum, 1);
  assert.equal(backend.currentContext().incarnation, "J");
  replies[0](snapshot(999, { incarnation: "I" }));
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(replies.length, 2); assert.equal(maximum, 1);
  assert.equal(backend.currentContext().incarnation, "J");
  replies[1](snapshot(2, { incarnation: "J" })); await refresh;
  assert.equal(replies.length, 2); assert.equal(backend.currentContext().revision, 2);
  backend.mapEvent({ seq: 101, kind: "context.updated", payload: snapshot(3, { incarnation: "J" }) });
  assert.equal(backend.currentContext().revision, 3); assert.equal(replies.length, 2);
  backend.dispose();
});

test("real webview shows estimate and detail, rejects old generation, hides on new conversation", () => {
  const { JSDOM } = require("jsdom"); const fs = require("node:fs");
  const { chatHtml } = require("../out/tests/chatHtml.js");
  const dom = new JSDOM(chatHtml("nonce", "chat.js", "chat.css", "vscode-resource:"), { runScripts: "outside-only" });
  dom.window.acquireVsCodeApi = () => ({ postMessage() {}, getState() {}, setState() {} });
  dom.window.eval(fs.readFileSync("out/chat.js", "utf8"));
  const receive = data => dom.window.dispatchEvent(new dom.window.MessageEvent("message", { data }));
  receive({ type: "conversationOpened", id: "A", title: "Conversation A", generation: 2 });
  receive({ type: "usage", snapshot: snapshot(), generation: 2 });
  const gauge = dom.window.document.querySelector("#usage");
  assert.equal(gauge.hidden, false); assert.match(gauge.textContent, /~42k \/ 1M · 4%/);
  assert.match(gauge.title, /Skills catalog: 2k/); assert.equal(gauge.tabIndex, 0);
  receive({ type: "usage", snapshot: snapshot(99, { estimate_total: 800000, percent: 80 }), generation: 1 });
  assert.match(gauge.textContent, /4%/);
  receive({ type: "conversationOpened", title: "New conversation", generation: 3 }); receive({ type: "history", items: [], generation: 3 });
  assert.equal(gauge.hidden, true); dom.window.close();
});
