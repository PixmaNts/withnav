const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const { JSDOM } = require("jsdom");
const { parseSettingsMessage, isModified, appliesText } = require("../out/tests/settingsProtocol.js");
const { SettingsHost } = require("../out/tests/settingsHost.js");
const { settingsHtml } = require("../out/tests/settingsHtml.js");
const { buildSections, matches } = require("../out/tests/webview/settingsModel.js");

// A small catalogue and config in the shapes of GET /v1/settings, /v1/models, /v1/agents, /v1/layers.
const field = (key, value, def, set, applies = "new_sessions") => ({ key, value, default: def, set, error: null, applies });
function settingsBody(revision = "rev-1") {
  return {
    revision,
    paths: { config: "/home/u/.config/navai/config.json", models: "/home/u/.config/navai/models.json", agents: ["/home/u/.config/navai/agents", "/ws/.navai/agents"] },
    fields: [
      field("layers.reflex", { model: "inception/mercury-edit-2", variant: null }, null, true, "restart"),
      field("layers.instinct", { model: "omniroute/typesafe/jev-1.13", variant: null }, null, true, "restart"),
      field("layers.metacognition", { model: "omniroute/combo/flash", variant: "low" }, null, true, "restart"),
      field("default_agent", "build", "build", true),
      field("permissions.automatic_review", null, true, false),
      field("permissions.scratch_dirs", null, ["/run/user/1000/navai", "/tmp/navai"], false),
      field("subagents.max_concurrent", 5, 4, true),
      field("subagents.watchdog_interval_secs", null, 300, false),
      field("agent.watchdog_interval_secs", null, 300, false),
      field("tui.theme", "nord-light", null, true, "live"),
      field("agents.build", { model: "omniroute/cc/opus", variant: "high" }, null, true),
      field("agents.explore", null, null, false),
    ],
    gateways: [
      { name: "omniroute", base_url: "http://localhost:20128/v1", api_key_env: "OMNIROUTE_API_KEY", key_source: "auth_store", catalog_format: "omniroute", connect_timeout_ms: 5000, request_timeout_ms: 600000, headers: [], model_count: 4, used_by: ["layers.instinct", "agents.build"] },
      { name: "inception", base_url: "https://api.inceptionlabs.ai/v1", api_key_env: "INCEPTION_API_KEY", key_source: "missing", catalog_format: null, connect_timeout_ms: 5000, request_timeout_ms: 600000, headers: ["X-Title"], model_count: 1, used_by: ["layers.reflex"] },
    ],
  };
}
const modelsBody = { models: [
  { name: "inception/mercury-edit-2", gateway: "inception", provider_group: "inception", upstream_id: "mercury-edit-2", api: "fim", context_window: 128000 },
  { name: "omniroute/typesafe/jev-1.13", gateway: "omniroute", provider_group: "typesafe", upstream_id: "typesafe/jev-1.13", api: "systemone", context_window: 64000 },
  { name: "omniroute/combo/flash", gateway: "omniroute", provider_group: "combo", upstream_id: "combo/flash", api: "chat", context_window: 128000, variants: ["low", "high"] },
  { name: "omniroute/cc/opus", gateway: "omniroute", provider_group: "cc", upstream_id: "cc/opus", api: "chat", context_window: 1000000, reported_output: 128000, variants: ["high", "max"] },
] };
const agentsBody = { default_agent: "build", agents: [
  { id: "build", description: "Implements the task", mode: "primary", effective: { model: "omniroute/cc/opus", variant: "high", source: "config" }, warning: null },
  { id: "explore", description: "Read-only exploration", mode: "subagent", effective: null, warning: null },
] };
const layersBody = { layers: [{ layer: "reflex", status: "usable", model: "inception/mercury-edit-2", variant: null, reason: null }] };

const json = (body, status = 200) => new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

/** A fake session server: GETs answer the fixtures, PUT answers what `put` returns. */
function fakeServer(put) {
  const calls = [];
  let current = settingsBody();
  return {
    calls,
    set current(v) { current = v; },
    transport: {
      async request(path, init = {}) {
        calls.push({ path, method: init.method ?? "GET", body: init.body ? JSON.parse(init.body) : undefined });
        const p = path.split("?")[0];
        if (p === "/v1/settings" && init.method === "PUT") return put(JSON.parse(init.body), calls);
        if (p === "/v1/settings") return json(current);
        if (p === "/v1/models") return json(modelsBody);
        if (p === "/v1/agents") return json(agentsBody);
        if (p === "/v1/layers") return json(layersBody);
        if (p.startsWith("/v1/gateways/")) return json({ ok: true, status: 200, latency_ms: 42, model_count: 4, error: null });
        return json({ error: "not found" }, 404);
      },
    },
  };
}

