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
const shortened = text => `${text.slice(0, 2000)}\n...[output elided]...\n${text.slice(-2000)}`;

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
  apply("llm.response", { message: { tool_calls: [{ id: "C", name: "bash", args: { command } }] } });
  assert.equal(permission({ tool_call_id: "C", arguments: shortened(JSON.stringify({ command })) }).card.call, command);
  const content = "x".repeat(6000);
  apply("llm.response", { message: { tool_calls: [{ id: "W", name: "write", args: { path: "/full/path", content } }] } });
  const write = permission({ request_id: "W", tool_call_id: "W", tool: "write", arguments: shortened(JSON.stringify({ content, path: "/full/path" })) });
  assert.match(write.card.call, /\/full\/path/);
  assert.match(write.card.call, /1904 bytes omitted/);
});

test("child replay correlates repeated call ids at the matching permission request", async t => {
  const { backend, calls, permission, apply } = setup(t);
  const command = "rm -rf /important";
  const args = JSON.stringify({ command });
  const card = permission({ child_session_id: "child", tool_call_id: "C", arguments: args });
  t.mock.method(globalThis, "fetch", async (url, init) => {
    calls.push({ url, ...init });
    const events = [
      { kind: "llm.response", payload: { message: { tool_calls: [{ id: "C", name: "bash", args: { command: "echo harmless" } }] } } },
      { kind: "llm.response", payload: { message: { tool_calls: [{ id: "C", name: "bash", args: { command } }] } } },
      { kind: "permission.requested", payload: { request_id: "R", tool_call_id: "C", tool: "bash", arguments: args } },
    ];
    return new Response(events.map(e => `data: ${JSON.stringify(e)}\n\n`).join(""));
  });
  await backend.loadChildPermissionCalls("child", [card.card], new AbortController());
  assert.equal(calls[0].url, "http://127.0.0.1:1234/v1/sessions/child/events");
  assert.equal(calls[0].headers.authorization, "Bearer secret");
  assert.equal(calls[0].method, undefined);
  assert.deepEqual(backend.history().at(-1), { type: "permissionCall", requestId: "R", call: command, generation: 0 });
  apply("permission.resolved", { request_id: "R", resolution: "deny" });
  assert.equal(backend.hasOpenPermissions(), false);
});

test("sequential permission answers post once until durable resolution, failures release the guard", async t => {
  const { backend, permission, apply, calls } = setup(t);
  permission();
  await backend.answerPermission("parent", "R", "allow_once");
  await backend.answerPermission("parent", "R", "deny");
  assert.equal(calls.length, 1);
  apply("permission.resolved", { request_id: "R", resolution: "allow_once" });
  assert.equal(backend.pendingControls.size, 0);
  permission({ request_id: "retry" });
  let attempts = 0;
  t.mock.method(globalThis, "fetch", async () => {
    attempts++;
    return new Response(JSON.stringify(attempts === 1 ? { error: "retry" } : { accepted: true }), { status: attempts === 1 ? 503 : 200 });
  });
  await assert.rejects(backend.answerPermission("parent", "retry", "deny"));
  await backend.answerPermission("parent", "retry", "deny");
  assert.equal(attempts, 2);
  backend.resetTranscript();
  assert.equal(backend.pendingControls.size, 0);
});

test("resolved historical child requests open no streams; open requests coalesce with concurrency capped at two", async t => {
  const { backend, permission, apply } = setup(t);
  let active = 0;
  let maximum = 0;
  let reads = 0;
  t.mock.method(globalThis, "fetch", async () => {
    reads++;
    active++;
    maximum = Math.max(maximum, active);
    return new Response(new ReadableStream({ cancel() { active--; } }));
  });
  for (let i = 0; i < 100; i++) {
    permission({ request_id: `old-${i}`, child_session_id: `old-child-${i}` });
    backend.scheduleChildRead(`old-child-${i}`);
    apply("permission.resolved", { request_id: `old-${i}`, resolution: "deny" });
  }
  backend.drainChildReads();
  assert.equal(reads, 0);
  for (let i = 0; i < 20; i++) {
    const child = `child-${Math.floor(i / 2)}`;
    permission({ request_id: `open-${i}`, child_session_id: child });
    backend.scheduleChildRead(child);
  }
  backend.drainChildReads();
  await Promise.resolve();
  assert.equal(reads, 2);
  assert.equal(maximum, 2);
  backend.dispose();
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(active, 0);
  assert.equal(backend.childReadQueue.size, 0);
});

