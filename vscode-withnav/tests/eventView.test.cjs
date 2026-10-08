const { test } = require("node:test");
const assert = require("node:assert/strict");
const { eventView, permissionEvidence, permissionCall, boundedContent, permissionOutcome, ConversationControls } = require("../out/tests/eventView.js");

const envelope = (kind, payload, seq = 1) => ({ kind, payload, seq });
const request = (extra = {}) => envelope("permission.requested", {
  request_id: "R", tool_call_id: "C", tool: "bash", agent: "build",
  arguments: JSON.stringify({ command: "echo first\nprintf second" }),
  scope: { kind: "folder", value: "/workspace" }, reason: "Full reason\nSecond line", ...extra,
});

test("permission card preserves full reason, command, scope and ids", () => {
  const { card } = eventView(request());
  assert.equal(card.requestId, "R");
  assert.equal(card.callId, "C");
  assert.equal(card.agent, "build");
  assert.equal(card.reason, "Full reason\nSecond line");
  assert.equal(card.call, "echo first\nprintf second");
  assert.deepEqual(card.scope, { kind: "folder", value: "/workspace" });
  assert.equal(card.allowFolder, false);
});

for (const resolution of ["allow_once", "deny", "expired", "cancelled", "stale"]) {
  test(`permission requested → ${resolution} closes the matching card`, () => {
    const controls = new ConversationControls();
    controls.apply(eventView(request()));
    assert.equal(controls.permissions.size, 1);
    const result = resolution === "stale" ? { type: "permissionState", requestId: "R", resolution }
      : eventView(envelope("permission.resolved", { request_id: "R", resolution, message: "Do not run" }));
    controls.apply(result);
    assert.equal(controls.permissions.size, 0);
    if (resolution === "deny") assert.equal(permissionOutcome(resolution, result.message), "Denied: Do not run");
    if (resolution === "expired") assert.match(permissionOutcome(resolution), /session restarted/);
    if (resolution === "stale") assert.match(permissionOutcome(resolution), /No longer waiting/);
  });
}

test("pending and error keep permission open until durable resolution", () => {
  const controls = new ConversationControls();
  controls.apply(eventView(request()));
  for (const resolution of ["pending", "error"]) {
    controls.apply({ type: "permissionState", requestId: "R", resolution });
    assert.equal(controls.permissions.size, 1);
  }
  controls.apply(eventView(envelope("permission.resolved", { request_id: "foreign", resolution: "deny" })));
  assert.equal(controls.permissions.size, 1);
});

test("durable replay reconstructs closed and still-open child permissions on the parent", () => {
  const events = [request(), envelope("permission.resolved", { request_id: "R", resolution: "allow_once" }, 2),
    request({ request_id: "child-request", child_session_id: "child" })];
  const replay = () => {
    const controls = new ConversationControls();
    const messages = events.map(e => eventView(e));
    messages.forEach(m => controls.apply(m));
    return { messages, open: [...controls.permissions.values()] };
  };
  assert.deepEqual(replay(), replay());
  assert.equal(replay().open.length, 1);
  assert.equal(replay().open[0].childSessionId, "child");
});

  for (const tool of ["bash", "write", "edit", "read", "grep", "glob", "fetch"]) {
  test(`reusable ${tool} grant follows server offer`, () => {
    assert.equal(eventView(request({ tool })).card.allowFolder, tool === "fetch");
    assert.equal(eventView(request({ tool, agent: "plan" })).card.allowFolder, false);
  });
}

test("write and edit previews bound content independently with exact UTF-8 omissions", () => {
  const content = "é".repeat(2500);
  assert.match(boundedContent(content), /904 bytes omitted/);
  assert.equal(boundedContent("😀", 3), "\n(4 bytes omitted)");
  assert.equal(boundedContent("small"), "small");
  assert.match(permissionCall("write", JSON.stringify({ path: "/full/path", content })), /^\/full\/path\n\nContent\n/);
  const edit = permissionCall("edit", JSON.stringify({ path: "/a", old_string: content, new_string: content }));
  assert.equal(edit.match(/904 bytes omitted/g).length, 2);
  assert.match(edit, /Old\n/);
  assert.match(edit, /New\n/);
});

