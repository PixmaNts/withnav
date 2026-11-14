const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const Module = require("node:module");
const { JSDOM } = require("jsdom");

// --- A stand-in for the vscode module, enough for the chat view -----------------------------------
const originalLoad = Module._load;
const noop = () => ({ dispose() {} });
let provider;
let createdPanel;
const vscode = {
  commands: { registerCommand() { return noop(); } },
  env: { async openExternal() { return true; } },
  Disposable: { from(...items) { return { dispose() { items.forEach(item => item.dispose()); } }; } },
  ViewColumn: { Active: 1, Beside: 2 },
  window: {
    showErrorMessage() {}, visibleTextEditors: [],
    createWebviewPanel() {
      let listener;
      createdPanel = {
        posted: [], title: "", reveal() {}, onDidDispose: noop,
        webview: { html: "", postMessage(m) { createdPanel.posted.push(m); return Promise.resolve(true); }, onDidReceiveMessage(l) { listener = l; return noop(); } },
        receive: m => listener(m),
      };
      return createdPanel;
    },
    registerWebviewViewProvider(_id, value) { provider = value; return noop(); },
    onDidChangeActiveTextEditor: noop, onDidChangeTextEditorSelection: noop, onDidChangeVisibleTextEditors: noop,
    tabGroups: { onDidChangeTabs: noop, all: [] },
  },
  workspace: { workspaceFolders: [] },
  Uri: { joinPath(_uri, ...parts) { return parts.join("/"); }, parse(value) { return value; } },
};
Module._load = function (id, ...args) { return id === "vscode" ? vscode : originalLoad.call(this, id, ...args); };
const { registerChatView } = require("../out/tests/chatView.js");
const { ChatHub } = require("../out/tests/chatHub.js");
const { renderMcpAppHtml, openMcpAppPanel } = require("../out/tests/mcpAppPanel.js");
Module._load = originalLoad;
const { stripChildren, filterChildren, footerText, actionsSummary, durationLabel, childGroup } = require("../out/tests/webview/agentStrip.js");
const { visibleSessions, filterCounts, sessionStatus, matchesFilter } = require("../out/tests/webview/conversations.js");

const memento = (initial = {}) => { const values = new Map(Object.entries(initial)); return { get: (key, fallback) => (values.has(key) ? values.get(key) : fallback), update: async (key, value) => { values.set(key, value); } }; };

/** A backend with just what the hub and the chat view call. */
function fakeBackend() {
  const listeners = new Set();
  return {
    endpoint: undefined, session: undefined, generation: 1, disposed: false, opened: [], sent: [], list: [],
    setServerInfo(info) { this.endpoint = info; },
    subscribe(listener) { listeners.add(listener); return () => listeners.delete(listener); },
    emit(message) { for (const listener of [...listeners]) listener({ generation: this.generation, ...message }); },
    dispose() { this.disposed = true; listeners.clear(); },
    currentGeneration() { return this.generation; },
    currentConversationId() { return this.session; },
    async openConversation(id) { this.session = id; this.opened.push(id); },
    async listConversations() { return this.list; },
    async deleteConversation() {},
    history: () => [], currentContext: () => undefined, publishSubagents() {}, publishCompaction() {}, publishBypass() {},
    resumePermissionReads() {}, cancelPermissionReads() {}, refreshCommands: async () => {}, refreshAssignments: async () => {},
    isReadOnly: () => false, captureOperation() { return { owner: {}, generation: this.generation }; }, ownsOperation: () => true,
    hasOpenPermissions: () => false,
    async runComposerCommand() { return true; },
    async send(text) { this.sent.push(text); },
    async stop() { this.stopped = true; },
    selectModel(name) { this.model = name; },
  };
}
function makeHub(saved, notifyBinding) {
  const backends = []; const lister = fakeBackend(); const saves = [];
  const hub = new ChatHub(
    () => { const b = fakeBackend(); backends.push(b); return b; },
    lister,
    { load: () => saved, save: value => saves.push(value) },
    () => undefined,
    notifyBinding,
  );
  return { hub, backends, lister, saves };
}

// --- The hub: one backend per tab -------------------------------------------------------------------
test("hub: every tab gets its own backend and every endpoint announcement reaches all of them", () => {
  const { hub, backends, lister } = makeHub();
  assert.equal(hub.tabs().length, 1, "an empty conversation is open at start");
  const second = hub.open("S2");
  assert.equal(backends.length, 2);
  assert.deepEqual(backends[1].opened, ["S2"]);
  const info = { port: 1, token: "t" };
  hub.setServerInfo(info);
  assert.deepEqual([lister.endpoint, backends[0].endpoint, backends[1].endpoint], [info, info, info]);
  const third = hub.open();
  assert.deepEqual(backends[2].endpoint, info, "a tab opened later gets the endpoint at once");
  assert.notEqual(second, third);
  assert.notEqual(hub.backend(second), hub.backend(third));
});

test("hub: opening a saved session twice focuses its tab; messages are tagged with their tab; the list is global", () => {
  const { hub, backends } = makeHub();
  const [firstId] = hub.tabs().map(tab => tab.id);
  const a = hub.open("A");
  assert.equal(hub.open("A"), a);
  assert.equal(hub.active(), a);
  assert.equal(backends.length, 2);
  const seen = []; const lists = [];
  hub.subscribe((tab, message) => seen.push([tab, message.type]));
  hub.onConversations(items => lists.push(items.map(item => item.id)));
  backends[0].emit({ type: "turnStart", id: "T" });
  backends[1].emit({ type: "delta", id: "U", text: "x" });
  backends[1].emit({ type: "conversations", items: [{ id: "A", title: "a", updatedAt: "", messageCount: 1 }] });
  assert.deepEqual(seen, [[firstId, "turnStart"], [a, "delta"]], "the list is not a tab message");
  assert.deepEqual(lists, [["A"]]);
  backends[1].emit({ type: "conversationOpened", id: "A", title: "Renamed" });
  assert.equal(hub.tabs().find(tab => tab.id === a).title, "Renamed");
  assert.equal(hub.tabs().find(tab => tab.id === a).sessionId, "A");
});

test("hub: closing a tab disposes its backend and focuses a neighbour; the open sessions are saved", () => {
  const { hub, backends, saves } = makeHub();
  const a = hub.open("A"); const b = hub.open("B");
  hub.focus(a);
  hub.close(a);
  assert.equal(backends[1].disposed, true);
  assert.equal(hub.backend(a), undefined);
  assert.equal(hub.active(), b);
  assert.deepEqual(saves.at(-1), { sessions: [null, "B"], active: 1 });
  hub.close(b); hub.close(hub.tabs()[0].id);
  assert.deepEqual(hub.tabs(), [], "the last tab can go: the page shows the session list");
});

test("hub: tabs are restored from the store and reopen their sessions after a server restart", () => {
  const { hub, backends } = makeHub({ sessions: ["A", null, "B"], active: 2 });
  assert.equal(hub.tabs().length, 3);
  assert.deepEqual(backends.map(b => b.opened), [["A"], [], ["B"]]);
  assert.equal(hub.active(), hub.tabs()[2].id);
  hub.setServerInfo({ port: 1, token: "t" });
  hub.setServerInfo({ port: 2, token: "t" });
  assert.deepEqual(backends.map(b => b.opened), [["A", "A"], [], ["B", "B"]], "each tab with a session reopens it on the new server");
  assert.deepEqual(backends.map(b => b.endpoint.port), [2, 2, 2]);
});

test("hub: a conversation's tabs close only once the server delete succeeds; a refused delete keeps tabs and list", async () => {
  const { hub, lister } = makeHub();
  lister.list = [{ id: "A", title: "A" }, { id: "B", title: "B" }];
  await hub.listConversations();
  const a = hub.open("A");
  let pending;
  lister.deleteConversation = () => new Promise((resolve, reject) => { pending = { resolve, reject }; });
  const refused = hub.deleteConversation("A");
  assert.notEqual(hub.backend(a), undefined, "the tab stays open while the delete is in flight");
  pending.reject(new Error("HTTP 500"));
  await assert.rejects(refused, /HTTP 500/);
  assert.notEqual(hub.backend(a), undefined, "a refused delete keeps the tab");
  assert.deepEqual(hub.conversations().map(item => item.id), ["A", "B"]);
  const accepted = hub.deleteConversation("A");
  assert.notEqual(hub.backend(a), undefined);
  pending.resolve();
  await accepted;
  assert.equal(hub.backend(a), undefined, "the tab closes once the server dropped the conversation");
  assert.deepEqual(hub.conversations().map(item => item.id), ["B"]);
});

test("hub: a failed coalesced list refresh is reported, unless the hub was disposed meanwhile", async t => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const flush = () => new Promise(resolve => setImmediate(resolve));
  const errors = []; const backends = []; const lister = fakeBackend();
  const hub = new ChatHub(() => { const b = fakeBackend(); backends.push(b); return b; }, lister, undefined, error => errors.push(error.message));
  hub.setServerInfo({ port: 1, token: "t" });
  let pending;
  lister.listConversations = () => new Promise((_resolve, reject) => { pending = reject; });
  backends[0].emit({ type: "turnEnd", id: "T" });
  t.mock.timers.tick(800);
  pending(new Error("HTTP 503"));
  await flush();
  assert.deepEqual(errors, ["HTTP 503"]);
  backends[0].emit({ type: "turnEnd", id: "U" });
  t.mock.timers.tick(800);
  hub.dispose();
  pending(new Error("late"));
  await flush();
  assert.deepEqual(errors, ["HTTP 503"], "a refresh failing after disposal stays silent");
});

