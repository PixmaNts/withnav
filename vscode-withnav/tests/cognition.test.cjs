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

const commandCatalog = ["model", "variant", "agent", "compact", "context"].map(name => ({ name, description: name, args_hint: "" }));
test("a failed compact has one command result, not an extra error block", async t => {
  const { backend, apply } = setup(t, { operation_id: "C", ok: false });
  backend.commands = commandCatalog;
  const events = [];
  const original = backend.emit.bind(backend);
  backend.emit = event => { events.push(event); original(event); };
  await backend.runComposerCommand("/compact");
  apply("command.result", { operation_id: "C", name: "compact", ok: false, error: "Summary unavailable" });
  assert.equal(events.filter(event => event.type === "commandError").length, 0);
  assert.equal(events.filter(event => event.type === "compaction" && event.ok === false).length, 1);
  assert.match(backend.state.compaction.text, /^\/compact · Summary unavailable$/);
});
test('deletion counts nested children, resets current chat and silently refreshes a missing conversation', async t => {
  const { backend } = setup(t, {});
  let sessions = [{ id: 'parent' }, { id: 'child', parent_session_id: 'parent', promptable: false }, { id: 'nested', parent_session_id: 'child', promptable: false }];
  let status = 204;
  const calls = [];
  t.mock.method(globalThis, 'fetch', async (url, init) => {
    calls.push(init.method);
    if (init.method === 'DELETE') { sessions = []; return new Response(null, { status }); }
    return new Response(JSON.stringify({ sessions }));
  });
  const list = await backend.listConversations();
  assert.equal(list.length, 1);
  assert.equal(list[0].subagentCount, 2);
  await backend.deleteConversation('parent');
  assert.equal(backend.currentConversationId(), undefined);
  assert.equal(backend.history().some(item => item.type === 'message'), false);
  status = 404;
  await backend.deleteConversation('parent');
  assert.deepEqual(calls.slice(-2), ['DELETE', 'GET']);
  backend.dispose();
});
test("commands use exact body without context and unknown/no-session commands retain draft", async t => {
  const { backend, calls } = setup(t, { operation_id: "OP", ok: true });
  backend.commands = commandCatalog;
  await backend.runComposerCommand("/compact focus on tests");
  assert.deepEqual(JSON.parse(calls[0].body), { name: "compact", arguments: "focus on tests" });
  assert.match(calls[0].url, /\/parent\/commands$/);
  backend.mapEvent({seq:1,kind:"context.compacted",payload:{estimate_before:100,estimate_after:50}});
  await assert.rejects(backend.runComposerCommand("/unknown"), /Unknown command/);
  backend.state.session = undefined;
  await assert.rejects(backend.runComposerCommand("/context"), /conversation is needed/);
  assert.equal(calls.filter(c => c.method === "POST").length, 1);
});
test("deferred model resolves only its operation and agent replay updates display", async t => {
  const { backend, apply } = setup(t, { operation_id: "OP", ok: true });
  backend.commands = commandCatalog;
  backend.state.currentTurn = "T";
  await backend.runComposerCommand("/model next");
  apply("command.result", { operation_id: "OP", name: "model", ok: true, data: { accepted: true, applies: "after_turn" } });
  assert.equal(backend.state.pendingModel.name, "next");
  apply("model.changed", { operation_id: "OTHER", new: { agent: "build", source: "override", model: "other" } });
  assert.equal(backend.state.pendingModel.name, "next");
  apply("model.changed", { operation_id: "OP", new: { agent: "build", source: "override", model: "next" } });
  assert.equal(backend.state.pendingModel, undefined);
  apply("model.changed", { new: { agent: "plan", model: "gw/plan", source: "config", variant: null } }, 10);
  assert.equal(backend.state.agent, "plan");
});
test("skill sends typed content; durable refusal retains draft and is operation correlated", async t => {
  const { backend, calls, apply } = setup(t, { command_id: "SK", accepted: true });
  backend.commands = commandCatalog;
  assert.equal(await backend.runComposerCommand("/skill review check this"), false);
  assert.deepEqual(JSON.parse(calls[0].body).content, [{ type: "text", text: "check this" }, { type: "skill", name: "review" }]);
  apply("command.result", { operation_id: "OTHER", name: "message", ok: false, error: "unrelated" });
  assert.equal(backend.state.commands.has("SK"), true);
  const result = apply("command.result", { operation_id: "SK", name: "message", ok: false, error: "Skill disabled" });
  assert.equal(result.operationId, "SK");
  assert.match(result.text, /Skill disabled/);
  assert.equal(backend.history().findLast(m => m.type === "sendResult"), undefined);
  assert.equal(backend.state.commands.has("SK"), false);
});
test("stale command generation never posts", async t => {
  const { backend, calls } = setup(t);
  backend.commands = commandCatalog;
  await assert.rejects(backend.runComposerCommand("/context", -1), /conversation changed/);
  assert.equal(calls.length, 0);
});