test("bash commands are never bounded; server-shortened arguments say unavailable", () => {
  const command = "a".repeat(9000);
  assert.equal(permissionCall("bash", JSON.stringify({ command })), command);
  assert.match(permissionCall("bash", '{"command":"cut'), /full call unavailable/);
});

for (const [source, author] of [["user:http", "captain"], ["user:cli", "captain"],
  ["user:subagent", "subagent"], ["subagent", "subagent"], ["subagent:child", "subagent"],
  ["user:automation", "subagent"], ["user", "subagent"], ["unknown", "subagent"],
  ["navigator", "navigator"], ["navigator:edit", "navigator"]]) {
  test(`source ${source} is ${author}, not guessed Captain provenance`, () => {
    const message = eventView(envelope("message.received", { source, content: "text", selected_skill: { name: "review", origin: "captain" } }));
    assert.equal(message.author, author);
    assert.equal(message.source, source);
    assert.equal(message.selectedSkill?.name, author === "captain" ? "review" : undefined);
  });
}

test("budget, watchdog and question yields stay distinct", () => {
  const token = { incarnation: "I", hold_id: "H" };
  const budget = eventView(envelope("agent.yield", { kind: "budget", question: "budget" }));
  assert.equal(budget.kind, "budget");
  assert.equal(budget.text, "Out of steps");
  const watchdog = eventView(envelope("agent.yield", { kind: "watchdog", question: "No progress", details: { pause_token: token } }));
  assert.equal(watchdog.kind, "watchdog");
  assert.equal(watchdog.text, "Paused by the watchdog: No progress");
  assert.deepEqual(watchdog.token, token);
  assert.deepEqual(eventView(envelope("agent.yield", { kind: "ask", question: "Which file?" }), "T"), { type: "question", id: "T", text: "Which file?" });
});

test("watchdog command notices close only the matching pause on replay", () => {
  const controls = new ConversationControls();
  controls.apply(eventView(envelope("agent.yield", { kind: "watchdog", question: "pause", details: { pause_token: { incarnation: "I", hold_id: "H" } } })));
  const result = envelope("command.result", { name: "watchdog", data: { resolution: "continued", message: "Continued", pause_token: { incarnation: "I", hold_id: "other" } } });
  assert.equal(controls.envelopeUpdates(result).length, 0);
  result.payload.data.pause_token.hold_id = "H";
  controls.envelopeUpdates(result).forEach(m => controls.apply(m));
  assert.equal(controls.pauses.size, 0);
  assert.deepEqual(eventView(result), { type: "notice", text: "Continued" });
});

test("yielded turn keeps pause; next turn or terminal turn closes it", () => {
  for (const ending of [envelope("turn.start", {}), envelope("turn.end", { outcome: "done" })]) {
    const controls = new ConversationControls();
    controls.apply(eventView(envelope("agent.yield", { kind: "budget" })));
    assert.equal(controls.envelopeUpdates(envelope("turn.end", { outcome: "yielded" })).length, 0);
    controls.envelopeUpdates(ending).forEach(m => controls.apply(m));
    assert.equal(controls.pauses.size, 0);
  }
});

test("evidence absent and malformed does not invent an approval", () => {
  for (const value of [undefined, null, {}, { source: "instinct" }, { source: "unknown", policy_version: 1 }]) assert.equal(permissionEvidence(value), undefined);
});

test("evidence preserves all optional review fields only when present", () => {
  for (const source of ["instinct", "jev", "metacognition", "captain", "driver"]) {
    const minimal = { policy_version: 1, source };
    assert.deepEqual(permissionEvidence(minimal), minimal);
    const full = { ...minimal, reason: "Low risk", verdicts: { effect: "write", scope: "project", risk: "low" },
      overrides: ["deterministic"], models: { instinct: "model" }, latency_ms: 25, truncated: false };
    assert.deepEqual(permissionEvidence(full), full);
    assert.deepEqual(eventView(request({ permission_gate: full })).card.evidence, full);
  }
});