test("hub: binding notification is sent on activate and restart, not on background restore, and unbinds on close", () => {
  const bindings = [];
  const notifyBinding = params => bindings.push({ ...params });
  const saved = { sessions: ["S1", "S2", "S3"], active: 0 };
  const { hub } = makeHub(saved, notifyBinding);

  // Background restore: only active tab S1 should be bound, background tabs S2 and S3 are not bound
  assert.deepEqual(bindings, [
    { sessionId: "S1", focus: true },
  ]);

  // Tab activation sends focus: true for the focused tab
  const tabs = hub.tabs();
  hub.focus(tabs[1].id);
  assert.deepEqual(bindings, [
    { sessionId: "S1", focus: true },
    { sessionId: "S2", focus: true },
  ]);

  // Restart sends binding again for active tab even if port/token are unchanged
  hub.setServerInfo({ port: 1234, token: "token" });
  assert.deepEqual(bindings, [
    { sessionId: "S1", focus: true },
    { sessionId: "S2", focus: true },
    { sessionId: "S2", focus: true },
  ]);

  // Closing a tab sends focus: false for that tab and activates the neighbor
  hub.close(tabs[1].id);
  assert.deepEqual(bindings, [
    { sessionId: "S1", focus: true },
    { sessionId: "S2", focus: true },
    { sessionId: "S2", focus: true },
    { sessionId: "S2", focus: false },
    { sessionId: "S3", focus: true },
  ]);
});

// --- The chat view routes by tab --------------------------------------------------------------------
test("chat view: backend messages carry their tab, webview messages reach the backend of their tab and generation", async () => {
  const { hub, backends } = makeHub();
  const [firstId] = hub.tabs().map(tab => tab.id);
  const secondId = hub.open("B");
  registerChatView({ extensionUri: "extension", globalState: memento() }, hub);
  const messages = []; let receive;
  provider.resolveWebviewView({ webview: { cspSource: "t", asWebviewUri: v => v, postMessage(m) { messages.push(m); }, onDidReceiveMessage(l) { receive = l; return noop(); } }, onDidDispose() {} });
  receive({ type: "ready" });
  const tabsMessage = messages.find(m => m.type === "tabs");
  assert.deepEqual(tabsMessage.tabs.map(tab => tab.id), [firstId, secondId]);
  assert.equal(tabsMessage.active, secondId);
  backends[1].emit({ type: "delta", id: "T", text: "hi" });
  assert.deepEqual(messages.filter(m => m.type === "delta").map(m => [m.tab, m.generation]), [[secondId, 1]]);
  // Another generation on one backend only changes that tab.
  backends[0].generation = 5;
  backends[0].emit({ type: "turnStart", id: "U" });
  assert.equal(messages.filter(m => m.type === "turnStart")[0].generation, 5);
  receive({ type: "send", text: "to second", tab: secondId, generation: 1, includeContext: false });
  receive({ type: "send", text: "to first, stale", tab: firstId, generation: 1, includeContext: false });
  receive({ type: "send", text: "to first", tab: firstId, generation: 5, includeContext: false });
  receive({ type: "send", text: "no tab", generation: 1, includeContext: false });
  receive({ type: "send", text: "unknown tab", tab: "nope", generation: 1, includeContext: false });
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(backends[1].sent, ["to second"]);
  assert.deepEqual(backends[0].sent, ["to first"]);
  receive({ type: "stop", tab: secondId, generation: 1 });
  assert.equal(backends[1].stopped, true); assert.equal(backends[0].stopped, undefined);
  receive({ type: "closeTab", tab: secondId });
  assert.equal(hub.backend(secondId), undefined);
  assert.deepEqual(messages.filter(m => m.type === "tabs").at(-1).tabs.map(tab => tab.id), [firstId]);
  receive({ type: "openTab", sessionId: "C" });
  assert.equal(hub.tabs().length, 2);
  receive({ type: "openTab", sessionId: 42 });
  assert.equal(hub.tabs().length, 2, "a session id must be a string");
});

test("chat view: an image answer arriving after the tab switched conversation is dropped", async () => {
  const { hub, backends } = makeHub();
  const tab = hub.open("B");
  const backend = backends[1];
  const pending = [];
  backend.imageDataUrl = () => new Promise(resolve => pending.push(resolve));
  registerChatView({ extensionUri: "extension", globalState: memento() }, hub);
  const messages = []; let receive;
  provider.resolveWebviewView({ webview: { cspSource: "t", asWebviewUri: v => v, postMessage(m) { messages.push(m); }, onDidReceiveMessage(l) { receive = l; return noop(); } }, onDidDispose() {} });
  receive({ type: "ready" });
  const flush = () => new Promise(resolve => setImmediate(resolve));
  receive({ type: "loadImage", attachmentId: "img1", tab, generation: 1 });
  pending.shift()("data:image/png;base64,AA");
  await flush();
  assert.deepEqual(messages.filter(m => m.type === "imageData").map(m => [m.tab, m.attachmentId, m.url]), [[tab, "img1", "data:image/png;base64,AA"]], "a timely answer reaches its tab");
  receive({ type: "loadImage", attachmentId: "img2", tab, generation: 1 });
  backend.generation = 2; // a stop and resume of the same conversation
  pending.shift()("data:image/png;base64,BB");
  await flush();
  assert.equal(messages.filter(m => m.type === "imageData" && m.attachmentId === "img2").length, 1, "the same conversation still gets its answer");
  receive({ type: "loadImage", attachmentId: "img3", tab, generation: 2 });
  backend.session = "C"; backend.generation = 3; // another conversation opened in this tab
  pending.shift()("data:image/png;base64,CC");
  await flush();
  assert.equal(messages.filter(m => m.type === "imageData" && m.attachmentId === "img3").length, 0, "a late answer for the previous conversation is dropped");
});

test("mcp app: a browser that fails to open shows a panel error instead of escaping as an unhandled rejection", async () => {
  const original = vscode.env.openExternal;
  vscode.env.openExternal = async () => { throw new Error("no browser found"); };
  try {
    await openMcpAppPanel({}, { server: "s", tool: "t", callId: "c", html: "<p>app</p>", url: "https://example.com" });
    await assert.doesNotReject(createdPanel.receive({ type: "openExternal", url: "https://example.com" }));
    assert.deepEqual(createdPanel.posted, [{ type: "hostError", message: "Could not open the browser: no browser found" }]);
  } finally { vscode.env.openExternal = original; }
  const dom = new JSDOM(renderMcpAppHtml("n", { server: "s", tool: "t", callId: "c" }), {
    runScripts: "dangerously",
    beforeParse(window) { window.acquireVsCodeApi = () => ({ postMessage() {} }); },
  });
  const box = dom.window.document.getElementById("host-error");
  assert.equal(box.hidden, true, "no error before one is reported");
  dom.window.dispatchEvent(new dom.window.MessageEvent("message", { data: { type: "hostError", message: "Could not open the browser: no browser found" } }));
  assert.equal(box.hidden, false);
  assert.equal(box.textContent, "Could not open the browser: no browser found");
  dom.window.close();
});

// --- The page: tabs, agent strip, session list ------------------------------------------------------
function page(beforeScript) {
  const { chatHtml } = require("../out/tests/chatHtml.js");
  const dom = new JSDOM(chatHtml("n", "chat.js", "chat.css", "resource:"), { runScripts: "outside-only", pretendToBeVisual: true });
  const posts = []; const states = [];
  dom.window.acquireVsCodeApi = () => ({ postMessage: m => posts.push(JSON.parse(JSON.stringify(m))), getState() {}, setState(state) { states.push(state); } });
  beforeScript?.(dom.window);
  dom.window.eval(fs.readFileSync("out/chat.js", "utf8"));
  const d = dom.window.document;
  const send = data => dom.window.dispatchEvent(new dom.window.MessageEvent("message", { data }));
  const tab = (id, data) => send({ generation: 1, ...data, tab: id });
  const tabs = (list, active) => send({ type: "tabs", tabs: list.map(id => ({ id, title: id })), active });
  return { dom, d, posts, states, send, tab, tabs };
}

test("page: messages are routed to the tab they name and each tab keeps its own transcript", () => {
  const { dom, d, tab, tabs, posts } = page();
  assert.equal(posts[0].type, "ready");
  tabs(["t1", "t2"], "t1");
  tab("t1", { type: "conversationOpened", id: "S1", title: "First" });
  tab("t2", { type: "conversationOpened", id: "S2", title: "Second" });
  tab("t2", { type: "message", id: "m", author: "captain", text: "only in second" });
  tab("t1", { type: "message", id: "n", author: "captain", text: "only in first" });
  tab("nope", { type: "message", id: "o", author: "captain", text: "nobody's" });
  const views = d.querySelectorAll(".tab-view");
  assert.equal(views.length, 2);
  assert.match(views[0].querySelector(".transcript").textContent, /only in first/);
  assert.doesNotMatch(views[0].querySelector(".transcript").textContent, /second|nobody/);
  assert.match(views[1].querySelector(".transcript").textContent, /only in second/);
  assert.equal(views[0].hidden, false); assert.equal(views[1].hidden, true, "only the focused tab shows");
  // Anything the page posts for a conversation names its tab.
  views[0].querySelector(".prompt").value = "hello";
  tab("t1", { type: "status", state: "ready" });
  views[0].querySelector(".composer").dispatchEvent(new dom.window.Event("submit", { cancelable: true }));
  assert.deepEqual(posts.at(-1), { type: "send", text: "hello", includeContext: true, submission: 1, tab: "t1", generation: 1 });
  // Focusing the other tab tells the host, and its composer posts with its own tab.
  d.querySelectorAll(".tab-main")[1].click();
  assert.deepEqual(posts.at(-1), { type: "focusTab", tab: "t2" });
  assert.equal(views[1].hidden, false);
  tab("t2", { type: "status", state: "ready" });
  views[1].querySelector(".prompt").value = "again";
  views[1].querySelector(".composer").dispatchEvent(new dom.window.Event("submit", { cancelable: true }));
  assert.equal(posts.at(-1).tab, "t2");
  // Closing and adding go through the host.
  d.querySelectorAll(".tab-close")[0].click();
  assert.deepEqual(posts.at(-1), { type: "closeTab", tab: "t1" });
  d.querySelector(".tab-new").click();
  assert.deepEqual(posts.at(-1), { type: "openTab" });
  tabs(["t2"], "t2");
  assert.equal(d.querySelectorAll(".tab-view").length, 1, "a tab the host closed is gone");
  dom.window.close();
});