test("webview messages are checked at the host boundary", () => {
  for (const bad of [null, 1, {}, { type: "save", key: 42, value: true }, { type: "save", key: "x" }, { type: "save", key: "", value: 1 },
    { type: "open", target: "/etc/passwd" }, { type: "testGateway", name: 3 }, { type: "openAgent" }, { type: "fetch", url: "http://x" }]) {
    assert.equal(parseSettingsMessage(bad), null, JSON.stringify(bad));
  }
  assert.deepEqual(parseSettingsMessage({ type: "save", key: "tui.theme", value: null, extra: 1 }), { type: "save", key: "tui.theme", value: null });
  assert.deepEqual(parseSettingsMessage({ type: "open", target: "config", path: "/etc/passwd" }), { type: "open", target: "config" });
});

test("modified means config.json holds a value other than the default", () => {
  assert.equal(isModified({ set: false, value: null, default: 4 }), false);
  assert.equal(isModified({ set: true, value: 4, default: 4 }), false);
  assert.equal(isModified({ set: true, value: 5, default: 4 }), true);
  assert.equal(isModified({ set: true, value: ["/a"], default: ["/a"] }), false);
  assert.match(appliesText("restart"), /after restarting Navai/);
});

test("load reads settings, catalogue, agents and layers for the workspace, never exposing the token", async () => {
  const server = fakeServer(() => json({}));
  const posted = [];
  const host = new SettingsHost(server.transport, (m) => posted.push(m), { workspace: "/ws dir", findDefinition: (_dirs, id) => (id === "build" ? "/ws/.navai/agents/build.md" : undefined) });
  await host.handle({ type: "ready" });
  assert.ok(server.calls.every((c) => c.path.endsWith("?workspace=%2Fws%20dir")));
  assert.deepEqual(server.calls.map((c) => c.path.split("?")[0]).sort(), ["/v1/agents", "/v1/layers", "/v1/models", "/v1/settings"]);
  const snapshot = posted.find((m) => m.type === "snapshot").data;
  assert.equal(snapshot.models.length, 4);
  assert.equal(snapshot.agents[0].definition, "/ws/.navai/agents/build.md");
  assert.doesNotMatch(JSON.stringify(posted), /token|authorization/i);
});

test("a save sends the last revision and posts the new body; an unloaded page refuses", async () => {
  const server = fakeServer((body) => { const next = settingsBody("rev-2"); next.fields.find((f) => f.key === body.key).value = body.value; return json(next); });
  const posted = [];
  const host = new SettingsHost(server.transport, (m) => posted.push(m), { workspace: "/ws" });
  await host.handle({ type: "save", key: "subagents.max_concurrent", value: 8 });
  assert.equal(posted.at(-1).type, "saveFailed");
  await host.handle({ type: "ready" });
  posted.length = 0;
  await host.handle({ type: "save", key: "subagents.max_concurrent", value: 8 });
  const put = server.calls.find((c) => c.method === "PUT");
  assert.deepEqual(put.body, { revision: "rev-1", key: "subagents.max_concurrent", value: 8 });
  assert.deepEqual(posted.map((m) => m.type), ["saving", "snapshot", "snapshot", "snapshot", "saved"]);
  assert.equal(host.current().settings.revision, "rev-2");
});

test("409 reloads config.json, says so, and drops the saves queued behind it", async () => {
  let puts = 0;
  const server = fakeServer(() => { puts += 1; return json({ error: "config.json changed since it was read", reason: "stale", revision: "rev-9" }, 409); });
  const posted = [];
  const host = new SettingsHost(server.transport, (m) => posted.push(m), { workspace: "/ws" });
  await host.handle({ type: "ready" });
  server.current = settingsBody("rev-9");
  posted.length = 0;
  const first = host.handle({ type: "save", key: "tui.theme", value: "ayu-dark" });
  const second = host.handle({ type: "save", key: "subagents.max_concurrent", value: 9 });
  await Promise.all([first, second]);
  assert.equal(puts, 1, "the queued save must not be replayed over a file the user has not seen");
  const stale = posted.find((m) => m.type === "stale");
  assert.match(stale.message, /changed elsewhere/);
  assert.equal(host.current().settings.revision, "rev-9");
});

