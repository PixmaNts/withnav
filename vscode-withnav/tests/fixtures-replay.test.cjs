const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const Module = require("node:module");

const originalLoad = Module._load;
Module._load = function (id, ...args) {
  if (id === "vscode") return { workspace: { workspaceFolders: [] } };
  return originalLoad.call(this, id, ...args);
};
const { CognitionBackend } = require("../out/tests/cognition.js");
Module._load = originalLoad;

function loadFixture(name) {
  const filePath = path.join(__dirname, "../../tests/fixtures/events", name);
  return fs.readFileSync(filePath, "utf8");
}

function parseSseEnvelopes(raw) {
  const blocks = raw.split(/\r?\n\r?\n/);
  const envelopes = [];
  for (const block of blocks) {
    if (!block.trim()) continue;
    const data = [];
    for (const line of block.split(/\r?\n/)) {
      if (line.startsWith("data:")) {
        data.push(line.slice(5).trimStart());
      }
    }
    if (data.length > 0) {
      envelopes.push(JSON.parse(data.join("\n")));
    }
  }
  return envelopes;
}

function replayFixture(raw) {
  const backend = new CognitionBackend();
  backend.endpoint = { port: 1234, token: "secret" };
  const emitted = [];
  backend.subscribe((msg) => emitted.push(msg));

  const envelopes = parseSseEnvelopes(raw);
  if (envelopes.length > 0 && envelopes[0].session_id) {
    backend.state.session = envelopes[0].session_id;
  }

  let buffer = raw;
  let split;
  while ((split = buffer.indexOf("\n\n")) !== -1) {
    const frameRaw = buffer.slice(0, split);
    buffer = buffer.slice(split + 2);
    backend.handleFrame(backend.parseFrame(frameRaw));
  }
  if (buffer.trim()) {
    backend.handleFrame(backend.parseFrame(buffer));
  }

  return { backend, emitted, envelopes };
}

test("replays write-file fixture through CognitionBackend", () => {
  const raw = loadFixture("write-file.sse");
  const { backend, emitted } = replayFixture(raw);

  const captainMessages = emitted.filter((m) => m.type === "message" && m.author === "captain");
  assert.equal(captainMessages.length, 1);
  assert.ok(captainMessages[0].text.includes("hello.txt"));

  const tools = emitted.filter((m) => m.type === "tool" && m.phase === "end");
  assert.equal(tools.length, 2);
  assert.equal(tools[0].name, "glob");
  assert.equal(tools[0].ok, true);
  assert.equal(tools[0].summary, "glob");

  assert.equal(tools[1].name, "write");
  assert.equal(tools[1].ok, true);
  assert.equal(tools[1].summary, "write hello.txt");
  assert.ok(tools[1].evidence);

  const textReplacement = emitted.find((m) => m.type === "text");
  assert.ok(textReplacement, "must emit a replacement text message for final response");
  assert.ok(textReplacement.text.includes("hello.txt"));

  const turnEnds = emitted.filter((m) => m.type === "turnEnd");
  assert.equal(turnEnds.length, 1);
  assert.equal(turnEnds[0].outcome, "done");

  backend.dispose();
});

test("replays permission-denied fixture through CognitionBackend", () => {
  const raw = loadFixture("permission-denied.sse");
  const { backend, emitted } = replayFixture(raw);

  const permissions = emitted.filter((m) => m.type === "permission");
  assert.equal(permissions.length, 1);
  assert.equal(permissions[0].card.tool, "bash");

  const permissionResolutions = emitted.filter((m) => m.type === "permissionState");
  assert.equal(permissionResolutions.length, 1);
  assert.equal(permissionResolutions[0].resolution, "deny");

  const editTool = emitted.find((m) => m.type === "tool" && m.name === "edit" && m.phase === "end");
  assert.ok(editTool);
  assert.equal(editTool.ok, true);

  const turnEnds = emitted.filter((m) => m.type === "turnEnd");
  assert.equal(turnEnds.length, 2);
  assert.equal(turnEnds[0].outcome, "done");
  assert.equal(turnEnds[1].outcome, "done");

  backend.dispose();
});