test("page: closing conversation tabs removes their window resize listeners", () => {
  // Track resize listeners on window by identity; a closed tab that keeps one stays reachable and
  // keeps resizing a detached composer on every window resize.
  const resize = new Set();
  const { dom, tabs } = page(win => {
    const add = win.addEventListener.bind(win); const remove = win.removeEventListener.bind(win);
    win.addEventListener = (type, listener, options) => { if (type === "resize") resize.add(listener); return add(type, listener, options); };
    win.removeEventListener = (type, listener, options) => { if (type === "resize") resize.delete(listener); return remove(type, listener, options); };
  });
  try {
    const baseline = resize.size;
    for (let cycle = 0; cycle < 3; cycle++) {
      tabs(["a", "b", "c"], "a");
      assert.equal(resize.size, baseline + 3, "each open conversation listens for resizes");
      tabs([]);
      assert.equal(resize.size, baseline, "closed conversations leave no resize listener behind");
    }
  } finally {
    dom.window.close();
  }
});

test("page: tab icons show running, waiting for you and error; beyond four tabs the rest go into a menu", () => {
  const { dom, d, tab, tabs } = page();
  tabs(["a", "b", "c", "d", "e", "f"], "a");
  for (const id of ["a", "b", "c", "d", "e", "f"]) { tab(id, { type: "conversationOpened", id: `S-${id}`, title: `Title ${id}` }); tab(id, { type: "status", state: "ready" }); }
  assert.equal(d.querySelectorAll(".tab-strip .tab").length, 4);
  assert.equal(d.querySelector(".tab-overflow").textContent, "» 2");
  tab("a", { type: "turnStart", id: "T" });
  tab("b", { type: "permission", card: { requestId: "R", callId: "C", tool: "bash", agent: "build", scope: { kind: "folder", value: "/a" }, reason: "Review", call: "pwd" } });
  tab("c", { type: "status", state: "error", message: "boom" });
  const status = id => d.querySelector(`.tab[data-tab="${id}"]`).dataset.status;
  assert.deepEqual(["a", "b", "c", "d"].map(status), ["running", "waiting", "error", "done"].map((s, i) => i === 3 ? "idle" : s));
  tab("b", { type: "permissionState", requestId: "R", resolution: "allow_once" });
  assert.equal(status("b"), "done", "answering the permission releases the tab");
  // The hidden tabs are in the menu; focusing one swaps it into the strip.
  d.querySelector(".tab-overflow").click();
  assert.equal(d.querySelectorAll(".tab-menu .tab").length, 2);
  d.querySelector(".tab-menu .tab-main").click();
  assert.equal(d.querySelectorAll(".tab-strip .tab").length, 4);
  assert.ok(d.querySelector('.tab-strip .tab[data-tab="e"]'), "the focused tab is always visible");
  assert.equal(d.querySelector(".tab-overflow").textContent, "» 2");
  dom.window.close();
});

test("page: the agent strip shows three subagent chips, running first, and the dropdown lists them all", () => {
  const { dom, d, tab, tabs, posts } = page();
  tabs(["t1"], "t1");
  tab("t1", { type: "conversationOpened", id: "S", title: "S" });
  tab("t1", { type: "assignments", agents: [{ id: "build", description: "", mode: "primary", effective: null }, { id: "plan", description: "", mode: "primary", effective: null }], layers: [], current: "build", model: "gw/prov/claude-x" });
  const primary = d.querySelector(".chip-primary");
  assert.match(primary.textContent, /build/); assert.match(primary.textContent, /claude-x/);
  const items = [
    { id: "1", agent: "vigie", goal: "Look at the build\nscript please", state: "done" },
    { id: "2", agent: "lux", goal: "Review the diff", state: "running" },
    { id: "3", agent: "mousse", goal: "Rename a symbol", state: "failed" },
    { id: "4", agent: "vigie", goal: "Find usages", state: "done" },
    { id: "5", agent: "lux", goal: "Check the tests", state: "running" },
  ];
  tab("t1", { type: "subagents", parentId: "S", items });
  const chips = [...d.querySelectorAll(".chip-sub")];
  assert.equal(chips.length, 3, "at most three chips");
  assert.deepEqual(chips.map(chip => chip.dataset.state).slice(0, 2), ["running", "running"], "running ones first");
  assert.deepEqual(chips.map(chip => chip.querySelector(".chip-text").textContent.split(" ")[0]), ["@lux", "@lux", "@vigie"].map((v, i) => i === 2 ? "@mousse" : v).slice(0, 3).map((v, i) => i === 2 ? "@vigie" : v));
  assert.equal(d.querySelector(".chip-more").textContent, "+2");
  assert.equal(d.querySelector(".sa-menu").hidden, true);
  d.querySelector(".chip-more").click();
  const menu = d.querySelector(".sa-menu");
  assert.equal(menu.hidden, false);
  assert.equal(menu.querySelectorAll(".child-row").length, 5, "all of them, not three");
  assert.deepEqual([...menu.querySelectorAll(".sa-group")].map(h => h.textContent), ["Running · 2", "Error · 1", "Done · 2"]);
  assert.equal(menu.querySelector(".sa-footer").textContent, "5 agents · 2 running");
  assert.equal(menu.querySelector(".sa-goal").textContent, "Check the tests");
  // The goal is one line in a row.
  assert.ok([...menu.querySelectorAll(".sa-goal")].every(goal => !goal.textContent.includes("\n")));
  const pill = name => [...menu.querySelectorAll(".pill")].find(p => p.textContent.startsWith(name));
  pill("Running").click();
  assert.equal(d.querySelectorAll(".sa-menu .child-row").length, 2);
  d.querySelectorAll(".sa-menu .pill")[2].click();
  assert.equal(d.querySelectorAll(".sa-menu .child-row").length, 3, "Done also holds the failed one");
  d.querySelectorAll(".sa-menu .pill")[0].click();
  // Clicking a row or a chip opens that subagent.
  d.querySelector(".sa-menu .child-open").click();
  assert.deepEqual({ ...posts.at(-1), generation: undefined }, { type: "openChild", parentId: "S", childId: "5", tab: "t1", generation: undefined });
  chips[0].click();
  assert.equal(posts.at(-1).type, "openChild");
  // Three subagents or fewer: "All", no count.
  tab("t1", { type: "subagents", parentId: "S", items: items.slice(0, 2) });
  assert.equal(d.querySelectorAll(".chip-sub").length, 2);
  assert.equal(d.querySelector(".chip-more").textContent, "All");
  tab("t1", { type: "subagents", parentId: "S", items: [] });
  assert.equal(d.querySelector(".chip-more").hidden, true);
  dom.window.close();
});