for (const action of ["switch", "dispose"]) {
  test(`child pending reader is cancelled on ${action}`, async t => {
    const { backend, permission } = setup(t);
    permission({ child_session_id: "child" });
    let cancelled = false;
    let signal;
    t.mock.method(globalThis, "fetch", async (_url, init) => {
      signal = init.signal;
      return new Response(new ReadableStream({ cancel() { cancelled = true; } }));
    });
    backend.scheduleChildRead("child");
    backend.drainChildReads();
    await Promise.resolve();
    if (action === "switch") await backend.newConversation();
    else backend.dispose();
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(signal.aborted, true);
    assert.equal(cancelled, true);
    assert.equal(backend.childReads.size, 0);
  });
}

test("child replay byte cap cancels oversized streams without enriching", async t => {
  const { backend, permission } = setup(t);
  permission({ child_session_id: "child" });
  let cancelled = false;
  t.mock.method(globalThis, "fetch", async () => new Response(new ReadableStream({
    start(controller) { controller.enqueue(new Uint8Array(2 * 1024 * 1024 + 1)); },
    cancel() { cancelled = true; },
  })));
  backend.scheduleChildRead("child");
  backend.drainChildReads();
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(cancelled, true);
  assert.equal(backend.history().some(m => m.type === "permissionCall"), false);
  backend.dispose();
});

test("slow parent replay finishes before any historical child enrichment starts", async t => {
  const { backend, calls } = setup(t);
  backend.replayThrough = 3;
  backend.handleFrame({ id: "1", envelope: { seq: 1, kind: "permission.requested", payload: {
    request_id: "historical", tool_call_id: "C", tool: "bash", arguments: "{}", child_session_id: "child",
  } } });
  backend.drainChildReads();
  assert.equal(calls.length, 0);
  backend.handleFrame({ id: "2", envelope: { seq: 2, kind: "permission.resolved", payload: { request_id: "historical", resolution: "deny" } } });
  backend.handleFrame({ id: "3", envelope: { seq: 3, kind: "turn.end", payload: { outcome: "done" } } });
  backend.drainChildReads();
  assert.equal(calls.length, 0);
  backend.dispose();
});

test("child request mismatch or ambiguous latest response keeps the request's own shortened call", async t => {
  const { backend, permission } = setup(t);
  const reviewed = '{"command":"reviewed\n...[output elided]...\n"}';
  const card = permission({ child_session_id: "child", tool_call_id: "C", arguments: reviewed });
  for (const calls of [
    [{ id: "C", name: "bash", args: { command: "echo harmless" } }],
    [{ id: "C", name: "write", args: { command: "reviewed" } }],
    [{ id: "C", name: "bash", args: {} }, { id: "C", name: "bash", args: {} }],
  ]) {
    t.mock.method(globalThis, "fetch", async () => new Response([
      { kind: "llm.response", payload: { message: { tool_calls: calls } } },
      { kind: "permission.requested", payload: { request_id: "R", tool_call_id: "C", tool: "bash", arguments: reviewed } },
    ].map(e => `data: ${JSON.stringify(e)}\n\n`).join("")));
    await backend.loadChildPermissionCalls("child", [card.card], new AbortController());
    assert.equal(backend.history().some(m => m.type === "permissionCall"), false);
    assert.match(card.card.call, /shortened by the server/);
  }
});

test("host rejects an old generation even when session and pause ids collide", async t => {
  const { backend, calls, apply } = setup(t);
  backend.streamGeneration = 2;
  const pause = apply("agent.yield", { kind: "watchdog", details: { pause_token: { incarnation: "B", hold_id: "H" } } }, 10);
  await assert.rejects(backend.controlPause("parent", pause.id, "continue", 1), /conversation changed/);
  await assert.rejects(backend.controlPause("parent", pause.id, "stop", 1), /conversation changed/);
  assert.equal(calls.length, 0);
});

