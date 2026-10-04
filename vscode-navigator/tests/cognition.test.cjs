const { test } = require("node:test");
const assert = require("node:assert/strict");
const Module = require("node:module");
const originalLoad = Module._load;
Module._load = function (id, ...args) {
  if (id === "vscode") return { workspace: { workspaceFolders: [] } };
  return originalLoad.call(this, id, ...args);
};
const { CognitionBackend } = require("../out/tests/cognition.js");
Module._load = originalLoad;

function setup(t, response = { accepted: true }, status = 200) {
  const backend = new CognitionBackend();
  backend.endpoint = { port: 1234, token: "secret" };
  backend.session = "parent";
  const calls = [];
  t.mock.method(globalThis, "fetch", async (url, init) => {
    calls.push({ url, ...init });
    return new Response(JSON.stringify(response), { status });
  });
  const apply = (kind, payload, seq = 1) => {
    const message = backend.mapEvent({ kind, payload, seq });
    if (message) backend.emit(message);
    return message;
  };
  const permission = (extra = {}) => apply("permission.requested", { request_id: "R", tool: "bash", agent: "build", arguments: "{}", ...extra });
  return { backend, calls, apply, permission };
}

test("child approval posts to parent with bearer token and exact deny body", async t => {
  const { backend, calls, permission } = setup(t);
  permission({ child_session_id: "child" });
  await backend.answerPermission("parent", "R", "deny", "Do not run");
  assert.equal(calls[0].url, "http://127.0.0.1:1234/v1/sessions/parent/permissions/R");
  assert.deepEqual(JSON.parse(calls[0].body), { decision: "deny", message: "Do not run" });
  assert.equal(calls[0].headers.authorization, "Bearer secret");
  assert.equal(backend.hasOpenPermissions(), true);
});

test("foreign session, unknown request, forbidden folder grant and malformed message never post", async t => {
  const { backend, calls, permission } = setup(t);
  permission();
  for (const args of [["other", "R", "allow_once"], ["parent", "foreign", "deny"],
    ["parent", "R", "allow_folder"], ["parent", "R", "unexpected"], ["parent", "R", "deny", {}]]) {
    await assert.rejects(backend.answerPermission(...args));
  }
  assert.equal(calls.length, 0);
});

test("HTTP 409 marks permission stale", async t => {
  const { backend, permission } = setup(t, { error: "No such permission", reason: "permission_stale" }, 409);
  permission();
  await backend.answerPermission("parent", "R", "allow_once");
  assert.equal(backend.hasOpenPermissions(), false);
  assert.equal(backend.history().at(-1).resolution, "stale");
});

test("duplicate permission clicks post once; late failure cannot change another conversation", async t => {
  const { backend, permission } = setup(t);
  permission();
  let finish;
  let count = 0;
  t.mock.method(globalThis, "fetch", () => {
    count++;
    return new Promise(resolve => { finish = resolve; });
  });
  const answer = backend.answerPermission("parent", "R", "allow_once");
  await backend.answerPermission("parent", "R", "deny");
  assert.equal(count, 1);
  backend.resetTranscript();
  backend.session = "other";
  finish(new Response(JSON.stringify({ error: "Old request" }), { status: 409 }));
  await answer;
  assert.equal(backend.hasOpenPermissions(), false);
  assert.equal(backend.history().some(m => m.type === "permissionState"), false);
});

test("stale or malformed optional pause tokens are not guessed", t => {
  const { apply } = setup(t);
  const pause = apply("agent.yield", { kind: "watchdog", details: { pause_token: { incarnation: "I" } } });
  assert.equal(pause.token, undefined);
});

test("permission HTTP failure leaves card open and shows refusal", async t => {
  const { backend, permission } = setup(t, { error: "Unavailable" }, 503);
  permission();
  await assert.rejects(backend.answerPermission("parent", "R", "allow_once"), /Unavailable/);
  assert.equal(backend.hasOpenPermissions(), true);
  assert.equal(backend.history().at(-1).resolution, "error");
});

test("watchdog Continue sends host-owned token with root as owner and target", async t => {
  const { backend, calls, apply } = setup(t);
  const token = { incarnation: "I", hold_id: "H" };
  const pause = apply("agent.yield", { kind: "watchdog", question: "No progress", details: { pause_token: token } });
  await backend.controlPause("parent", pause.id, "continue");
  assert.equal(calls[0].url, "http://127.0.0.1:1234/v1/sessions/parent/children/parent/continue");
  assert.deepEqual(JSON.parse(calls[0].body), { pause_token: token });
  assert.equal(backend.history().at(-1).state, "resolved");
  await assert.rejects(backend.controlPause("parent", pause.id, "continue"));
  assert.equal(calls.length, 1);
});