test("page: the session list filters by chip and by title, lifts the sessions waiting for you and opens or focuses tabs", () => {
  const { dom, d, send, tab, tabs, posts } = page();
  tabs(["t1"], "t1");
  tab("t1", { type: "conversationOpened", id: "S-wait", title: "Waits" });
  tab("t1", { type: "status", state: "ready" });
  tab("t1", { type: "permission", card: { requestId: "R", callId: "C", tool: "bash", agent: "build", scope: { kind: "folder", value: "/a" }, reason: "Review", call: "pwd" } });
  const now = new Date().toISOString();
  send({ type: "conversations", items: [
    { id: "S-wait", title: "Waits", updatedAt: now, messageCount: 4, agent: "build", live: true, subagentCount: 2, subagentsRunning: 1, subagentsFailed: 0 },
    { id: "S-live", title: "Live one", updatedAt: now, messageCount: 1, agent: "plan", live: true },
    { id: "S-old", title: "Old fix", updatedAt: "2026-01-01T00:00:00Z", messageCount: 12, agent: "build", live: false },
    { id: "S-bad", title: "Broken sub", updatedAt: "2026-01-02T00:00:00Z", messageCount: 3, live: false, subagentCount: 1, subagentsFailed: 1 },
  ] });
  d.querySelector(".sh-back").click();
  assert.equal(d.querySelector(".sessions").hidden, false);
  assert.equal(d.querySelector(".tab-view").hidden, true);
  const chipText = () => [...d.querySelectorAll(".sl-chips .pill")].map(c => c.textContent);
  assert.deepEqual(chipText(), ["All 4", "Active 1", "Waiting for you 1", "Errors 1"]);
  assert.equal(d.querySelector(".sl-waiting").hidden, false);
  assert.deepEqual([...d.querySelectorAll(".sl-waiting .sl-title")].map(e => e.textContent), ["Waits"]);
  assert.deepEqual([...d.querySelectorAll(".sl-section:not(.sl-waiting) .sl-title")].map(e => e.textContent), ["Live one", "Broken sub", "Old fix"]);
  const waitingRow = d.querySelector(".sl-waiting .sl-row");
  assert.equal(waitingRow.dataset.status, "waiting");
  assert.match(waitingRow.querySelector(".sl-line2").textContent, /^build · 4 msgs · 2 subagents · 1 running/);
  assert.ok(waitingRow.querySelector(".sl-openmark"), "marked as open in a tab");
  assert.equal(d.querySelector('.sl-row[data-id="S-old"] .sl-openmark'), null);
  // Chips.
  const chip = name => [...d.querySelectorAll(".sl-chips .pill")].find(c => c.textContent.startsWith(name));
  chip("Errors").click();
  assert.deepEqual([...d.querySelectorAll(".sl-title")].map(e => e.textContent), ["Broken sub"]);
  chip("Active").click();
  assert.deepEqual([...d.querySelectorAll(".sl-title")].map(e => e.textContent), ["Live one"]);
  chip("All").click();
  // Search by title.
  const search = d.querySelector(".sl-search");
  search.value = "fix"; search.dispatchEvent(new dom.window.Event("input"));
  assert.deepEqual([...d.querySelectorAll(".sl-title")].map(e => e.textContent), ["Old fix"]);
  assert.match(d.querySelector(".sessions .picker-count").textContent, /1 of 4/);
  search.value = "zzz"; search.dispatchEvent(new dom.window.Event("input"));
  assert.match(d.querySelector(".sl-note").textContent, /No matching sessions/);
  search.value = ""; search.dispatchEvent(new dom.window.Event("input"));
  // A saved session opens in a tab; one already open just takes focus.
  d.querySelector('.sl-row[data-id="S-old"] .sl-open').click();
  assert.deepEqual(posts.at(-1), { type: "openTab", sessionId: "S-old" });
  d.querySelector('.sl-row[data-id="S-wait"] .sl-open').click();
  assert.equal(d.querySelector(".sessions").hidden, true);
  assert.equal(d.querySelector(".tab-view").hidden, false);
  assert.ok(!posts.some(p => p.type === "openTab" && p.sessionId === "S-wait"));
  d.querySelector(".sh-back").click();
  d.querySelector(".sl-new").click();
  assert.deepEqual(posts.at(-1), { type: "openTab" });
  d.querySelector('.sl-row[data-id="S-old"] .sl-delete').click();
  assert.deepEqual(posts.at(-1), { type: "deleteConversation", id: "S-old" });
  // No tab left: the list shows by itself.
  tabs([], undefined);
  assert.equal(d.querySelector(".sessions").hidden, false);
  assert.equal(d.querySelector(".tabbar .tab"), null);
  dom.window.close();
});

test("page: the composer has one toolbar, a context chip with a close button and no hint row or usage bar", () => {
  const { dom, d, send, tab, tabs, posts } = page();
  tabs(["t1"], "t1");
  tab("t1", { type: "conversationOpened", id: "S", title: "S" });
  tab("t1", { type: "status", state: "ready" });
  send({ type: "editorContext", label: "src/a.ts:12" });
  const view = d.querySelector(".tab-view");
  assert.equal(view.querySelectorAll(".composer .toolbar").length, 1);
  assert.equal(view.querySelector(".hint, #usage, .usage, #include-context"), null);
  assert.equal(view.querySelector(".ctx-chips .context-chip-label").textContent, "src/a.ts:12");
  const submit = text => { view.querySelector(".prompt").value = text; view.querySelector(".composer").dispatchEvent(new dom.window.Event("submit", { cancelable: true })); tab("t1", { type: "sendResult", text, accepted: true }); };
  submit("with");
  assert.equal(posts.at(-1).includeContext, true);
  view.querySelector(".chip-close").click();
  assert.equal(view.querySelector(".ctx-chips").hidden, true);
  submit("without");
  assert.equal(posts.at(-1).includeContext, false);
  assert.equal(view.querySelector(".ctx-chips").hidden, false, "the chip returns for the message after");
  submit("again");
  assert.equal(posts.at(-1).includeContext, true);
  view.querySelector(".attach-btn").click();
  assert.equal(view.querySelector(".attach-btn").getAttribute("aria-pressed"), "false");
  submit("off");
  assert.equal(posts.at(-1).includeContext, false);
  // The context ring replaces the usage bar.
  tab("t1", { type: "usage", snapshot: { model: "m", window: 1000000, window_source: "catalog", estimate_total: 420000, breakdown: { system: 1, rules: 1, skills_catalog: 1, tools: 1, messages: 1 }, output_reserve: 1, margin: 1, input_budget: 900000, percent: 42, revision: 1 } });
  assert.equal(view.querySelector(".ctx-ring").hidden, false);
  assert.equal(view.querySelector(".ring-label").textContent, "42%");
  // The agent toggle and the model button sit in the toolbar.
  tab("t1", { type: "assignments", agents: [{ id: "build", description: "", mode: "primary", effective: null }, { id: "plan", description: "", mode: "primary", effective: null }], layers: [], current: "build", model: "gw/x/m1" });
  assert.equal(view.querySelector(".toolbar .agent-btn .agent-name").textContent, "build");
  assert.equal(view.querySelector(".agent-seg, .seg-btn"), null, "the segmented toggle is gone");
  view.querySelector(".agent-btn").click();
  view.querySelectorAll(".agent-row")[1].click();
  assert.deepEqual(posts.at(-1), { type: "selectAgent", id: "plan", tab: "t1", generation: 1 });
  assert.equal(view.querySelector(".model-btn .model-name").textContent, "m1");
  dom.window.close();
});

// --- Pure helpers -----------------------------------------------------------------------------------
test("strip and dropdown helpers: running first then most recent, groups, counts and action summaries", () => {
  const child = (id, state) => ({ id, agent: "a", goal: id, state });
  const items = [child("1", "done"), child("2", "running"), child("3", "permission"), child("4", "failed"), child("5", "done"), child("6", "stopped")];
  const strip = stripChildren(items);
  assert.deepEqual(strip.shown.map(c => c.id), ["3", "2", "6"]);
  assert.equal(strip.hidden, 3);
  assert.deepEqual(stripChildren(items.slice(0, 2)).hidden, 0);
  assert.deepEqual(filterChildren(items, "all").map(g => [g.group, g.items.map(c => c.id)]), [["running", ["3", "2"]], ["error", ["4"]], ["done", ["6", "5", "1"]]]);
  assert.deepEqual(filterChildren(items, "running").map(g => g.group), ["running"]);
  assert.deepEqual(filterChildren(items, "done").map(g => g.group), ["error", "done"]);
  assert.equal(footerText(items), "6 agents · 2 running");
  assert.equal(footerText([child("1", "running")]), "1 agent · 1 running");
  assert.equal(childGroup("budget"), "running");
  assert.equal(actionsSummary([{ name: "read" }, { name: "read" }, { name: "grep" }]), "3 actions · read 2 files · 1 search");
  assert.equal(actionsSummary([{ name: "edit" }, { name: "bash" }, { name: "bash" }, { name: "mcp__x" }]), "4 actions · edited 1 file · ran 2 commands · 1 other");
  assert.deepEqual(["8s", "1m 05s", "1h 02m"], [durationLabel(8000), durationLabel(65000), durationLabel(3720000)]);
});

test("session list helpers: status comes from the open tab, else the server's live flag; failed subagents count as errors", () => {
  const at = (id, extra = {}) => ({ id, title: id, updatedAt: "2026-01-01T00:00:00Z", messageCount: 1, ...extra });
  const items = [at("w"), at("l", { live: true }), at("d"), at("f", { subagentsFailed: 2 }), at("r")];
  const tabs = new Map([["w", { tab: "t1", status: "waiting" }], ["r", { tab: "t2", status: "running" }]]);
  assert.deepEqual(items.map(i => sessionStatus(i, tabs.get(i.id))), ["waiting", "active", "done", "done", "running"]);
  assert.deepEqual(filterCounts(items, tabs), { all: 5, active: 2, waiting: 1, errors: 1 });
  assert.equal(matchesFilter(items[3], undefined, "errors"), true);
  const view = visibleSessions(items, tabs, "all", "");
  assert.deepEqual(view.waiting.map(i => i.id), ["w"]);
  assert.equal(view.rest.length, 4);
  assert.equal(visibleSessions(items, tabs, "all", "zz").matches, 0);
});

// --- Regressions from the first real VS Code run ------------------------------------------------------
test("titles: a session titled by its id shows the first Captain message, else a plain label, never the id", () => {
  const { displayTitle } = require("../out/tests/sessionTitle.js");
  const id = "01M6NEJP81GA4C2NABCDEFGHJK";
  assert.equal(displayTitle(id, id), "Untitled conversation");
  assert.equal(displayTitle("", id), "Untitled conversation");
  assert.equal(displayTitle(id, id, "Fix the failing\n  build in CI please, it breaks on every push"), "Fix the failing build in CI please, it…");
  assert.equal(displayTitle("Real title", id, "ignored"), "Real title");
  assert.equal(displayTitle("", ""), "New conversation");
  const { dom, d, tab, tabs } = page();
  tabs(["t1"], "t1");
  tab("t1", { type: "conversationOpened", id, title: id });
  assert.equal(d.querySelector(".sh-title").textContent, "Untitled conversation");
  tab("t1", { type: "history", items: [{ generation: 1, type: "message", id: "m", author: "captain", text: "Rename the widget" }] });
  assert.equal(d.querySelector(".sh-title").textContent, "Rename the widget");
  assert.equal(d.querySelector(".tab-title").textContent, "Rename the widget");
  assert.ok(!d.body.textContent.includes(id));
  dom.window.close();
});