test("400 stays attached to the key the server names; other failures to the saved key", async () => {
  let answer = () => json({ error: "scratch dir must be absolute: tmp", reason: "invalid", key: "permissions.scratch_dirs" }, 400);
  const server = fakeServer(() => answer());
  const posted = [];
  const host = new SettingsHost(server.transport, (m) => posted.push(m), { workspace: "/ws" });
  await host.handle({ type: "ready" });
  await host.handle({ type: "save", key: "permissions.scratch_dirs", value: ["tmp"] });
  assert.deepEqual(posted.at(-1), { type: "invalid", key: "permissions.scratch_dirs", message: "scratch dir must be absolute: tmp" });
  answer = () => json({ error: "disk full" }, 500);
  await host.handle({ type: "save", key: "tui.theme", value: "ayu-dark" });
  assert.equal(posted.at(-1).type, "saveFailed");
  assert.match(posted.at(-1).message, /disk full/);
});

test("connection test only targets a known gateway and reports the result", async () => {
  const server = fakeServer(() => json({}));
  const posted = [];
  const host = new SettingsHost(server.transport, (m) => posted.push(m), { workspace: "/ws" });
  await host.handle({ type: "ready" });
  await host.handle({ type: "testGateway", name: "../v1/sessions" });
  assert.ok(!server.calls.some((c) => c.method === "POST"));
  await host.handle({ type: "testGateway", name: "omniroute" });
  assert.equal(server.calls.at(-1).path.split("?")[0], "/v1/gateways/omniroute/test");
  assert.deepEqual(posted.slice(-2).map((m) => m.type), ["testing", "testResult"]);
  assert.equal(posted.at(-1).result.latency_ms, 42);
});

function snapshot() {
  return {
    settings: settingsBody(), models: modelsBody.models, layers: layersBody.layers, home: "/home/u",
    agents: agentsBody.agents.map((a) => ({ ...a, definition: a.id === "build" ? "/home/u/.config/navai/agents/build.md" : undefined })),
  };
}

test("sections follow the TOC of the design and filter models by API, grouped gateway › provider", () => {
  const sections = buildSections(snapshot());
  assert.deepEqual(sections.map((s) => s.label), ["Gateways", "omniroute", "inception", "Layers", "reflex", "instinct", "metacognition", "cognition", "build", "explore", "Permissions", "Subagents", "Appearance"]);
  const reflex = sections.find((s) => s.id === "layer:reflex").rows[0];
  assert.deepEqual(reflex.control.groups.map((g) => g.label), ["inception"]);
  const build = sections.find((s) => s.id === "agent:build").rows[0];
  assert.deepEqual(build.control.groups.map((g) => [g.label, g.options.map((o) => o.value)]), [["omniroute › combo", ["omniroute/combo/flash"]], ["omniroute › cc", ["omniroute/cc/opus"]]]);
  // A model change keeps the variant only when the new model offers it.
  assert.deepEqual(build.write("omniroute/combo/flash"), { model: "omniroute/combo/flash", variant: "high" });
  assert.deepEqual(build.write("omniroute/cc/opus"), { model: "omniroute/cc/opus", variant: "high" });
  const gw = sections.find((s) => s.id === "gateway:omniroute");
  assert.ok(gw.rows.every((r) => !r.key), "gateway rows are read-only");
  assert.ok(matches(sections.find((s) => s.id === "subagents").rows[0], "max concurrent"));
  assert.ok(!matches(sections.find((s) => s.id === "subagents").rows[1], "@modified"));
});

function page() {
  const dom = new JSDOM(settingsHtml("n", "settings.js", "settings.css", "resource:"), { runScripts: "outside-only", pretendToBeVisual: true });
  const posts = [];
  dom.window.acquireVsCodeApi = () => ({ postMessage: (m) => posts.push(JSON.parse(JSON.stringify(m))), getState() {}, setState() {} });
  dom.window.eval(fs.readFileSync("out/settings.js", "utf8"));
  const d = dom.window.document;
  const receive = (data) => dom.window.dispatchEvent(new dom.window.MessageEvent("message", { data }));
  const rows = () => [...d.querySelectorAll(".row")];
  const title = (row) => row.querySelector(".row-title").textContent;
  const toc = (label) => [...d.querySelectorAll(".toc-item")].find((a) => a.textContent === label);
  return { dom, d, posts, receive, rows, title, toc };
}