test("stored legacy instinct events render with the layer name", () => {
  const { evidenceLabel } = require("../out/tests/eventView.js");
  for (const source of ["jev", "instinct"]) {
    const evidence = permissionEvidence({ source, policy_version: 1 });
    assert.equal(evidenceLabel(evidence, "tool"), "auto · instinct");
  }
});

test("open and denied Captain-review cards never claim approval", () => {
  const { evidenceLabel } = require("../out/tests/eventView.js");
  const evidence = { source: "captain", policy_version: 1 };
  assert.equal(evidenceLabel(evidence, "permission"), "Captain review required");
  assert.equal(evidenceLabel(evidence, "permission", "deny"), "Captain review required");
  assert.equal(evidenceLabel(evidence, "permission", "allow_once"), "approved · Captain");
  assert.equal(evidenceLabel(evidence, "tool"), "approved · Captain");
});

test("edit replace_all and every non-content argument remain visible and distinguish calls", () => {
  const args = { path: "/a", old_string: "before", new_string: "after", replace_all: false, custom_option: { mode: "exact" } };
  const single = permissionCall("edit", JSON.stringify(args));
  const all = permissionCall("edit", JSON.stringify({ ...args, replace_all: true }));
  assert.notEqual(single, all);
  assert.match(all, /replace_all: true — replace every occurrence/);
  assert.match(single, /replace_all: false/);
  assert.match(all, /custom_option: \{"mode":"exact"\}/);
  assert.match(permissionCall("write", JSON.stringify({ path: "/a", content: "text", mode: "create" })), /mode: "create"/);
});

test("session.end closes yielded budget pauses identically live and on replay", () => {
  const events = [envelope("agent.yield", { kind: "budget" }), envelope("turn.end", { outcome: "yielded" }, 2), envelope("session.end", {}, 3)];
  const controls = new ConversationControls();
  const history = [];
  for (const event of events) {
    for (const update of controls.envelopeUpdates(event)) { history.push(update); controls.apply(update); }
    const message = eventView(event);
    if (message) { history.push(message); controls.apply(message); }
  }
  assert.equal(controls.pauses.size, 0);
  assert.equal(history.at(-1).type, "pauseState");
  const replay = new ConversationControls();
  history.forEach(message => replay.apply(message));
  assert.equal(replay.pauses.size, 0);
});

test("ambiguous, wrong-name or inconsistent repeated calls never replace request arguments", () => {
  const { PermissionCallReplay } = require("../out/tests/eventView.js");
  const replay = new PermissionCallReplay();
  const request = { tool_call_id: "C", tool: "bash", arguments: JSON.stringify({ command: "dangerous" }) };
  const call = { id: "C", name: "bash", args: { command: "dangerous" } };
  for (const calls of [[call, call], [{ ...call, name: "write" }], [{ ...call, args: { command: "harmless" } }]]) {
    replay.observe(envelope("llm.response", { message: { tool_calls: calls } }));
    assert.equal(replay.argumentsFor(request), undefined);
  }
  replay.observe(envelope("llm.response", { message: { tool_calls: [call] } }));
  assert.equal(replay.argumentsFor(request), request.arguments);
  assert.equal(replay.argumentsFor({ ...request, arguments: "unrelated shortened text" }), undefined);
});

test("Captain and Nav provenance retain human and wake rendering for new sessions", () => {
  const { evidenceLabel } = require("../out/tests/eventView.js");
  for (const source of ["driver", "captain"]) {
    assert.equal(eventView(envelope("message.received", { source, content: "hello" })).author, "captain");
    assert.equal(evidenceLabel({ source, policy_version: 1 }, "tool"), "approved · Captain");
  }
  assert.equal(eventView(envelope("message.received", { source: "nav", content: "wake" })).author, "navigator");
});