test("header strip: no duplicate agent/model chip without subagents", () => {
  const { dom, d, tab, tabs } = page();
  tabs(["t1"], "t1");
  tab("t1", { type: "conversationOpened", id: "S", title: "S" });
  tab("t1", { type: "assignments", agents: [{ id: "build", description: "", mode: "primary", effective: null }], layers: [], current: "build", model: "gw/p/claude-x" });
  assert.equal(d.querySelector(".agent-strip").hidden, true);
  tab("t1", { type: "subagents", parentId: "S", items: [{ id: "1", agent: "lux", goal: "g", state: "running" }] });
  assert.equal(d.querySelector(".agent-strip").hidden, false);
  dom.window.close();
});

test("layout: the document never scrolls; only the transcript does, and absolutely positioned bylines stay inside it", () => {
  const css = fs.readFileSync("src/webview/chat.css", "utf8");
  assert.match(css, /html, body \{[^}]*overflow: hidden/);
  assert.match(css, /\.shell \{[^}]*position: fixed; inset: 0;[^}]*overflow: hidden/);
  assert.match(css, /\.transcript \{ position: relative;/);
  assert.doesNotMatch(css, /100vh/);
});

// --- Readability pass ---------------------------------------------------------------------------------
test("markdown: a mermaid block becomes a labelled diagram card showing its source; other code blocks keep their language", () => {
  const { dom, d, tab, tabs } = page();
  tabs(["t1"], "t1");
  tab("t1", { type: "conversationOpened", id: "S", title: "S" });
  tab("t1", { type: "history", items: [{ generation: 1, type: "message", id: "m", author: "cognition", text: "```mermaid\ngraph TD\n A-->B\n```\n\n```rust\nfn main() {}\n```" }] });
  const cards = d.querySelectorAll(".code-block");
  assert.equal(cards.length, 2);
  assert.ok(cards[0].classList.contains("diagram-card"));
  assert.equal(cards[0].querySelector(".code-bar > span").textContent, "mermaid");
  assert.match(cards[0].querySelector(".diagram-note").textContent, /source/i);
  assert.match(cards[0].querySelector("code").textContent, /A-->B/);
  assert.equal(cards[1].classList.contains("diagram-card"), false);
  assert.equal(cards[1].querySelector(".code-bar > span").textContent, "rust");
  dom.window.close();
});

test("transcript: Captain bubbles show their label and Nav answers carry the crew colour", () => {
  const { dom, d, tab, tabs } = page();
  tabs(["t1"], "t1");
  tab("t1", { type: "conversationOpened", id: "S", title: "S" });
  tab("t1", { type: "history", items: [
    { generation: 1, type: "message", id: "a", author: "captain", text: "hello", context: "a.rs:1" },
    { generation: 1, type: "message", id: "b", author: "cognition", text: "- one\n- two" },
  ] });
  const captain = d.querySelector(".message.captain");
  assert.equal(captain.querySelector(".byline").textContent, "Captain");
  assert.equal(captain.querySelector(".context-chip.sent").textContent, "a.rs:1");
  assert.equal(d.querySelector(".message.cognition").dataset.by, "nav");
  dom.window.close();
});