test("replays reconnect-gap fixture through CognitionBackend", () => {
  const raw = loadFixture("reconnect-gap.sse");
  const { backend, emitted } = replayFixture(raw);

  const textReplacement = emitted.find((m) => m.type === "text");
  assert.ok(textReplacement, "reconnection gap must emit a replacement text message");
  assert.equal(textReplacement.text, "Paris is the capital of France and its largest city.");

  const turnEnds = emitted.filter((m) => m.type === "turnEnd");
  assert.equal(turnEnds.length, 1);
  assert.equal(turnEnds[0].outcome, "done");

  backend.dispose();
});

test("replays session-end-interrupted fixture through CognitionBackend", () => {
  const raw = loadFixture("session-end-interrupted.sse");
  const { backend, emitted } = replayFixture(raw);

  const toolStarts = emitted.filter((m) => m.type === "tool" && m.phase === "start");
  assert.equal(toolStarts.length, 2);

  // The completed call keeps its result; the running one settles interrupted.
  const toolEnds = emitted.filter((m) => m.type === "tool" && m.phase === "end");
  assert.equal(toolEnds.length, 2);
  const globEnd = toolEnds.find((m) => m.name === "glob");
  assert.equal(globEnd.ok, true);
  assert.equal(globEnd.interrupted, undefined);

  const interrupted = emitted.filter((m) => m.type === "tool" && m.phase === "end" && m.interrupted);
  assert.equal(interrupted.length, 1);
  assert.equal(interrupted[0].name, "bash");
  assert.equal(interrupted[0].ok, undefined);
  assert.equal(interrupted[0].id, "01M48ENDTRN000000000000000", "the settled row keeps its original turn");

  // The unclosed turn ends stopped, and the conversation reports ready.
  const turnEnds = emitted.filter((m) => m.type === "turnEnd");
  assert.equal(turnEnds.length, 1);
  assert.equal(turnEnds[0].outcome, "stopped");
  assert.equal(turnEnds[0].id, "01M48ENDTRN000000000000000");
  const statuses = emitted.filter((m) => m.type === "status");
  assert.ok(statuses.some((s) => s.state === "ready"));

  backend.dispose();
});

test("replays session-end-permission fixture: cancelled permission, settled gated call", () => {
  const raw = loadFixture("session-end-permission.sse");
  const { backend, emitted } = replayFixture(raw);

  const permission = emitted.find((m) => m.type === "permission");
  assert.ok(permission, "the gated call raises its permission card");
  assert.equal(permission.card.tool, "bash");

  const resolutions = emitted.filter((m) => m.type === "permissionState");
  assert.equal(resolutions.length, 1);
  assert.equal(resolutions[0].resolution, "cancelled");
  assert.equal(resolutions[0].requestId, permission.card.requestId);

  // The gated call never ran and never had a pre_call: the extension shows
  // no row for it (a pre-existing divergence with the terminal client, which
  // renders planned calls; out of scope here). Its permission card is what
  // the session must settle, and the completed call keeps its result.
  const completed = emitted.find((m) => m.type === "tool" && m.phase === "end" && m.name === "glob");
  assert.ok(completed);
  assert.equal(completed.ok, true);
  assert.equal(emitted.filter((m) => m.type === "tool" && m.phase === "end" && m.interrupted).length, 0);

  // The turn closed as yielded before the end: its own event settles it
  // (rendered as done), and session.end adds no second turnEnd for it.
  const turnEnds = emitted.filter((m) => m.type === "turnEnd");
  assert.equal(turnEnds.length, 1);
  assert.equal(turnEnds[0].outcome, "done");
  assert.equal(turnEnds[0].id, "01M48ENPTRN000000000000000");

  const statuses = emitted.filter((m) => m.type === "status");
  assert.ok(statuses.some((s) => s.state === "ready"));

  backend.dispose();
});