test("webview renders the TOC, rows as Category › sub: Title, Modified, and the setting count", () => {
  const { dom, d, posts, receive, rows, title, toc } = page();
  assert.deepEqual(posts[0], { type: "ready" });
  receive({ type: "snapshot", data: snapshot() });
  assert.equal(d.querySelectorAll(".toc-item").length, 13);
  assert.ok(toc("omniroute").classList.contains("selected"), "Gateways opens its first gateway");
  assert.equal(title(rows()[0]), "Gateways › omniroute:Base URL");
  assert.ok(rows()[0].querySelector("input").disabled);
  assert.match(d.querySelector("#count").textContent, /^\d+ Settings$/);
  assert.equal(d.querySelector("#paths").textContent, "~/.config/navai/config.json · models.json");

  toc("Subagents").click();
  const [max, sub] = rows();
  assert.ok(max.classList.contains("modified"));
  assert.match(title(max), /^Subagents:Max ConcurrentModified/);
  assert.ok(!sub.classList.contains("modified"));
  assert.equal(sub.querySelector("input").value, "300", "an unset key shows its default");

  toc("metacognition").click();
  assert.match(rows()[0].querySelector(".row-applies").textContent, /after restarting Navai/);
  toc("build").click();
  assert.ok(toc("cognition").classList.contains("active"), "an agent keeps cognition highlighted");
  assert.equal(rows()[2].querySelector("select").disabled, true, "mode is read-only");
  dom.window.close();
});

test("webview search filters rows across sections and the TOC; Escape clears", () => {
  const { dom, d, receive, rows, title } = page();
  receive({ type: "snapshot", data: snapshot() });
  const search = d.querySelector("#search");
  search.value = "watchdog";
  search.dispatchEvent(new dom.window.Event("input"));
  assert.deepEqual(rows().map(title), ["Subagents:Watchdog Interval Secs", "Agent:Watchdog Interval Secs"]);
  assert.equal(d.querySelector("#count").textContent, "2 Settings Found");
  assert.deepEqual([...d.querySelectorAll(".toc-item")].map((a) => a.textContent), ["Subagents"]);
  search.value = "nothing-matches-this";
  search.dispatchEvent(new dom.window.Event("input"));
  assert.match(d.querySelector("#rows").textContent, /No settings found/);
  search.dispatchEvent(new dom.window.KeyboardEvent("keydown", { key: "Escape" }));
  assert.equal(search.value, "");
  assert.ok(rows().length > 2);
  d.querySelector("#filter").click();
  assert.ok(rows().every((r) => r.classList.contains("modified")), "the funnel shows modified settings only");
  dom.window.close();
});

test("webview controls post one key per change; refusals show under their row; stale shows a banner", () => {
  const { dom, d, posts, receive, rows, toc } = page();
  receive({ type: "snapshot", data: snapshot() });
  toc("Permissions").click();
  const box = rows()[0].querySelector("input[type=checkbox]");
  assert.equal(box.checked, true, "automatic review defaults to on");
  box.click();
  assert.deepEqual(posts.at(-1), { type: "save", key: "permissions.automatic_review", value: false });

  rows()[1].querySelector(".list > .button").click();
  const input = rows()[1].querySelector(".list-edit input");
  input.value = "/srv/scratch";
  rows()[1].querySelector(".list-edit .button.primary").click();
  assert.deepEqual(posts.at(-1), { type: "save", key: "permissions.scratch_dirs", value: ["/run/user/1000/navai", "/tmp/navai", "/srv/scratch"] });
  receive({ type: "invalid", key: "permissions.scratch_dirs", message: "scratch dir must be absolute" });
  assert.equal(rows()[1].querySelector(".row-error").textContent, "scratch dir must be absolute");
  assert.equal(rows()[0].querySelector(".row-error"), null);

  toc("Appearance").click();
  const reset = rows()[0].querySelector(".row-reset");
  reset.click();
  assert.deepEqual(posts.at(-1), { type: "save", key: "tui.theme", value: null });

  d.querySelector("#open-json").click();
  assert.deepEqual(posts.at(-1), { type: "open", target: "config" });
  d.querySelector("#extension-settings").click();
  assert.deepEqual(posts.at(-1), { type: "openExtensionSettings" });

  receive({ type: "stale", key: "tui.theme", message: "config.json changed elsewhere." });
  assert.match(d.querySelector(".banner").textContent, /changed elsewhere/);
  dom.window.close();
});

test("settings HTML has a strict CSP and a nonce-bound script", () => {
  const html = settingsHtml("abc", "https://h/settings.js", "https://h/settings.css", "https://h");
  assert.match(html, /default-src 'none'/);
  assert.match(html, /script-src 'nonce-abc'/);
  assert.match(html, /connect-src 'none'/);
  assert.doesNotMatch(html, /unsafe-inline/);
});