test("styles: gutters, theme-based contrast and distinct block styles are in the stylesheet", () => {
  const css = fs.readFileSync("src/webview/chat.css", "utf8");
  assert.match(css, /\.transcript \{ padding: 4px var\(--nav-gutter\) 24px; \}/);
  assert.match(css, /--nav-muted: color-mix\(in srgb, var\(--nav-fg\)/);
  assert.match(css, /code \{[^}]*--vscode-textPreformat-background[^}]*--vscode-textPreformat-foreground/);
  assert.match(css, /\.code-block \{[^}]*border-radius: 6px/);
  assert.match(css, /\.markdown-table th \{ background/);
  assert.match(css, /li > p \{ margin: 0; \}/);
  assert.doesNotMatch(css.slice(css.indexOf("Readability pass")), /opacity: \./, "no opacity stacked on text colours");
});

// --- Responsive layout --------------------------------------------------------------------------------
test("layout: wide views keep the session list as a left column, and the choice to hide it is remembered", () => {
  const { dom, d, send, tab, tabs, posts, states } = page();
  const shell = d.querySelector(".shell");
  tabs(["t1"], "t1");
  tab("t1", { type: "conversationOpened", id: "S1", title: "One" });
  send({ type: "conversations", workspace: "navai", items: [{ id: "S1", title: "One", updatedAt: new Date().toISOString(), messageCount: 2 }, { id: "S2", title: "Two", updatedAt: new Date().toISOString(), messageCount: 1 }] });
  assert.equal(shell.dataset.wide, "false");
  assert.equal(d.querySelector(".sessions").hidden, true, "narrow: the list is a separate view");
  Object.defineProperty(d.documentElement, "clientWidth", { value: 1240, configurable: true });
  dom.window.dispatchEvent(new dom.window.Event("resize"));
  assert.equal(shell.dataset.wide, "true");
  assert.equal(shell.dataset.sidebar, "on");
  assert.equal(d.querySelector(".sessions").hidden, false);
  assert.equal(d.querySelector(".tab-view").hidden, false, "the conversation stays visible beside the list");
  assert.equal(d.querySelector(".sl-group-name").textContent, "navai");
  assert.ok(d.querySelector('.sl-row[data-id="S1"]').classList.contains("is-active"));
  assert.ok(!d.querySelector('.sl-row[data-id="S2"]').classList.contains("is-active"));
  // A click on a closed session opens a tab; the group folds; the sidebar hides and the choice is saved.
  d.querySelector('.sl-row[data-id="S2"] .sl-open').click();
  assert.deepEqual(posts.at(-1), { type: "openTab", sessionId: "S2" });
  d.querySelector(".sl-group-toggle").click();
  assert.equal(d.querySelector(".sl-section:not(.sl-waiting)").hidden, true);
  const toggle = d.querySelector(".sidebar-toggle");
  assert.equal(toggle.hidden, false);
  assert.equal(toggle.getAttribute("aria-pressed"), "true");
  assert.equal(toggle.getAttribute("aria-label"), "Hide sessions");
  toggle.click();
  assert.equal(shell.dataset.sidebar, "off");
  assert.equal(toggle.getAttribute("aria-pressed"), "false");
  assert.equal(toggle.getAttribute("aria-label"), "Show sessions");
  assert.equal(d.querySelector(".sessions").hidden, true);
  assert.equal(states.at(-1).sidebar, false);
  toggle.click();
  assert.equal(shell.dataset.sidebar, "on", "the same button brings the list back");
  assert.equal(states.at(-1).sidebar, true);
  Object.defineProperty(d.documentElement, "clientWidth", { value: 600, configurable: true });
  dom.window.dispatchEvent(new dom.window.Event("resize"));
  assert.equal(shell.dataset.wide, "false");
  assert.equal(toggle.hidden, true, "narrow: the back arrow to the list view is used instead");
  dom.window.close();
});

test("appearance: the text size from the host is applied as a CSS variable, out-of-range values are ignored", () => {
  const { dom, d, send } = page();
  send({ type: "appearance", fontSize: 16 });
  assert.equal(d.documentElement.style.getPropertyValue("--nav-font-size"), "16px");
  send({ type: "appearance", fontSize: 40 });
  send({ type: "appearance", fontSize: "big" });
  assert.equal(d.documentElement.style.getPropertyValue("--nav-font-size"), "16px");
  const css = fs.readFileSync("src/webview/chat.css", "utf8");
  assert.match(css, /--nav-fg: var\(--vscode-editor-foreground, var\(--vscode-foreground\)\);/);
  assert.match(css, /--nav-muted: color-mix\(in srgb, var\(--nav-fg\)/);
  assert.match(css, /:root \{[^}]*font-size: var\(--nav-font-size, 14px\)/);
  dom.window.close();
});

test("layout: the stylesheet centres one column for header, transcript and composer and lays out the wide grid", () => {
  const css = fs.readFileSync("src/webview/chat.css", "utf8");
  assert.match(css, /--nav-column: 760px;/);
  assert.match(css, /\.transcript \{ padding-inline: var\(--nav-column-pad\); \}/);
  assert.match(css, /\.composer \{ margin-inline: max\(12px, calc\(\(100% - var\(--nav-column\)\) \/ 2\)\); \}/);
  assert.match(css, /\.shell\[data-wide="true"\] \{ display: grid;/);
});

test("turn: two or more runs of actions fold into one Activity line with a red failed count", () => {
  const { dom, d, tab, tabs } = page();
  tabs(["t1"], "t1");
  tab("t1", { type: "conversationOpened", id: "S", title: "S" });
  tab("t1", { type: "turnStart", id: "T" });
  const run = (callId, name, ok) => { tab("t1", { type: "tool", id: "T", callId, phase: "start", name, summary: name }); tab("t1", { type: "tool", id: "T", callId, phase: "end", name, summary: name, ok }); };
  run("a", "read", true);
  assert.equal(d.querySelector(".turn-activity"), null, "one run: no Activity line");
  tab("t1", { type: "delta", id: "T", stepId: "s1", text: "between" });
  run("b", "bash", false);
  tab("t1", { type: "delta", id: "T", stepId: "s2", text: "after" });
  run("c", "bash", true);
  const line = d.querySelector(".turn-activity");
  assert.equal(d.querySelectorAll(".turn-activity").length, 1);
  assert.match(line.textContent, /^Activity · 3 actions · read 1 file · ran 2 commands · 1 failed$/);
  assert.equal(line.querySelector(".turn-activity-failed").textContent.trim(), "· 1 failed");
  const root = d.querySelector(".message.cognition");
  assert.equal(root.dataset.activity, "closed");
  assert.equal(line.getAttribute("aria-expanded"), "false");
  line.click();
  assert.equal(root.dataset.activity, "open");
  assert.equal(line.getAttribute("aria-expanded"), "true");
  assert.equal(d.querySelectorAll(".tool-group").length, 3, "the per-step groups are still there, shown when expanded");
  dom.window.close();
});

// --- Agent picker -------------------------------------------------------------------------------------
const agentsMessage = (current = "build") => ({ type: "assignments", agents: [
  { id: "build", description: "Writes and edits code", mode: "primary", effective: null },
  { id: "plan", description: "Plans without editing", mode: "primary", effective: null },
  { id: "lux", description: "", mode: "primary", effective: null },
  { id: "explorer", description: "Maps a codebase", mode: "primary", effective: null },
  { id: "orchestrator", description: "Splits work across agents", mode: "primary", effective: null },
  { id: "helper", description: "hidden", mode: "subagent", effective: null },
], layers: [], current, model: "gw/p/claude-x", variant: "high" });
function agentPage() {
  const ctx = page();
  ctx.tabs(["t1"], "t1");
  ctx.tab("t1", { type: "conversationOpened", id: "S", title: "S" });
  ctx.tab("t1", { type: "status", state: "ready" });
  ctx.tab("t1", agentsMessage());
  ctx.view = ctx.d.querySelector(".tab-view");
  ctx.key = (el, key) => el.dispatchEvent(new ctx.dom.window.KeyboardEvent("keydown", { key, bubbles: true, cancelable: true }));
  return ctx;
}

test("agent picker: the toolbar shows model / variant then the agent button; the popover lists every primary agent", () => {
  const { dom, view } = agentPage();
  const buttons = [...view.querySelectorAll(".toolbar > button, .toolbar > .spacer")].map(b => b.className.split(" ")[0]);
  assert.ok(buttons.indexOf("model-btn") < buttons.indexOf("agent-btn"));
  assert.ok(buttons.indexOf("attach-btn") < buttons.indexOf("spacer"), "attach stays on the left");
  assert.equal(view.querySelector(".model-btn .model-name").textContent, "claude-x / high");
  assert.equal(view.querySelector(".agent-btn .agent-name").textContent, "build");
  assert.equal(view.querySelector(".agent-btn .agent-dot").dataset.crew, "nav");
  assert.equal(view.querySelector(".agent-popover").hidden, true);
  view.querySelector(".agent-btn").click();
  assert.equal(view.querySelector(".agent-popover").hidden, false);
  assert.equal(view.querySelector(".agent-btn").getAttribute("aria-expanded"), "true");
  assert.equal(dom.window.document.activeElement, view.querySelector(".agent-search"));
  const rows = [...view.querySelectorAll(".agent-row")];
  assert.deepEqual(rows.map(r => r.dataset.id), ["build", "plan", "lux", "explorer", "orchestrator"], "all primary agents, no subagent");
  assert.equal(rows[0].querySelector(".agent-check").textContent, "✓");
  assert.equal(rows[1].querySelector(".agent-check"), null);
  assert.equal(rows[0].querySelector(".agent-row-desc").textContent, "Writes and edits code");
  assert.equal(rows[2].querySelector(".agent-row-desc"), null, "no description, no second line");
  assert.equal(rows[2].querySelector(".agent-dot").dataset.crew, "lux");
  assert.equal(rows[4].querySelector(".agent-dot").dataset.crew, undefined, "an agent without a crew gets a neutral dot");
  dom.window.close();
});

test("agent picker: search filters, arrows move, Enter picks and Esc or an outside click closes", () => {
  const { dom, d, view, key, posts } = agentPage();
  const search = view.querySelector(".agent-search");
  view.querySelector(".agent-btn").click();
  search.value = "expl"; search.dispatchEvent(new dom.window.Event("input"));
  assert.deepEqual([...view.querySelectorAll(".agent-row")].map(r => r.dataset.id), ["explorer"]);
  search.value = "splits"; search.dispatchEvent(new dom.window.Event("input"));
  assert.deepEqual([...view.querySelectorAll(".agent-row")].map(r => r.dataset.id), ["orchestrator"], "descriptions are searched too");
  search.value = "zzz"; search.dispatchEvent(new dom.window.Event("input"));
  assert.match(view.querySelector(".agent-empty").textContent, /No matching/);
  search.value = ""; search.dispatchEvent(new dom.window.Event("input"));
  key(search, "ArrowDown"); key(search, "ArrowDown");
  const selected = () => view.querySelector('.agent-row[aria-selected="true"]').dataset.id;
  assert.equal(selected(), "lux");
  assert.equal(search.getAttribute("aria-activedescendant"), view.querySelector('.agent-row[aria-selected="true"]').id);
  key(search, "ArrowUp"); assert.equal(selected(), "plan");
  key(search, "Enter");
  assert.deepEqual(posts.at(-1), { type: "selectAgent", id: "plan", tab: "t1", generation: 1 });
  assert.equal(view.querySelector(".agent-popover").hidden, true, "picking closes it");
  // Picking the current agent sends nothing.
  const before = posts.length;
  view.querySelector(".agent-btn").click(); key(search, "Enter");
  assert.equal(posts.length, before);
  // Esc closes; so does a click elsewhere.
  view.querySelector(".agent-btn").click(); key(search, "Escape");
  assert.equal(view.querySelector(".agent-popover").hidden, true);
  view.querySelector(".agent-btn").click();
  d.querySelector(".transcript").dispatchEvent(new dom.window.MouseEvent("click", { bubbles: true }));
  assert.equal(view.querySelector(".agent-popover").hidden, true);
  dom.window.close();
});

test("agent picker: the star asks the host to toggle a favourite, and favourites are listed first", () => {
  const { dom, send, view, posts } = agentPage();
  view.querySelector(".agent-btn").click();
  view.querySelector('.agent-row[data-id="lux"] .agent-star').click();
  assert.deepEqual(posts.at(-1), { type: "toggleFavoriteAgent", name: "lux", tab: "t1", generation: 1 });
  assert.equal(view.querySelector(".agent-popover").hidden, false, "starring keeps the popover open");
  send({ type: "favoriteAgents", items: ["orchestrator", "lux"] });
  assert.deepEqual([...view.querySelectorAll(".agent-row")].map(r => r.dataset.id), ["lux", "orchestrator", "build", "plan", "explorer"]);
  assert.equal(view.querySelector('.agent-row[data-id="lux"] .agent-star').textContent, "★");
  assert.equal(view.querySelector('.agent-row[data-id="build"] .agent-star').getAttribute("aria-pressed"), "false");
  dom.window.close();
});

test("image thumbnails: each button is named after the image and the open-beside action", () => {
  const { dom, d, posts, tab, tabs } = page();
  tabs(["t1"], "t1");
  tab("t1", { type: "conversationOpened", id: "S", title: "S" });
  tab("t1", { type: "message", id: "m", author: "captain", text: "look", images: [{ attachment_id: "a1", name: "shot.png", width: 640, height: 480 }, { attachment_id: "a2", name: "" }] });
  const thumbs = [...d.querySelectorAll(".thumb")];
  assert.deepEqual(thumbs.map(b => b.tagName), ["BUTTON", "BUTTON"]);
  assert.deepEqual(thumbs.map(b => b.getAttribute("aria-label")), ["Open shot.png beside", "Open image beside"]);
  thumbs[0].click();
  assert.deepEqual(posts.at(-1), { type: "openImage", attachmentId: "a1", name: "shot.png", tab: "t1", generation: 1 });
  dom.window.close();
});

test("agent picker: Ctrl+Alt+F toggles the highlighted agent's favorite from the keyboard and is announced", () => {
  const { dom, send, view, posts, key } = agentPage();
  view.querySelector(".agent-btn").click();
  const search = view.querySelector(".agent-search");
  assert.equal(search.getAttribute("aria-keyshortcuts"), "Control+Alt+F");
  const hint = dom.window.document.getElementById(search.getAttribute("aria-describedby"));
  assert(hint, "the search box is described by the shortcut hint");
  assert.match(hint.textContent, /Ctrl\+Alt\+F adds or removes the highlighted agent from favorites/);
  assert.match(search.placeholder, /Ctrl\+Alt\+F favorite/);
  assert.doesNotMatch(hint.textContent + search.placeholder, /Ctrl\+F/);
  assert.equal(search.getAttribute("aria-controls"), view.querySelector(".agent-list").id, "the combobox names the listbox it drives");
  // `propagated` tells whether the event bubbled out of the search box to the document.
  const press = (extra = {}) => {
    const e = new dom.window.KeyboardEvent("keydown", { key: "f", code: "KeyF", ctrlKey: true, altKey: true, bubbles: true, cancelable: true, ...extra });
    let propagated = false; const seen = () => { propagated = true; };
    dom.window.document.addEventListener("keydown", seen); search.dispatchEvent(e); dom.window.document.removeEventListener("keydown", seen);
    e.propagated = propagated; return e;
  };
  key(search, "ArrowDown");
  const before = posts.length;
  const e = press();
  assert.equal(e.defaultPrevented, true);
  assert.equal(e.propagated, false, "the shortcut does not bubble out of the search box");
  assert.equal(posts.length, before + 1, "one toggle per press");
  assert.deepEqual(posts.at(-1), { type: "toggleFavoriteAgent", name: "plan", tab: "t1", generation: 1 });
  assert.equal(view.querySelector(".agent-popover").hidden, false, "the popover stays open");
  send({ type: "favoriteAgents", items: ["plan"] });
  assert.equal(view.querySelector('.agent-row[aria-selected="true"]').dataset.id, "plan", "the highlight follows the agent to its new place");
  assert.equal(dom.window.document.activeElement, search, "focus stays in the search box");
  // Removing it again goes through the same shortcut, on the same highlighted agent.
  const added = posts.length;
  press();
  assert.equal(posts.length, added + 1, "one toggle for the removal");
  assert.deepEqual(posts.at(-1), { type: "toggleFavoriteAgent", name: "plan", tab: "t1", generation: 1 });
  send({ type: "favoriteAgents", items: [] });
  assert.equal(view.querySelector('.agent-row[aria-selected="true"]').dataset.id, "plan");
  assert.equal(dom.window.document.activeElement, search);
  // Plain Ctrl+F is VS Code's find, Alt+F was reported as a host key, Ctrl+Alt+A belongs to the model picker, AltGr and Shift/Meta
  // variants are typing or host keys, and a composition is the IME's: none acts, none is blocked.
  const n = posts.length;
  for (const extra of [{ altKey: false }, { ctrlKey: false }, { key: "a", code: "KeyA" }, { key: "f", modifierAltGraph: true }, { shiftKey: true }, { metaKey: true }, { isComposing: true }, { key: "Dead" }]) {
    const ignored = press(extra);
    assert.equal(ignored.defaultPrevented, false, JSON.stringify(extra));
    assert.equal(ignored.propagated, true, JSON.stringify(extra));
  }
  // An empty list sends nothing.
  search.value = "zzz"; search.dispatchEvent(new dom.window.Event("input"));
  press();
  assert.equal(posts.length, n);
  dom.window.close();
});

test("agent picker: the star still toggles a favorite with the mouse", () => {
  const { dom, view, posts } = agentPage();
  view.querySelector(".agent-btn").click();
  const before = posts.length;
  view.querySelector('.agent-row[data-id="plan"] .agent-star').click();
  assert.equal(posts.length, before + 1);
  assert.deepEqual(posts.at(-1), { type: "toggleFavoriteAgent", name: "plan", tab: "t1", generation: 1 });
  assert.equal(view.querySelector(".agent-popover").hidden, false, "the popover stays open");
  dom.window.close();
});

test("agent picker: disabled while the view is locked, and unusable in a read-only child view", () => {
  const { dom, tab, view } = agentPage();
  assert.equal(view.querySelector(".agent-btn").disabled, false);
  tab("t1", { ...agentsMessage(), locked: true });
  assert.equal(view.querySelector(".agent-btn").disabled, true);
  view.querySelector(".agent-btn").click();
  assert.equal(view.querySelector(".agent-popover").hidden, true);
  dom.window.close();
});

test("host: favourite agents are kept under their own key, validated like the models, and sent to the webview", async () => {
  const values = new Map();
  const state = { get: (key, fallback) => (values.has(key) ? values.get(key) : fallback), update: async (key, value) => { values.set(key, value); } };
  const { hub } = makeHub();
  registerChatView({ extensionUri: "extension", globalState: state }, hub);
  const messages = []; let receive;
  provider.resolveWebviewView({ webview: { cspSource: "t", asWebviewUri: v => v, postMessage(m) { messages.push(m); }, onDidReceiveMessage(l) { receive = l; return noop(); } }, onDidDispose() {} });
  receive({ type: "ready" });
  assert.deepEqual(messages.find(m => m.type === "favoriteAgents").items, []);
  receive({ type: "toggleFavoriteAgent", name: "lux" });
  receive({ type: "toggleFavoriteAgent", name: "plan" });
  receive({ type: "toggleFavoriteAgent", name: 7 });
  receive({ type: "toggleFavoriteAgent", name: "" });
  receive({ type: "toggleFavoriteAgent", name: "x".repeat(301) });
  receive({ type: "toggleFavorite", name: "some/model" });
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(values.get("withnav.favoriteAgents"), ["lux", "plan"]);
  assert.deepEqual(values.get("withnav.favoriteModels"), ["some/model"], "models keep their own list");
  assert.deepEqual(messages.filter(m => m.type === "favoriteAgents").at(-1).items, ["lux", "plan"]);
  receive({ type: "toggleFavoriteAgent", name: "lux" });
  assert.deepEqual(values.get("withnav.favoriteAgents"), ["plan"], "a second toggle removes it");
  values.set("withnav.favoriteAgents", "not a list");
  receive({ type: "ready" });
  assert.deepEqual(messages.filter(m => m.type === "favoriteAgents").at(-1).items, [], "a corrupt value reads as empty");
});

test("agent picker helpers: crews and ordering", () => {
  const { crewOf, orderAgents } = require("../out/tests/webview/agentStrip.js");
  assert.equal(crewOf("build"), "nav"); assert.equal(crewOf("review"), "lux"); assert.equal(crewOf("orchestrator"), undefined);
  const items = ["a", "b", "c"].map(id => ({ id, description: id === "c" ? "searchable" : "" }));
  assert.deepEqual(orderAgents(items, "", new Set(["c"])).map(i => i.id), ["c", "a", "b"]);
  assert.deepEqual(orderAgents(items, "search", new Set()).map(i => i.id), ["c"]);
});

test("mcp app: tool row renders inline media preview and Open App button posts openMcpApp", () => {
  const { dom, posts, tab, view } = agentPage();
  tab("t1", { type: "turnStart", id: "turn1", generation: 1 });
  tab("t1", {
    type: "tool",
    id: "turn1",
    callId: "c1",
    phase: "end",
    name: "mcp__excalidraw__get_canvas_screenshot",
    summary: "mcp__excalidraw__get_canvas_screenshot",
    ok: true,
    mcpMedia: [{ mime_type: "image/png", data: "AAAA" }],
    mcpUi: { server: "excalidraw", tool: "get_canvas_screenshot", resource_uri: "ui://excalidraw/app.html", url: "http://127.0.0.1:3000" },
    args: { scale: 2 },
    output: "captured",
    generation: 1,
  });
  const img = view.querySelector(".mcp-media-card img");
  assert(img, "inline image preview is rendered");
  assert.equal(img.getAttribute("src"), "data:image/png;base64,AAAA");
  const zoom = view.querySelector(".mcp-media-card");
  const style = dom.window.document.createElement("style"); style.textContent = fs.readFileSync("out/chat.css", "utf8"); dom.window.document.head.append(style);
  assert.equal(dom.window.getComputedStyle(view.querySelector(".mcp-media-grid")).overflow, "visible", "the grid does not clip the button's focus outline");
  assert.equal(zoom.tagName, "BUTTON", "the zoom is a native button, so Tab, Enter and Space reach it");
  assert.equal(zoom.type, "button");
  assert.equal(zoom.getAttribute("aria-label"), "Zoom mcp__excalidraw__get_canvas_screenshot preview 1");
  assert.equal(zoom.getAttribute("aria-pressed"), "false");
  img.click();
  assert(zoom.classList.contains("zoomed"), "clicking toggles zoom");
  assert.equal(zoom.getAttribute("aria-pressed"), "true");
  zoom.click();
  assert.equal(zoom.classList.contains("zoomed"), false);
  assert.equal(zoom.getAttribute("aria-pressed"), "false");
  const openBtn = view.querySelector(".mcp-open-btn");
  assert(openBtn, "Open App button is rendered");
  openBtn.click();
  const posted = posts.find(p => p.type === "openMcpApp");
  assert(posted, "clicking Open App posts openMcpApp");
  assert.equal(posted.app.server, "excalidraw");
  assert.equal(posted.app.resourceUri, "ui://excalidraw/app.html");
  assert.equal(posted.app.url, "http://127.0.0.1:3000");
  // The app's document is served by WithNav, never inlined into the panel.
  const frameUrl = "http://127.0.0.1:4321/mcp-app/abc";
  const html = renderMcpAppHtml("nonce1", { ...posted.app, html: "<script>alert(1)</script>" }, frameUrl);
  assert.match(html, /<iframe id="mcp-frame" sandbox="allow-scripts allow-forms" src="http:\/\/127\.0\.0\.1:4321\/mcp-app\/abc">/);
  assert.doesNotMatch(html, /srcdoc|alert\(1\)/, "resource HTML never reaches the panel document");
  const csp = html.match(/Content-Security-Policy" content="([^"]*)"/)[1];
  assert.match(csp, /frame-src http:\/\/127\.0\.0\.1:4321;/, "the panel may embed only the app's own origin");
  assert.match(csp, /script-src 'nonce-nonce1';/, "the host script is the only script the panel runs");
  assert.doesNotMatch(csp, /unsafe-inline'?;?\s*$|script-src[^;]*unsafe-inline/);
  const failed = renderMcpAppHtml("nonce1", { ...posted.app, url: undefined }, undefined, "no app");
  assert.doesNotMatch(failed, /<iframe/);
  assert.match(failed, /frame-src 'none'/);
  dom.window.close();
});

// --- Keyboard and accessibility structure of the tab strip and the session list -------------------------
const press = (dom, target, key, init = {}) => {
  const event = new dom.window.KeyboardEvent("keydown", { key, bubbles: true, cancelable: true, ...init });
  target.dispatchEvent(event);
  return event;
};

test("tab strip: the tab is the focusable element, with a roving tabindex and a mouse-only close button", () => {
  const { dom, d, tabs, tab } = page();
  tabs(["a", "b", "c"], "b");
  for (const id of ["a", "b", "c"]) tab(id, { type: "conversationOpened", id: `S-${id}`, title: id });
  const strip = d.querySelector('[role="tablist"]');
  const buttons = [...strip.querySelectorAll(".tab-main")];
  assert.deepEqual(buttons.map(b => b.getAttribute("role")), ["tab", "tab", "tab"]);
  assert.deepEqual(buttons.map(b => b.getAttribute("aria-selected")), ["false", "true", "false"]);
  assert.deepEqual(buttons.map(b => b.tabIndex), [-1, 0, -1], "only the selected tab is in the Tab order");
  assert.deepEqual([...strip.querySelectorAll(".tab")].map(w => w.getAttribute("role")), ["presentation", "presentation", "presentation"]);
  assert.ok(buttons.every(b => b.closest('[role="tab"]') === b), "role=tab sits on the focusable button, not a wrapper");
  assert.deepEqual([...strip.querySelectorAll(".tab-close")].map(c => [c.tabIndex, c.getAttribute("aria-hidden")]), [[-1, "true"], [-1, "true"], [-1, "true"]]);
  assert.deepEqual(buttons.map(b => d.getElementById(b.getAttribute("aria-controls")).getAttribute("aria-labelledby")), buttons.map(b => b.id));
  assert.equal(d.getElementById(buttons[1].getAttribute("aria-controls")).getAttribute("role"), "tabpanel");
  dom.window.close();
});

test("tab strip: arrows wrap, Home and End jump, focus moves without activating, Enter activates and keeps focus on the tab", () => {
  const { dom, d, tabs, tab, posts } = page();
  tabs(["a", "b", "c"], "a");
  for (const id of ["a", "b", "c"]) tab(id, { type: "conversationOpened", id: `S-${id}`, title: id });
  const main = id => d.querySelector(`.tab[data-tab="${id}"] .tab-main`);
  main("a").focus();
  const before = posts.length;
  assert.equal(press(dom, main("a"), "ArrowRight").defaultPrevented, true);
  assert.equal(d.activeElement, main("b"));
  assert.deepEqual([...d.querySelectorAll(".tab-main")].map(b => b.tabIndex), [-1, 0, -1], "tabindex follows focus");
  press(dom, main("b"), "End"); assert.equal(d.activeElement, main("c"));
  press(dom, main("c"), "ArrowRight"); assert.equal(d.activeElement, main("a"), "wraps to the first");
  press(dom, main("a"), "ArrowLeft"); assert.equal(d.activeElement, main("c"), "wraps to the last");
  press(dom, main("c"), "Home"); assert.equal(d.activeElement, main("a"));
  assert.equal(posts.length, before, "moving focus activates and closes nothing");
  assert.equal(d.querySelector('.tab[data-tab="a"] .tab-main').getAttribute("aria-selected"), "true");
  // Keyboard activation (a click with no pointer) shows the tab and leaves focus on it.
  press(dom, main("a"), "ArrowRight");
  main("b").click();
  assert.deepEqual(posts.at(-1), { type: "focusTab", tab: "b" });
  assert.equal(main("b").getAttribute("aria-selected"), "true");
  assert.equal(d.activeElement, main("b"), "focus stays on the tab, not the composer");
  // A re-render caused by another tab's status keeps focus on the same tab.
  press(dom, main("b"), "ArrowRight");
  tab("a", { type: "turnStart", id: "T" });
  assert.equal(d.activeElement, main("c"));
  assert.deepEqual([...d.querySelectorAll(".tab-main")].map(b => b.tabIndex), [-1, -1, 0], "exactly one tab stop");
  // Unrelated keys are left alone.
  assert.equal(press(dom, main("c"), "a").defaultPrevented, false);
  dom.window.close();
});

test("tab strip: Delete asks the host to close the focused tab and focus goes to a neighbour, not the composer", () => {
  const { dom, d, tabs, tab, posts } = page();
  tabs(["a", "b", "c"], "b");
  for (const id of ["a", "b", "c"]) tab(id, { type: "conversationOpened", id: `S-${id}`, title: id });
  const main = id => d.querySelector(`.tab[data-tab="${id}"] .tab-main`);
  main("b").focus();
  press(dom, main("b"), "Delete");
  assert.deepEqual(posts.at(-1), { type: "closeTab", tab: "b" });
  tabs(["a", "c"], "c");
  assert.equal(d.activeElement, main("c"), "the next tab takes focus");
  press(dom, main("c"), "Delete");
  assert.deepEqual(posts.at(-1), { type: "closeTab", tab: "c" });
  tabs(["a"], "a");
  assert.equal(d.activeElement, main("a"), "the previous one when the last tab was closed");
  dom.window.close();
});

test("session list: options hold no focusable control, both listboxes are controlled, and the keys act on the selected row", () => {
  const { dom, d, send, tabs } = page();
  tabs([], undefined);
  const now = new Date().toISOString();
  send({ type: "conversations", items: [
    { id: "S-1", title: "One", updatedAt: now, messageCount: 1, live: true },
    { id: "S-2", title: "Two", updatedAt: now, messageCount: 2 },
  ] });
  const search = d.querySelector(".sl-search");
  assert.deepEqual(search.getAttribute("aria-controls").split(" ").map(id => d.getElementById(id)?.getAttribute("role")), ["listbox", "listbox"]);
  assert.equal(search.getAttribute("aria-expanded"), "true");
  for (const option of d.querySelectorAll('.sl-list [role="option"]')) {
    const focusable = [...option.querySelectorAll("button, a, input, [tabindex]")].filter(e => e.tabIndex >= 0);
    assert.deepEqual(focusable, [], "nothing in an option is in the Tab order");
    assert.match(option.getAttribute("aria-label"), /, (Active|Done), /);
    const del = option.querySelector(".sl-delete");
    assert.equal(del.tabIndex, -1); assert.equal(del.getAttribute("aria-hidden"), "true");
    assert.deepEqual(JSON.parse(option.dataset.vscodeContext), { webviewSection: "session", sessionId: option.dataset.id, preventDefaultContextMenuItems: true });
  }
  assert.equal(d.querySelectorAll("button.sl-open").length, 0);
  assert.equal(search.getAttribute("aria-activedescendant"), "session-option-0");
  press(dom, search, "ArrowDown");
  assert.equal(search.getAttribute("aria-activedescendant"), "session-option-1");
  // Enter opens the selected row.
  const clicked = [];
  d.querySelector('.sl-row[data-id="S-2"]').addEventListener("click", () => clicked.push("S-2"));
  press(dom, search, "Enter");
  assert.deepEqual(clicked, ["S-2"]);
  dom.window.close();
});

test("session list: Delete removes the selected row only when it has no text to edit, and a collapsed section is skipped", () => {
  const { dom, d, send, tabs, posts } = page();
  tabs(["t1"], "t1");
  const now = new Date().toISOString();
  send({ type: "conversations", workspace: "w", items: [
    { id: "S-1", title: "One", updatedAt: now, messageCount: 1 },
    { id: "S-2", title: "Two", updatedAt: now, messageCount: 2 },
  ] });
  d.querySelector(".sh-back").click();
  const search = d.querySelector(".sl-search");
  press(dom, search, "ArrowDown");
  const empty = press(dom, search, "Delete");
  assert.equal(empty.defaultPrevented, true);
  const [first, second] = [...d.querySelectorAll(".sl-row")].map(r => r.dataset.id);
  assert.deepEqual(posts.at(-1), { type: "deleteConversation", id: second });
  // With text, Delete at the end still has nothing to edit; in the middle it edits.
  search.value = "On"; search.dispatchEvent(new dom.window.Event("input"));
  search.setSelectionRange(0, 0);
  assert.equal(press(dom, search, "Delete").defaultPrevented, false, "the caret has text after it: Delete edits the field");
  posts.length = 0;
  search.setSelectionRange(2, 2);
  press(dom, search, "Delete");
  assert.deepEqual(posts.at(-1), { type: "deleteConversation", id: first });
  // Collapsing the group removes its rows from the keyboard model: no hidden row is selected, opened or deleted.
  search.value = ""; search.dispatchEvent(new dom.window.Event("input"));
  d.querySelector(".sl-group-toggle").click();
  assert.equal(d.querySelectorAll(".sl-row").length, 0);
  assert.equal(search.hasAttribute("aria-activedescendant"), false);
  assert.equal(d.querySelector(".sl-note").hidden, true, "a collapsed list is not announced as empty");
  posts.length = 0;
  press(dom, search, "ArrowDown"); press(dom, search, "Enter"); press(dom, search, "Delete");
  assert.deepEqual(posts, []);
  d.querySelector(".sl-group-toggle").click();
  assert.equal(d.querySelectorAll(".sl-row").length, 2);
  assert.equal(search.getAttribute("aria-activedescendant"), "session-option-0");
  dom.window.close();
});

test("tab strip: a keyboard activation never lingers, and a tab activated elsewhere takes focus to the composer", () => {
  const { dom, d, tabs, tab } = page();
  tabs(["a", "b"], "a");
  for (const id of ["a", "b"]) tab(id, { type: "conversationOpened", id: `S-${id}`, title: id });
  const main = id => d.querySelector(`.tab[data-tab="${id}"] .tab-main`);
  // Enter on the already selected tab changes nothing to rebuild; the next status update must not pull focus back to it.
  main("a").focus();
  main("a").click();
  assert.equal(d.activeElement, main("a"));
  press(dom, main("a"), "ArrowRight");
  tab("a", { type: "turnStart", id: "T" });
  assert.equal(d.activeElement, main("b"), "focus stays where the arrow put it");
  d.querySelector(".prompt").focus();
  tab("a", { type: "turnEnd", id: "T" }); tab("b", { type: "status", state: "ready" });
  assert.equal(d.activeElement, d.querySelector(".tab-view:not([hidden]) .prompt"), "leaving the strip is respected");
  // The host activates another tab while focus sits on a tab: the composer of that tab gets it.
  tab("b", { type: "status", state: "ready" });
  main("a").focus();
  tabs(["a", "b"], "b");
  assert.equal(d.activeElement, d.querySelector(".tab-view:not([hidden]) .prompt"));
  dom.window.close();
});