test("budget Continue sends null token; refusal stays visible and retryable", async t => {
  const { backend, calls, apply } = setup(t, { accepted: false });
  const pause = apply("agent.yield", { kind: "budget" });
  await assert.rejects(backend.controlPause("parent", pause.id, "continue"), /refused/);
  assert.deepEqual(JSON.parse(calls[0].body), { pause_token: null });
  assert.equal(backend.history().at(-1).state, "error");
  assert.equal(backend.controls.pauses.size, 1);
});

test("Stop refusal preserves open session and pause; accepted Stop resolves it", async t => {
  const { backend, calls, apply } = setup(t, { accepted: false, actor_state: "running" });
  const pause = apply("agent.yield", { kind: "watchdog", details: { pause_token: { incarnation: "I", hold_id: "H" } } });
  await assert.rejects(backend.controlPause("parent", pause.id, "stop"), /Stop refused: running/);
  assert.equal(backend.session, "parent");
  assert.equal(calls[0].url, "http://127.0.0.1:1234/v1/sessions/parent/stop");
  assert.equal(calls[0].body, undefined);
  t.mock.method(globalThis, "fetch", async () => new Response(JSON.stringify({ accepted: true })));
  await backend.controlPause("parent", pause.id, "stop");
  assert.equal(backend.session, undefined);
  assert.equal(backend.resumeId, "parent");
  assert.equal(backend.history().at(-1).state, "resolved");
});

test("missing watchdog token and foreign pause id never post", async t => {
  const { backend, calls, apply } = setup(t);
  const pause = apply("agent.yield", { kind: "watchdog" });
  await assert.rejects(backend.controlPause("parent", pause.id, "continue"), /pause token/);
  await assert.rejects(backend.controlPause("other", pause.id, "stop"));
  await assert.rejects(backend.controlPause("parent", "foreign", "continue"));
  assert.equal(calls.length, 0);
});

test("tool evidence comes from pre-call or post-call and survives row updates", t => {
  const { apply } = setup(t);
  const evidence = { policy_version: 1, source: "jev", reason: "Low risk" };
  assert.equal(apply("tool.pre_call", { tool_call_id: "plain", name: "read", args: {} }).evidence, undefined);
  assert.deepEqual(apply("tool.pre_call", { tool_call_id: "C", name: "bash", args: { command: "pwd" }, permission_gate: evidence }).evidence, evidence);
  assert.deepEqual(apply("tool.post_call", { tool_call_id: "C", name: "bash", status: "ok" }).evidence, evidence);
  const post = { ...evidence, source: "metacognition", verdicts: { risk: "low" }, models: ["reviewer"] };
  assert.deepEqual(apply("tool.post_call", { tool_call_id: "D", name: "write", status: "ok", metadata: { permission_gate: post } }).evidence, post);
});

test("permission uses full llm call even when server review arguments were shortened", t => {
  const { apply, permission } = setup(t);
  const command = "a".repeat(8000);
  apply("llm.response", { message: { tool_calls: [{ id: "C", args: { command } }] } });
  assert.equal(permission({ tool_call_id: "C", arguments: '{"command":"cut' }).card.call, command);
  const content = "x".repeat(6000);
  apply("llm.response", { message: { tool_calls: [{ id: "W", args: { path: "/full/path", content } }] } });
  const write = permission({ request_id: "W", tool_call_id: "W", tool: "write", arguments: "cut" });
  assert.match(write.card.call, /^\/full\/path/);
  assert.match(write.card.call, /1904 bytes omitted/);
});

test("child full call is read through host SSE without resuming and can enrich a resolved replay card", async t => {
  const { backend, calls, permission, apply } = setup(t);
  const card = permission({ child_session_id: "child", tool_call_id: "C", arguments: "cut" });
  apply("permission.resolved", { request_id: "R", resolution: "deny" });
  t.mock.method(globalThis, "fetch", async (url, init) => {
    calls.push({ url, ...init });
    return new Response(`id: 1\ndata: ${JSON.stringify({ kind: "llm.response", payload: { message: { tool_calls: [{ id: "C", args: { command: "whole child command" } }] } } })}\n\n`);
  });
  await backend.loadChildPermissionCall(card);
  assert.equal(calls[0].url, "http://127.0.0.1:1234/v1/sessions/child/events");
  assert.equal(calls[0].headers.authorization, "Bearer secret");
  assert.equal(calls[0].method, undefined);
  assert.deepEqual(backend.history().at(-1), { type: "permissionCall", requestId: "R", call: "whole child command" });
  assert.equal(backend.hasOpenPermissions(), false);
});