for (const name of ["session-end-interrupted.sse", "session-end-permission.sse"]) {
  test(`a second session.end settles nothing twice (${name})`, () => {
    const raw = loadFixture(name);
    const { backend, emitted } = replayFixture(raw);

    const settleCount = (messages) => ({
      tools: messages.filter((m) => m.type === "tool" && m.phase === "end").length,
      interrupted: messages.filter((m) => m.type === "tool" && m.phase === "end" && m.interrupted).length,
      resolutions: messages.filter((m) => m.type === "permissionState").length,
      stoppedTurns: messages.filter((m) => m.type === "turnEnd" && m.outcome === "stopped").length,
    });
    const before = settleCount(emitted);
    // The initial replay settled the fixture's own scenario: interrupted A has
    // one interrupted tool end, permission B has one cancelled resolution.
    if (name === "session-end-interrupted.sse") {
      assert.equal(before.interrupted, 1);
      assert.equal(before.stoppedTurns, 1);
    } else {
      assert.equal(before.resolutions, 1);
    }

    // The same frame again: the seq dedup drops it whole.
    const envelopes = parseSseEnvelopes(raw);
    const endEnvelope = envelopes.find((e) => e.kind === "session.end");
    backend.handleFrame({ id: String(endEnvelope.seq), envelope: endEnvelope });
    // A second end at a higher seq: everything it would settle is already
    // settled, so it emits no new settlement.
    const second = { ...endEnvelope, seq: endEnvelope.seq + 1, event_id: `${endEnvelope.event_id}2` };
    backend.handleFrame({ id: String(second.seq), envelope: second });

    const after = settleCount(backend.history());
    // The re-fed frame is deduped by seq and the higher-seq end settles
    // nothing new: the counts do not move.
    assert.equal(after.tools, before.tools);
    assert.equal(after.interrupted, before.interrupted);
    assert.equal(after.resolutions, before.resolutions);
    assert.equal(after.stoppedTurns, before.stoppedTurns);

    backend.dispose();
  });
}

test("session.end settles a tool under the turn that started it, not the current one", () => {
  const raw = loadFixture("session-end-interrupted.sse");
  const backend = new CognitionBackend();
  backend.endpoint = { port: 1234, token: "secret" };
  const emitted = [];
  backend.subscribe((msg) => emitted.push(msg));
  const envelopes = parseSseEnvelopes(raw);
  backend.state.session = envelopes[0].session_id;

  // Feed everything EXCEPT the session.end, then run a second turn to
  // completion: the turn pointer moves past the fixture's turn while its
  // bash call is still running.
  for (const envelope of envelopes.filter((e) => e.kind !== "session.end")) {
    backend.handleFrame({ id: String(envelope.seq), envelope });
  }
  backend.handleFrame({ id: "100", envelope: { seq: 100, kind: "turn.start", payload: { turn_id: "T2" }, session_id: backend.state.session, actor_id: "agent:main", occurred_at: "2026-10-06T16:00:05.000Z", schema_version: 1, event_id: "e100", causation_id: null, correlation_id: null } });
  backend.handleFrame({ id: "101", envelope: { seq: 101, kind: "turn.end", payload: { turn_id: "T2", outcome: "done" }, session_id: backend.state.session, actor_id: "agent:main", occurred_at: "2026-10-06T16:00:06.000Z", schema_version: 1, event_id: "e101", causation_id: null, correlation_id: null } });
  assert.equal(emitted.filter((m) => m.type === "tool" && m.phase === "end" && m.interrupted).length, 0, "T2's end settles nothing: the running call belongs to the first turn");

  // Now the isolated end arrives: the running call settles under the turn
  // that started it (already closed by pointer, not by its own event), and
  // the never-ended first turn ends stopped.
  const endEnvelope = envelopes.find((e) => e.kind === "session.end");
  backend.handleFrame({ id: String(endEnvelope.seq), envelope: endEnvelope });

  const interrupted = emitted.find((m) => m.type === "tool" && m.phase === "end" && m.interrupted);
  assert.ok(interrupted);
  assert.equal(interrupted.id, "01M48ENDTRN000000000000000", "the settled row keeps its original turn, not T2");
  const stopped = emitted.filter((m) => m.type === "turnEnd" && m.outcome === "stopped");
  assert.equal(stopped.length, 1);
  assert.equal(stopped[0].id, "01M48ENDTRN000000000000000");

  backend.dispose();
});