function setup(t, response = { accepted: true }, status = 200) {
  const backend = new CognitionBackend();
  backend.endpoint = { port: 1234, token: "secret" };
  backend.state.session = "parent";
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

for (const decision of ["allow_once", "allow_always", "reject"]) {
  test(`new ${decision} answer posts to the owner with its optional message`, async t => {
    const { backend, calls, permission } = setup(t);
    permission({ child_session_id: "child", always_label: "Always (project): bash git push *" });
    await backend.answerPermission("parent", "R", decision, "Use another approach");
    assert.deepEqual(JSON.parse(calls[0].body), {
      decision, ...(decision === "reject" ? { message: "Use another approach" } : {}),
    });
  });
}

test("Always without a server label never posts", async t => {
  const { backend, calls, permission } = setup(t);
  permission({ tool: "fetch" });
  await assert.rejects(backend.answerPermission("parent", "R", "allow_always"));
  assert.equal(calls.length, 0);
});

test("sending a message percent-encodes the session id in the request path", async t => {
  const { backend, calls } = setup(t);
  backend.state.session = "a/b c";
  await backend.send("hello");
  assert.equal(calls[0].url, "http://127.0.0.1:1234/v1/sessions/a%2Fb%20c/messages");
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
  backend.state.session = "other";
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
  const { backend, permission } = setup(t, { error: "Uwithnavlable" }, 503);
  permission();
  await assert.rejects(backend.answerPermission("parent", "R", "allow_once"), /Uwithnavlable/);
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
  assert.equal(backend.state.controls.pauses.size, 1);
});

test("Stop refusal preserves open session and pause; accepted Stop resolves it", async t => {
  const { backend, calls, apply } = setup(t, { accepted: false, actor_state: "running" });
  const pause = apply("agent.yield", { kind: "watchdog", details: { pause_token: { incarnation: "I", hold_id: "H" } } });
  await assert.rejects(backend.controlPause("parent", pause.id, "stop"), /Stop refused: running/);
  assert.equal(backend.state.session, "parent");
  assert.equal(calls[0].url, "http://127.0.0.1:1234/v1/sessions/parent/stop");
  assert.equal(calls[0].body, undefined);
  t.mock.method(globalThis, "fetch", async () => new Response(JSON.stringify({ accepted: true })));
  await backend.controlPause("parent", pause.id, "stop");
  assert.equal(backend.state.session, undefined);
  assert.equal(backend.state.resumeId, "parent");
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
  const evidence = { policy_version: 1, source: "instinct", reason: "Low risk" };
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
  assert.equal(backend.state.pendingControls.size, 0);
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
  assert.equal(backend.state.pendingControls.size, 0);
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
  assert.equal(backend.state.childReadQueue.size, 0);
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
    assert.equal(backend.state.childReads.size, 0);
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
  backend.state.replayThrough = 3;
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
  backend.state.streamGeneration = 2;
  const pause = apply("agent.yield", { kind: "watchdog", details: { pause_token: { incarnation: "B", hold_id: "H" } } }, 10);
  await assert.rejects(backend.controlPause("parent", pause.id, "continue", 1), /conversation changed/);
  await assert.rejects(backend.controlPause("parent", pause.id, "stop", 1), /conversation changed/);
  assert.equal(calls.length, 0);
});

test("delayed A creation after opening B cannot install A or send its message", async t => {
  const { backend, permission } = setup(t);
  backend.state.session = undefined;
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
  assert.equal(backend.state.controls.permissions.has("B-request"), true);
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
  assert.equal(backend.state.controls.pauses.size, 0);
  assert.equal(messages.find(m => m.type === "pauseState").message, "Ended with the previous run");
  apply("agent.yield", { kind: "budget" }, 14);
  assert.equal(backend.state.controls.pauses.size, 1);
  backend.dispose();
});

test("summary 503 retries then reconciles and releases child enrichment", async t => {
  const { backend, permission } = setup(t);
  backend.state.replayThrough = Infinity;
  backend.state.delivered.add(4);
  permission({ child_session_id: "child" });
  backend.scheduleChildRead("child");
  let summaries = 0;
  let childReads = 0;
  t.mock.method(globalThis, "fetch", async url => {
    if (url.endsWith("/events")) { childReads++; return new Response(""); }
    if (url.includes('/v1/agents?')) return new Response(JSON.stringify({agents:[],default_agent:'build'}));
    if (url.includes('/v1/layers?')) return new Response(JSON.stringify({layers:[]}));
    summaries++;
    return new Response(JSON.stringify(summaries === 1 ? { error: "Uwithnavlable" } : { current_seq: 4, model: "model" }), { status: summaries === 1 ? 503 : 200 });
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
    if (url.includes("/v1/sessions?")) return new Response(JSON.stringify({ error: "Uwithnavlable" }), { status: 503 });
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
  backend.state.session = undefined;
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
  finish(new Response(JSON.stringify({ models: [{ name: "cognition", gateway: "gw", provider_group: "gw", upstream_id: "model", api: "chat", enabled: true }], default: "cognition" })));
  await pending;
  assert.equal(backend.models[0].name, "cognition");
  backend.dispose();
});

test("obsolete endpoint catalog cannot replace current models", async t => {
  const { backend } = setup(t);
  let finish;
  t.mock.method(globalThis, "fetch", async () => new Promise(resolve => { finish = resolve; }));
  const pending = backend.fetchModels();
  backend.endpoint = { port: 4321, token: "new" };
  backend.models = [{ name: "current", gateway: "gw", provider_group: "gw", upstream_id: "new", api: "chat", enabled: true }];
  finish(new Response(JSON.stringify({ models: [{ name: "obsolete", gateway: "gw", provider_group: "gw", upstream_id: "old", api: "chat", enabled: true }] })));
  await pending;
  assert.equal(backend.models[0].name, "current");
  backend.dispose();
});

for (const child of [undefined, "child"]) {
  test(`accepted ${child ? "mirrored child" : "root"} permission stays submitted across failed open`, async t => {
    const { backend, permission } = setup(t);
    backend.connect = async () => {};
    permission({ request_id: "RA", child_session_id: child });
    let posts = 0;
    t.mock.method(globalThis, "fetch", async url => {
      if (url.endsWith("/B")) return new Response(JSON.stringify({ error: "missing" }), { status: 404 });
      posts++;
      return new Response(JSON.stringify({ accepted: true }));
    });
    await backend.answerPermission("parent", "RA", "allow_once");
    await assert.rejects(backend.openConversation("B"));
    await backend.answerPermission("parent", "RA", "deny");
    assert.equal(posts, 1);
    assert.equal(backend.history().filter(m => m.type === "permissionState").at(-1).resolution, "pending");
    backend.dispose();
  });
}

for (const status of [200, 503]) {
  test(`in-flight permission ${status} settles its restored conversation operation`, async t => {
    const { backend, permission } = setup(t);
    backend.connect = async () => {};
    permission({ request_id: "RA", child_session_id: "child" });
    let finish;
    let posts = 0;
    t.mock.method(globalThis, "fetch", async url => {
      if (url.endsWith("/B")) return new Response(JSON.stringify({ error: "missing" }), { status: 404 });
      posts++;
      if (posts === 1) return new Promise(resolve => { finish = resolve; });
      return new Response(JSON.stringify({ accepted: true }));
    });
    const pending = backend.answerPermission("parent", "RA", "allow_once");
    const observed = pending.catch(error => error);
    await assert.rejects(backend.openConversation("B"));
    finish(new Response(JSON.stringify(status === 200 ? { accepted: true } : { error: "retryable" }), { status }));
    const result = await observed;
    if (status === 200) {
      assert.equal(result, undefined);
      await backend.answerPermission("parent", "RA", "deny");
      assert.equal(posts, 1);
    } else {
      assert.match(result.message, /retryable/);
      assert.equal(backend.history().at(-1).resolution, "error");
      await backend.answerPermission("parent", "RA", "deny");
      assert.equal(posts, 2);
    }
    backend.dispose();
  });
}

test("failed open preserves context state and emits restored usage with the current generation", async t => {
  const { backend, apply } = setup(t);
  backend.connect = async () => {};
  apply("turn.start", { turn_id: "T" });
  apply("llm.delta", { turn_id: "T", delta: { delta_type: "text", text: "Hello" } });
  const context = { incarnation: "I", revision: 3, estimate_total: 42 };
  backend.state.context = context;
  backend.state.contextEpoch = 7;
  backend.state.retiredIncarnations = ["old"];
  const seen = []; backend.subscribe(m => seen.push(m));
  t.mock.method(globalThis, "fetch", async () => new Response(JSON.stringify({ error: "missing" }), { status: 404 }));
  await assert.rejects(backend.openConversation("B"));
  apply("llm.response", { turn_id: "T", message: { text: "Hello" } });
  assert.equal(backend.history().filter(m => m.type === "delta").map(m => m.text).join(""), "Hello");
  assert.equal(backend.state.context, context);
  assert.equal(backend.state.contextEpoch, 7);
  assert.deepEqual(backend.state.retiredIncarnations, ["old"]);
  const usage = seen.find(m => m.type === "usage");
  assert.equal(usage.snapshot, context);
  assert.equal(usage.generation, backend.currentGeneration());
  backend.dispose();
});

test("failed open between tool calls preserves summary and approval evidence", async t => {
  const { backend, apply } = setup(t);
  backend.connect = async () => {};
  const evidence = { source: "instinct", policy_version: 1, reason: "Read only" };
  apply("turn.start", { turn_id: "T" });
  apply("tool.pre_call", { tool_call_id: "C", name: "bash", args: { command: "pwd" }, permission_gate: evidence });
  t.mock.method(globalThis, "fetch", async () => new Response(JSON.stringify({ error: "missing" }), { status: 404 }));
  await assert.rejects(backend.openConversation("B"));
  const tool = apply("tool.post_call", { tool_call_id: "C", name: "bash", status: "ok" });
  assert.equal(tool.summary, "bash: pwd");
  assert.deepEqual(tool.evidence, evidence);
  assert.equal(tool.id, "T");
  backend.dispose();
});

test("older answer completion cannot clear or fail a newer submitted operation", async t => {
  const { backend, permission, apply } = setup(t);
  permission({ request_id: "RA" });
  let finish;
  let posts = 0;
  t.mock.method(globalThis, "fetch", async () => {
    posts++;
    if (posts === 1) return new Promise(resolve => { finish = resolve; });
    return new Response(JSON.stringify({ accepted: true }));
  });
  const older = backend.answerPermission("parent", "RA", "allow_once");
  apply("permission.resolved", { request_id: "RA", resolution: "allow_once" });
  permission({ request_id: "RA" });
  await backend.answerPermission("parent", "RA", "deny");
  finish(new Response(JSON.stringify({ error: "old failure" }), { status: 503 }));
  await older;
  await backend.answerPermission("parent", "RA", "allow_once");
  assert.equal(posts, 2);
  assert.equal(backend.history().filter(m => m.type === "permissionState").at(-1).resolution, "pending");
  backend.dispose();
});