test("delayed A creation after opening B cannot install A or send its message", async t => {
  const { backend, permission } = setup(t);
  backend.session = undefined;
  backend.connect = async () => {};
  backend.listConversations = async () => [];
  let finishA;
  const requests = [];
  t.mock.method(globalThis, "fetch", async (url, init) => {
    requests.push({ url, ...init });
    if (url.endsWith("/v1/sessions") && JSON.parse(init.body).resume !== "B") return new Promise(resolve => { finishA = resolve; });
    if (url.endsWith("/v1/sessions")) return new Response(JSON.stringify({ session_id: "B" }));
    return new Response(JSON.stringify({ session_id: "B", current_seq: 0, actor_state: "idle", model: "model" }));
  });
  const pending = backend.send("A draft");
  await new Promise(resolve => setImmediate(resolve));
  await backend.openConversation("B");
  permission({ request_id: "B-request" });
  const generation = backend.currentGeneration();
  finishA(new Response(JSON.stringify({ session_id: "A" })));
  await assert.rejects(pending, /Send cancelled.*draft was kept/);
  assert.equal(backend.currentConversationId(), "B");
  assert.equal(backend.currentGeneration(), generation);
  assert.equal(backend.controls.permissions.has("B-request"), true);
  assert.equal(requests.some(r => r.url.endsWith("/messages")), false);
  assert.equal(backend.history().some(m => m.type === "conversationOpened" && m.id === "A"), false);
  backend.dispose();
});

test("resumed server replay omitting historical session.end leaves old budget resolved", async t => {
  const { backend, apply } = setup(t);
  backend.connect = async () => {};
  backend.listConversations = async () => [];
  let summaries = 0;
  t.mock.method(globalThis, "fetch", async (url) => {
    if (url.endsWith("/v1/sessions")) return new Response(JSON.stringify({ session_id: "parent" }));
    return new Response(JSON.stringify({ current_seq: summaries++ === 0 ? 12 : 13, actor_state: summaries === 1 ? "stopped" : "idle" }));
  });
  await backend.openConversation("parent");
  const messages = [];
  backend.subscribe(message => messages.push(message));
  apply("agent.yield", { kind: "budget" }, 10);
  apply("turn.end", { outcome: "yielded" }, 11);
  // Actual resumed subscriptions omit the old session.end at seq 12.
  assert.equal(backend.controls.pauses.size, 0);
  assert.equal(messages.find(m => m.type === "pauseState").message, "Ended with the previous run");
  apply("agent.yield", { kind: "budget" }, 14);
  assert.equal(backend.controls.pauses.size, 1);
  backend.dispose();
});

test("summary 503 retries then reconciles and releases child enrichment", async t => {
  const { backend, permission } = setup(t);
  backend.replayThrough = Infinity;
  backend.delivered.add(4);
  permission({ child_session_id: "child" });
  backend.scheduleChildRead("child");
  let summaries = 0;
  let childReads = 0;
  t.mock.method(globalThis, "fetch", async url => {
    if (url.endsWith("/events")) { childReads++; return new Response(""); }
    summaries++;
    return new Response(JSON.stringify(summaries === 1 ? { error: "Unavailable" } : { current_seq: 4, model: "model" }), { status: summaries === 1 ? 503 : 200 });
  });
  await backend.loadSessionModel("parent");
  assert.equal(summaries, 2);
  assert.equal(childReads, 1);
  assert.ok(backend.history().some(m => m.type === "status" && /reconcile conversation history/.test(m.message ?? "")));
  backend.dispose();
});

test("recreated view requeues interrupted child enrichment and retains completed previews", async t => {
  const { backend, permission } = setup(t);
  const args = JSON.stringify({ command: "pwd" });
  permission({ child_session_id: "child", tool_call_id: "C", arguments: args });
  let reads = 0;
  t.mock.method(globalThis, "fetch", async () => {
    reads++;
    if (reads === 1) return new Response(new ReadableStream({}));
    return new Response([
      { kind: "llm.response", payload: { message: { tool_calls: [{ id: "C", name: "bash", args: { command: "pwd" } }] } } },
      { kind: "permission.requested", payload: { request_id: "R", tool_call_id: "C", tool: "bash", arguments: args } },
    ].map(e => `data: ${JSON.stringify(e)}\n\n`).join(""));
  });
  backend.resumePermissionReads();
  backend.drainChildReads();
  await Promise.resolve();
  backend.cancelPermissionReads();
  await new Promise(resolve => setImmediate(resolve));
  backend.resumePermissionReads();
  backend.drainChildReads();
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(reads, 2);
  assert.ok(backend.history().some(m => m.type === "permissionCall" && m.call === "pwd"));
  backend.cancelPermissionReads();
  backend.resumePermissionReads();
  backend.drainChildReads();
  assert.equal(reads, 2);
  backend.dispose();
});