test("a genuine interrupted tool.post_call is not counted as failed", () => {
  const { backend, emitted } = replayFixture(loadFixture("session-end-interrupted.sse"));
  backend.handleFrame({ id: "50", envelope: { seq: 50, kind: "tool.post_call", payload: { tool_call_id: "toolu_01longtaskcall000000001", name: "bash", status: "interrupted", output: "" }, session_id: backend.state.session, actor_id: "agent:main", occurred_at: "2026-10-06T16:00:04.000Z", schema_version: 1, event_id: "e50", causation_id: null, correlation_id: null } });
  const end = emitted.find((m) => m.type === "tool" && m.phase === "end" && m.callId === "toolu_01longtaskcall000000001");
  assert.ok(end, "the genuine interrupted post_call settles the running row");
  assert.equal(end.interrupted, true);
  assert.equal(end.ok, undefined, "an interrupted call is not a failure");
  assert.equal(end.id, "01M48ENDTRN000000000000000", "keeps its original turn");
  backend.dispose();
});

test("session.end settles permissions before turns and reports ready last", () => {
  const raw = loadFixture("session-end-permission.sse");
  const { backend, emitted } = replayFixture(raw);
  const order = emitted.map((m) => {
    if (m.type === "permissionState") return "permission";
    if (m.type === "turnEnd") return "turn";
    if (m.type === "status") return m.state;
    return undefined;
  }).filter(Boolean);
  // The end's own emissions: the card settles first, the turn's own event
  // already ended it, and the end finishes by reporting ready.
  const permissionAt = order.lastIndexOf("permission");
  const readyAt = order.lastIndexOf("ready");
  assert.ok(permissionAt >= 0 && readyAt > permissionAt, "ready comes after the cancelled permission");
  backend.dispose();
});

test("session.end settles a delegated child permission card too", () => {
  const raw = loadFixture("session-end-permission.sse");
  const backend = new CognitionBackend();
  backend.endpoint = { port: 1234, token: "secret" };
  const emitted = [];
  backend.subscribe((msg) => emitted.push(msg));
  const envelopes = parseSseEnvelopes(raw);
  backend.state.session = envelopes[0].session_id;

  for (const envelope of envelopes.filter((e) => e.kind !== "session.end" && e.kind !== "permission.requested")) {
    backend.handleFrame({ id: String(envelope.seq), envelope });
  }
  // A delegated child permission rides the parent's conversation: the root's
  // teardown closes the child, so the isolated end must settle its card too.
  backend.handleFrame({ id: "20", envelope: { seq: 20, kind: "permission.requested", payload: { request_id: "child-req", tool_call_id: "child-call", tool: "bash", arguments: "{\"command\":\"ls\"}", scope: { kind: "folder", value: "/w" }, agent: "build", reason: "child work", child_session_id: "child-1" }, session_id: backend.state.session, actor_id: "agent:main", occurred_at: "2026-10-06T16:10:03.500Z", schema_version: 1, event_id: "e20", causation_id: null, correlation_id: null } });
  // The fixture's own end sits at a lower seq than the injected card: an end
  // that follows the card (a resync gap replaying the end last) is what must
  // settle it.
  backend.handleFrame({ id: "21", envelope: { seq: 21, kind: "session.end", payload: {}, session_id: backend.state.session, actor_id: "agent:main", occurred_at: "2026-10-06T16:10:05.000Z", schema_version: 1, event_id: "e21", causation_id: null, correlation_id: null } });

  const resolutions = emitted.filter((m) => m.type === "permissionState");
  assert.equal(resolutions.length, 1);
  assert.equal(resolutions[0].requestId, "child-req");
  assert.equal(resolutions[0].resolution, "cancelled");

  backend.dispose();
});

test("replays turn-error fixture through CognitionBackend", () => {
  const raw = loadFixture("turn-error.sse");
  const { backend, emitted } = replayFixture(raw);

  const turnEnds = emitted.filter((m) => m.type === "turnEnd");
  assert.equal(turnEnds.length, 1);
  assert.equal(turnEnds[0].outcome, "error");
  assert.equal(turnEnds[0].error, "Prompt exceeds maximum context length");

  const statuses = emitted.filter((m) => m.type === "status");
  assert.ok(statuses.some((s) => s.state === "error"));

  backend.dispose();
});