test("restart A with failed bootstrap then create B never relabels A controls or history", async t => {
  const { backend, permission, apply } = setup(t);
  backend.connect = async () => {};
  permission({ request_id: "RA" });
  const pause = apply("agent.yield", { kind: "watchdog", details: { pause_token: { incarnation: "A", hold_id: "H" } } }, 10);
  const calls = [];
  t.mock.method(globalThis, "fetch", async (url, init) => {
    calls.push({ url, ...init });
    if (url.includes("/v1/models")) return new Response(JSON.stringify({ models: [] }));
    if (url.includes("/v1/sessions?")) return new Response(JSON.stringify({ error: "Unavailable" }), { status: 503 });
    if (url.endsWith("/v1/sessions")) return new Response(JSON.stringify({ session_id: "B" }));
    return new Response(JSON.stringify({ accepted: true }));
  });
  backend.setServerInfo({ port: 4321, token: "new" });
  await new Promise(resolve => setImmediate(resolve));
  await backend.send("New request");
  assert.equal(backend.currentConversationId(), "B");
  assert.equal(backend.history().some(m => ["permission", "pause"].includes(m.type)), false);
  const generation = backend.currentGeneration();
  await assert.rejects(backend.answerPermission("B", "RA", "allow_once", undefined, generation));
  for (const action of ["continue", "stop"]) await assert.rejects(backend.controlPause("B", pause.id, action, generation));
  assert.equal(calls.some(c => /\/permissions\/|\/continue$|\/stop$/.test(c.url)), false);
  backend.dispose();
});

for (const status of [404, 503]) {
  test(`failed open ${status} restores A transcript, subscription and answerable permission`, async t => {
    const { backend, permission } = setup(t);
    permission({ request_id: "RA" });
    let connects = 0;
    backend.connect = async () => { connects++; };
    let requests = 0;
    t.mock.method(globalThis, "fetch", async url => {
      if (url.endsWith("/B")) { requests++; return new Response(JSON.stringify({ error: "Cannot open B" }), { status }); }
      return new Response(JSON.stringify({ accepted: true }));
    });
    await assert.rejects(backend.openConversation("B"), /Cannot open B/);
    assert.equal(backend.currentConversationId(), "parent");
    assert.equal(backend.history().some(m => m.type === "permission" && m.card.requestId === "RA"), true);
    assert.equal(connects, 1);
    assert.equal(requests, 3);
    await backend.answerPermission("parent", "RA", "allow_once", undefined, backend.currentGeneration());
    backend.dispose();
  });
}

test("server-wide catalog installs after bootstrap changes conversation generation", async t => {
  const { backend } = setup(t);
  backend.session = undefined;
  backend.connect = async () => {};
  backend.listConversations = async () => [];
  let finish;
  t.mock.method(globalThis, "fetch", async url => {
    if (url.includes("/v1/models")) return new Promise(resolve => { finish = resolve; });
    if (url.includes("/v1/sessions?")) return new Response(JSON.stringify({ sessions: [{ id: "parent" }] }));
    if (url.endsWith("/v1/sessions")) return new Response(JSON.stringify({ session_id: "parent" }));
    return new Response(JSON.stringify({ current_seq: 0, actor_state: "idle", model: "model" }));
  });
  const pending = backend.fetchModels();
  await backend.bootstrapConversations();
  assert.equal(backend.currentConversationId(), "parent");
  finish(new Response(JSON.stringify({ models: [{ alias: "cognition", model: "model" }], default: "cognition" })));
  await pending;
  assert.equal(backend.models[0].alias, "cognition");
  backend.dispose();
});

test("obsolete endpoint catalog cannot replace current models", async t => {
  const { backend } = setup(t);
  let finish;
  t.mock.method(globalThis, "fetch", async () => new Promise(resolve => { finish = resolve; }));
  const pending = backend.fetchModels();
  backend.endpoint = { port: 4321, token: "new" };
  backend.models = [{ alias: "current", model: "new" }];
  finish(new Response(JSON.stringify({ models: [{ alias: "obsolete", model: "old" }] })));
  await pending;
  assert.equal(backend.models[0].alias, "current");
  backend.dispose();
});
